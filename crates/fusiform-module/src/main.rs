#![forbid(unsafe_code)]

//! The ck-fusiform daemon: fetch upstream catalogs on a cadence, keep their
//! history, and answer questions about it.
//!
//! The module owns its domain and nothing else. HELLO, authentication, route
//! binding, reconnect and control dispatch all come from `subc-client-rs`, and
//! storage comes from `cortexkit-store`; re-deriving any of that by hand is how
//! a module ends up subtly wrong at handshake.
//!
//! # Startup order, and why it is this way
//!
//! The store is opened AFTER the daemon connection, using the storage
//! descriptor that arrives in HELLO_ACK. Keying storage before connecting means
//! the module's idea of its own path can differ from the supervisor's, and the
//! symptom is two databases where one is silently ignored.
//!
//! The poll loop starts only once the store is open, so there is no window in
//! which a tick can find no store.

use std::sync::{Arc, Mutex};

use subc_client_rs::{async_trait, HandlerOutcome, ModuleHandler, RequestCtx};
use subc_protocol::session::HealthReport;
use subc_protocol::ModuleHelloAckBody;

use fusiform_store::CatalogStore;

use fusiform_module::fetch::{Fetcher, SourceEndpoint};
use fusiform_module::health;
use fusiform_module::loop_::{tick, PollContext, POLL_INTERVAL_MS};
use fusiform_module::route;
use fusiform_module::seed;
use fusiform_module::signals::Signals;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Answered BEFORE any argument validation, and before the subc connection
    // arg is required.
    //
    // A version probe runs a bare binary with no daemon context — that is
    // exactly when a deploy ladder and an incident responder ask it. Gating it
    // behind a config argument makes it fail in the only situation it exists
    // for: SUBC's placement ladder invoked `--version` on this binary and got
    // `MissingSubcArg` with exit 1, and fell back to timing an argument
    // refusal.
    //
    // Fleet convention, from CKCRED via SUBC: name, version, and build rev
    // before any config gate.
    if std::env::args()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
    {
        println!(
            "{}",
            fusiform_protocol::version_line("ck-fusiform", env!("CARGO_PKG_VERSION"))
        );
        return Ok(());
    }

    let handler = Fusiform::new();
    subc_client_rs::serve(fusiform_module::manifest(), handler).await?;
    Ok(())
}

struct Fusiform {
    signals: Arc<Signals>,
    /// Set once, when HELLO_ACK delivers the storage descriptor.
    ///
    /// A mutex rather than a channel because the health path reads whether the
    /// store exists and must never block: it checks an atomic in `Signals`, not
    /// this. Nothing on the health path touches this lock.
    store: Mutex<Option<Arc<CatalogStore>>>,
}

impl Fusiform {
    fn new() -> Self {
        Self {
            signals: Arc::new(Signals::new()),
            store: Mutex::new(None),
        }
    }
}

#[async_trait]
impl ModuleHandler for Fusiform {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        // Clone the handle rather than holding the mutex across the read. The
        // lock guards the Option, not the store: holding it for the duration of
        // a multi-megabyte render would serialize every concurrent request
        // behind the slowest one, and the store has its own connection
        // serialization.
        let store = {
            let guard = self.store.lock().unwrap_or_else(|p| p.into_inner());
            guard.as_ref().map(Arc::clone)
        };

        let Some(store) = store else {
            // Connected, but the store never opened. An explicit refusal rather
            // than an empty catalog: a consumer receiving zero models cannot
            // tell "the upstream describes nothing" from "this module cannot
            // read", and would cache the first as if it were data.
            return HandlerOutcome::Error {
                code: "unavailable".to_string(),
                message: "fusiform has no store open; no catalog can be read".to_string(),
            };
        };

