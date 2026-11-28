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

mod seed_fallback;
use seed_fallback::SeedFallback;

use subc_client_rs::{async_trait, HandlerOutcome, ModuleHandler, RequestCtx};
use subc_protocol::session::HealthReport;
use subc_protocol::ModuleHelloAckBody;

use fusiform_store::CatalogStore;

use fusiform_module::curated_models;
use fusiform_module::enrollment;
use fusiform_module::fetch::{Fetcher, SourceEndpoint};
use fusiform_module::health;
use fusiform_module::loop_::{tick, PollContext, POLL_INTERVAL_MS};
use fusiform_module::plan_prices;
use fusiform_module::seed;
use fusiform_module::serve_request;
use fusiform_module::settings::{Settings, Source};
use fusiform_module::signals::Signals;
use fusiform_module::store_poll::StorePoller;

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

    // `--manifest` also answers before any connection argument is required:
    // deployment tooling runs it on a staged binary, with no daemon running,
    // to read which commit the build was made from before installing it.
    if std::env::args().nth(1).as_deref() == Some("--manifest") {
        println!("{}", fusiform_module::manifest_json());
        return Ok(());
    }

    let handler = Fusiform::with_settings(Settings::load()?);
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
    fallback: Arc<Mutex<Option<Arc<SeedFallback>>>>,
    settings: Settings,
    #[cfg(test)]
    after_store_check: Option<fn(&CatalogStore)>,
}

impl Fusiform {
    fn with_settings(settings: Settings) -> Self {
        let signals = match &settings.source {
            Source::ModelsDev => Signals::new(),
            Source::Store { url, .. } => Signals::for_store(url.clone()),
        };
        Self {
            signals: Arc::new(signals),
            store: Mutex::new(None),
            fallback: Arc::new(Mutex::new(None)),
            settings,
            #[cfg(test)]
            after_store_check: None,
        }
    }
}

#[async_trait]
impl ModuleHandler for Fusiform {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        // Clone the handle rather than holding the mutex across the off-thread
        // read and render. The lock guards the Option, not the store: holding it
        // while awaiting a multi-megabyte render would serialize every concurrent
        // request behind the slowest one, and the store has its own connection
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

        let fallback = self
            .fallback
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(fallback) = fallback {
            if let Some(tool) = seed_fallback::mutating_tool(&body) {
                return HandlerOutcome::Error { code: "refused".into(), message: format!("{tool} is unavailable on the built-in snapshot; wait for a verified store release") };
            }
            return fusiform_module::serve_catalog_request(
                Arc::clone(&fallback.store),
                Arc::clone(&self.signals),
                body,
                true,
            )
            .await;
        }
        if store.is_store_mode() && self.signals.store_poll_error().is_some() {
            if let Some(tool) = seed_fallback::mutating_tool(&body) {
                if !matches!(store.latest_store_release(), Ok(Some(_))) {
                    return HandlerOutcome::Error {
                        code: "refused".into(),
                        message: format!("{tool} requires a verified store release"),
                    };
                }
            }
        }
        serve_request(store, Arc::clone(&self.signals), body).await
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

        let open_result = match &self.settings.source {
            Source::ModelsDev => CatalogStore::open(&descriptor),
            Source::Store { .. } => CatalogStore::open_for_store(&descriptor),
        };
        let store = match open_result {
            Ok(store) => Arc::new(store),
            Err(e) => {
                eprintln!("fusiform: store did not open: {e}");
                self.signals
                    .store_startup_status(format!("store did not open: {e}"));
                return;
            }
        };

        // Inspect the previous process's pages before seed or curated writes.
        // A read-pool connection avoids the poll writer, and the blocking pool
        // keeps the SQLite scan off the async runtime's reader thread.
        let check_store = Arc::clone(&store);
        let checked_at_ms = now_ms();
        let started = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || check_store.quick_check())
            .await
            .unwrap_or_else(|error| format!("quick_check task did not complete: {error}"));
        let duration_ms = started.elapsed().as_millis() as u64;
        eprintln!("fusiform: store quick_check: {result:?} ({duration_ms} ms)");
        self.signals
            .record_store_quick_check(result, checked_at_ms, duration_ms);
        #[cfg(test)]
        if let Some(after_store_check) = self.after_store_check {
            after_store_check(&store);
        }

        // Enroll the store with engram, the fleet's backup owner, by writing its
        // descriptor beside the database. Placed by the path the store actually
        // opened, so the descriptor's relative `store.db` names this database.
        // A failure is logged and makes health Degraded but does not stop the
        // module: a backup gap is not a reason to stop answering reads.
        enrollment::write_beside_store(store.path(), &self.signals);

        if let Source::Store { url, keys } = &self.settings.source {
            let poller =
                fusiform_module::settings::validate_store(url, keys).and_then(|(url, keys)| {
                    StorePoller::new(url, keys).map_err(|error| error.to_string())
                });
            let bootstrap = match store.latest_store_release() {
                Ok(Some(cursor)) => {
                    self.signals.record_store_applied_release(cursor.release);
                    false
                }
                Ok(None) => true,
                Err(error) => {
                    self.signals.store_startup_status(error.to_string());
                    return;
                }
            };
            self.signals
                .adopt_from_store(&store, fusiform_core::SourceId::ModelsDev);
            let floor = (store.rates_version() as i64).saturating_add(1);
            let fallback = match SeedFallback::prepare(&descriptor, bootstrap, floor) {
                Ok(fallback) => fallback,
                Err(error) => {
                    eprintln!("fusiform: built-in snapshot fallback unavailable: {error}");
                    None
                }
            };
            // Once a release replaces the seed fallback, the selected store
            // must not offer a cursor at or below one already served by the seed.
            let startup_at = fallback.as_ref().map_or_else(now_ms, |seed| {
                now_ms().max((seed.store.rates_version() as i64).saturating_add(1))
            });
            advance_versions_at_start(&store, startup_at);
            *self.fallback.lock().unwrap_or_else(|p| p.into_inner()) = fallback.map(Arc::new);
            *self.store.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&store));
            self.signals.store_opened();
            if !bootstrap {
                tokio::spawn(fusiform_module::catalog_bus::run_publisher(
                    Arc::clone(&store),
                    Arc::clone(&self.signals),
                    fusiform_module::catalog_bus::CatalogBus::default(),
                ));
            }
            match poller {
                Ok(poller) => {
                    let fallback = Arc::clone(&self.fallback);
                    let publisher =
                        bootstrap.then(|| (Arc::clone(&store), Arc::clone(&self.signals)));
                    let uses_seed = fallback.lock().unwrap_or_else(|p| p.into_inner()).is_some();
                    self.signals.store_startup_status(
                        if uses_seed {
                            seed_fallback::OFFLINE_REASON
                        } else {
                            "waiting for verified store head"
                        }
                        .into(),
                    );
                    tokio::spawn(poller.run(
                        store,
                        Arc::clone(&self.signals),
                        move || {
                            fallback.lock().unwrap_or_else(|p| p.into_inner()).take();
                            if let Some((store, signals)) = publisher {
                                tokio::spawn(fusiform_module::catalog_bus::run_publisher(
                                    store,
                                    signals,
                                    fusiform_module::catalog_bus::CatalogBus::default(),
                                ));
                            }
                        },
                        uses_seed,
                    ));
                }
                Err(error) => self.signals.store_startup_status(if bootstrap {
                    format!("{}: {error}", seed_fallback::OFFLINE_REASON)
                } else {
                    error
                }),
            }
            return;
        }

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
                // Not fatal, and the message says what that means rather than
                // leaving an operator to work it out.
                //
                // A module that cannot seed still polls, and the first fetch
                // populates the store — but until that lands it serves an empty
                // catalog, and an empty catalog answers reads successfully. A
                // consumer cannot tell it from an upstream that describes
                // nothing, which is why this has to be loud AND has to say what
                // the next thirty minutes look like.
                eprintln!(
                    "fusiform: could not seed the store, so the catalog is EMPTY and \
                     will answer reads with zero models until the first poll \
                     succeeds: {e}"
                );
            }
        }

        // Apply the curated plan prices on EVERY boot, not only a fresh store.
        //
        // Unlike the seed, this file ships with the binary and changes with it:
        // a release carrying a corrected price must reach the store, and gating
        // on emptiness would mean the correction only ever landed on a machine
        // that had never run fusiform. `append_plan_prices` diffs on the claim,
        // so re-applying an unchanged file writes nothing.
        //
        // Failure is not fatal for the same reason the seed's is not — the
        // catalog is a separate plane and serves fine without this — but the
        // message says which plane is affected, because "plan prices" means
        // nothing to an operator holding a model-catalog incident.
        let plan_rows = plan_prices::store_rows(plan_prices::rows());
        match store.append_plan_prices(&plan_rows) {
            Ok(0) => {}
            Ok(n) => eprintln!("fusiform: recorded {n} curated plan price change(s)"),
            Err(e) => eprintln!(
                "fusiform: could not apply curated plan prices, so subscription \
                 pricing reads will serve whatever the store already held — the \
                 model catalog is unaffected: {e}"
            ),
        }

        // Record the curated model rows on EVERY boot, for the same reason as
        // the plan prices: the file ships with the binary, and a release that
        // changes a curated price must reach the store, or a point-in-time read
        // after it would serve the old price. A change is dated at this boot,
        // the instant this build began serving it; an unchanged file writes
        // nothing.
        //
        // Not fatal: current reads serve curated rows from the compiled file
        // and are unaffected. Only point-in-time reads of curated rows are,
        // and the message says so.
        match curated_models::apply_to_store(&store, curated_models::table(), now_ms()) {
            Ok(0) => {}
            Ok(n) => eprintln!("fusiform: recorded {n} curated model row change(s)"),
            Err(e) => eprintln!(
                "fusiform: could not record the curated model rows, so a \
                 point-in-time read of a curated row serves what the store \
                 already held; current reads are unaffected: {e}"
            ),
        }

        record_fact_overrides_at_start(&store, now_ms());
        record_cache_lifetimes_at_start(&store, now_ms());

        advance_versions_at_start(&store, now_ms());

        // Adopt the catalog's real age before the loop starts. The staleness
        // signal lives in an atomic, so a restart empties it — and a module
        // whose upstream has been unreachable for hours would report healthy
        // the moment it restarts, which is precisely when an operator is
        // looking. Read once here on the startup path; the health path itself
        // still reads only in-memory signals.
        // Adopt what the store knows before the loop starts. History signals
        // are atomics, so a restart empties them and a module with real history
        // would report as a fresh install — healthy, never written, nothing
        // observed — at exactly the moment an operator is looking.
        //
        // The logic lives in `Signals` rather than here so a test can drive it.
        self.signals
            .adopt_from_store(&store, fusiform_core::SourceId::ModelsDev);

        *self.store.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&store));
        self.signals.store_opened();

        tokio::spawn(fusiform_module::catalog_bus::run_publisher(
            Arc::clone(&store),
            Arc::clone(&self.signals),
            fusiform_module::catalog_bus::CatalogBus::default(),
        ));

        // The poll loop starts only now, so no tick can run without a store.
        let signals = Arc::clone(&self.signals);
        tokio::spawn(async move {
            run_poll_loop(store, signals).await;
        });
    }

    async fn health(&self) -> HealthReport {
        // Reads in-memory signals. No blocking lock, no disk, no subprocess:
        // a health reply that queues behind a degraded resource is useless
        // exactly when it is needed.
        health::report(&self.signals, now_ms())
    }
}