        match route::serve_tool_call(&store, &body) {
            Ok(response) => match serde_json::to_vec(&response) {
                Ok(bytes) => HandlerOutcome::Response(bytes),
                Err(e) => HandlerOutcome::Error {
                    code: "internal".to_string(),
                    message: format!("catalog response did not serialize: {e}"),
                },
            },
            Err(e) => HandlerOutcome::Error {
                code: e.code.to_string(),
                message: e.message,
            },
        }
    }

    async fn on_hello_ack(&self, ack: &ModuleHelloAckBody) {
        let Some(descriptor_json) = ack.storage.as_ref() else {
            // No managed storage configured. The module stays up and reports
            // Failing, because a fusiform that cannot persist history is
            // running but useless, and that is exactly what the health status
            // is for.
            eprintln!("fusiform: HELLO_ACK carried no storage descriptor; no history can be kept");
            return;
        };

        let descriptor: cortexkit_store_types::StorageDescriptor =
            match serde_json::from_value(descriptor_json.clone()) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("fusiform: storage descriptor did not deserialize: {e}");
                    return;
                }
            };

        let store = match CatalogStore::open(&descriptor) {
            Ok(store) => Arc::new(store),
            Err(e) => {
                eprintln!("fusiform: store did not open: {e}");
                return;
            }
        };

        self.signals.store_opened();

        // Seed an empty store before anything reads it. A fresh install with no
        // network must be able to answer, and an empty catalog is a wrong
        // answer that looks like a legitimate one: a consumer receiving zero
        // models cannot tell it from "the upstream describes nothing".
        //
        // Before the signal priming below, so the adopted instant is the
        // snapshot's fetch time rather than nothing.
        match seed::seed_if_empty(&store) {
            Ok(seed::SeedOutcome::Seeded {
                eras_written,
                model_count,
                fetched_at,
                version,
            }) => eprintln!(
                "fusiform: seeded {model_count} models ({eras_written} eras) from the \
                 embedded snapshot fetched at {}, catalog version {version}",
                fetched_at.0
            ),
            Ok(seed::SeedOutcome::AlreadyPopulated { existing_eras }) => {
                eprintln!("fusiform: store already holds {existing_eras} eras; seed not applied")
            }
            Err(e) => {
                // Not fatal. A module that cannot seed can still poll, and the
                // first fetch will populate the store — but until then it
                // serves nothing, so this must be loud rather than swallowed.
                eprintln!("fusiform: could not seed the store: {e}");
            }
        }

        // Adopt the catalog's real age before the loop starts. The staleness
        // signal lives in an atomic, so a restart empties it — and a module
        // whose upstream has been unreachable for hours would report healthy
        // the moment it restarts, which is precisely when an operator is
        // looking. Read once here on the startup path; the health path itself
        // still touches nothing but atomics.
        match store.last_confirming_observation(fusiform_core::SourceId::ModelsDev) {
            Ok(Some(at)) => self.signals.adopt_last_observation(at.0),
            Ok(None) => {
                // A genuinely fresh install. "No observation yet" is the true
                // answer and health treats it as Ok rather than stale.
            }
            Err(e) => {
                // The store opened but cannot be read. Not fatal — the loop
                // will try again — but it must not pass as a fresh install, so
                // it is logged rather than swallowed.
                eprintln!("fusiform: could not read last observation at startup: {e}");
            }
        }

        *self.store.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&store));

        // The poll loop starts only now, so no tick can run without a store.
        let signals = Arc::clone(&self.signals);
        tokio::spawn(async move {
            run_poll_loop(store, signals).await;
        });
    }

    async fn health(&self) -> HealthReport {
        // Reads atomics and does arithmetic. No lock, no disk, no subprocess:
        // a health reply that queues behind a degraded resource is useless
        // exactly when it is needed.
        health::report(&self.signals, now_ms())
    }
}

/// Poll forever at the fixed cadence.
///
/// The first tick runs immediately rather than after one interval. A module
/// that starts with an empty store and waits thirty minutes before its first
/// fetch is thirty minutes of answering from nothing, and the seed exists
/// precisely so that window is short.
async fn run_poll_loop(store: Arc<CatalogStore>, signals: Arc<Signals>) {
    let fetcher = match Fetcher::new() {
        Ok(f) => f,
        Err(e) => {
            // Without a fetcher there is nothing to run. The loop exits and the
            // heartbeat stops advancing, which health reports as degraded — the
            // honest outcome, since reads still work against stored history.
            eprintln!("fusiform: HTTP client did not build: {e}");
            return;
        }
    };

    let ctx = PollContext {
        store,
        fetcher,
        endpoint: SourceEndpoint::models_dev(),
        signals,
    };

    let mut interval =
        tokio::time::interval(std::time::Duration::from_millis(POLL_INTERVAL_MS as u64));
    // Skip missed ticks rather than firing them back to back. After a suspend
    // or a long stall, bursting the backlog would hammer the upstream to learn
    // one thing: what it says now.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        // `tick` stamps the attempt itself, at the top, so this loop cannot
        // forget to and a test calling tick directly exercises it.
        match tick(&ctx, now_ms()).await {
            Ok(report) => {
                eprintln!(
                    "fusiform: poll {:?}, {} eras written",
                    report.outcome, report.eras_written
                );
            }
            Err(e) => {
                // A store error, not a fetch error — fetch failures are an
                // outcome, not an error. The loop continues: a transient write
                // failure must not end polling, and a persistent one shows up
                // in health as a heartbeat that advances while nothing is
                // written.
                eprintln!("fusiform: tick failed: {e}");
            }
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