/// Keep serving if history cannot be written: current reads still use the
/// compiled file, while historical reads retain the last recorded claims.
fn record_fact_overrides_at_start(store: &CatalogStore, boot_at_ms: i64) {
    match fusiform_module::fact_overrides::apply_to_store(
        store,
        fusiform_module::fact_overrides::overrides(),
        boot_at_ms,
    ) {
        Ok(0) => {}
        Ok(n) => eprintln!("fusiform: recorded {n} fact override change(s)"),
        Err(e) => eprintln!(
            "fusiform: could not record fact overrides, so point-in-time reads \
             serve what the store already held; current reads are unaffected: {e}"
        ),
    }
}

/// Current reads remain available if recording fails; dated reads keep the
/// last successfully recorded rules rather than silently using today's file.
fn record_cache_lifetimes_at_start(store: &CatalogStore, boot_at_ms: i64) {
    let rows =
        fusiform_module::cache_lifetimes::stored_rules(fusiform_module::cache_lifetimes::rules());
    match store.append_cache_lifetimes(&rows, boot_at_ms) {
        Ok(0) => {}
        Ok(n) => eprintln!("fusiform: recorded {n} cache lifetime rule change(s)"),
        Err(e) => eprintln!(
            "fusiform: could not record cache lifetime rules, so point-in-time reads \
             serve what the store already held; current reads are unaffected: {e}"
        ),
    }
}

fn advance_versions_at_start(store: &CatalogStore, at_ms: i64) {
    if let Err(e) = store.advance_catalog_version_at_start(fusiform_core::Timestamp(at_ms)) {
        eprintln!(
            "fusiform: could not advance the catalog and rates versions at start, so \
             readers that refetch on a version change may keep their previous copy: {e}"
        );
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

    let ctx = PollContext::new(store, fetcher, SourceEndpoint::models_dev(), signals);

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
                // The composition rather than the row count alone. Measured
                // over 11 hours of live polling, 72% of era churn was models
                // arriving and leaving — an arriving model writes one era per
                // fact it has — so "45 eras written" reads as 45 facts moving
                // when it was 8 facts and 5 arrivals.
                let c = report.composition;
                if report.eras_written == 0 {
                    eprintln!("fusiform: poll {:?}", report.outcome);
                } else {
                    eprintln!(
                        "fusiform: poll {:?}, {} eras ({} facts changed, \
                         {} models arrived, {} withdrawn)",
                        report.outcome,
                        report.eras_written,
                        c.facts_changed,
                        c.models_arrived,
                        c.models_withdrawn
                    );
                }
            }
            Err(e) => {
                // A failed fetch is a normal outcome unless the store also
                // fails to record it. In that case the error names both causes.
                eprintln!("fusiform: poll could not complete: {e}");
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

#[cfg(test)]
mod fact_override_boot_tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};

    #[test]
    fn fact_override_boot_write_failure_is_nonfatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let store = CatalogStore::open(&StorageDescriptor {
            module_id: "fusiform".into(),
            storage_namespace: "default".into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: path.to_string_lossy().into(),
            },
        })
        .unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER refuse_override BEFORE INSERT ON fact_override_era \
            BEGIN SELECT RAISE(ABORT, 'override refused'); END;",
        )
        .unwrap();
        // Prove the trigger reached the actual write, rather than testing an
        // empty diff that could not fail in the first place.
        assert!(fusiform_module::fact_overrides::apply_to_store(
            &store,
            fusiform_module::fact_overrides::overrides(),
            1_800_000_000_000,
        )
        .is_err());
        record_fact_overrides_at_start(&store, 1_800_000_000_000);
        assert!(store.fact_overrides_at(i64::MAX, None).unwrap().is_empty());
        store
            .advance_catalog_version_at_start(fusiform_core::Timestamp(1_800_000_000_000))
            .unwrap();
        assert_eq!(store.catalog_version().unwrap(), 1_800_000_000_000);
    }
}

#[cfg(test)]
mod cache_lifetime_boot_tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};

    #[test]
    fn cache_lifetime_boot_noop_and_write_failure_leave_history_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let store = CatalogStore::open(&StorageDescriptor {
            module_id: "fusiform".into(),
            storage_namespace: "default".into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: path.to_string_lossy().into(),
            },
        })
        .unwrap();
        let boot = 1_800_000_000_000;
        let rows = fusiform_module::cache_lifetimes::stored_rules(
            fusiform_module::cache_lifetimes::rules(),
        );
        // A new store has the schema migration's original rule snapshot. Record
        // today's rules first so the refusal trigger tests an unchanged boot.
        assert_eq!(store.append_cache_lifetimes(&rows, boot).unwrap(), 2);
        let original = store.cache_lifetimes_at(boot, None).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TRIGGER refuse_lifetime BEFORE INSERT ON cache_lifetime_era BEGIN SELECT RAISE(ABORT, 'lifetime refused'); END;").unwrap();
        assert_eq!(store.append_cache_lifetimes(&rows, boot).unwrap(), 0);
        record_cache_lifetimes_at_start(&store, boot);
        assert_eq!(store.cache_lifetimes_at(boot, None).unwrap(), original);
        conn.execute_batch("DROP TRIGGER refuse_lifetime;").unwrap();
        store.append_cache_lifetimes(&[], boot).unwrap();
        conn.execute_batch("CREATE TRIGGER refuse_lifetime BEFORE INSERT ON cache_lifetime_era BEGIN SELECT RAISE(ABORT, 'lifetime refused'); END;").unwrap();
        // Reappearing claims would write, so the trigger exercises a real failure.
        assert!(store.append_cache_lifetimes(&rows, boot + 1).is_err());
        record_cache_lifetimes_at_start(&store, boot + 1);
        assert!(store
            .cache_lifetimes_at(boot + 1, None)
            .unwrap()
            .iter()
            .all(|r| r.rule.is_none()));
    }
}

#[cfg(test)]
mod store_boot_tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
    use subc_protocol::session::HealthStatus;

    #[tokio::test]
    async fn store_cursor_read_failure_before_install_is_failing() {
        let dir = tempfile::tempdir().unwrap();
        let mut module = Fusiform::with_settings(Settings {
            source: Source::Store {
                url: "https://example.test/".into(),
                keys: Vec::new(),
            },
        });
        module.after_store_check = Some(|store| {
            // Opening also reads the cursor. Fail only the later startup read.
            rusqlite::Connection::open(store.path())
                .unwrap()
                .execute("DROP TABLE store_release", [])
                .unwrap();
        });
        let descriptor = StorageDescriptor {
            module_id: "fusiform".into(),
            storage_namespace: "default".into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.path().join("store.db").to_string_lossy().into_owned(),
            },
        };
        module
            .on_hello_ack(&ModuleHelloAckBody {
                negotiated_ver: 1,
                subc_ops: Vec::new(),
                subc_capabilities: Vec::new(),
                storage: Some(serde_json::to_value(descriptor).unwrap()),
                machine_id: None,
                resume_token: None,
            })
            .await;

        assert!(module.store.lock().unwrap().is_none());
        let report = fusiform_module::health::report(&module.signals, now_ms());
        assert_eq!(report.status, HealthStatus::Failing, "{report:?}");
        assert!(report.detail.unwrap().contains("store_release"));
    }
}
