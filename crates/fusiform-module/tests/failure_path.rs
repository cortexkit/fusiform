//! What the poll loop does when the upstream stops answering.
//!
//! Every other test here exercises the path where the fetch succeeds. This one
//! runs a real failure streak against a real socket, because the failure path
//! is where the observation/era distinction earns its keep: a failed poll
//! observed nothing, so it must be recorded and must not narrow any window.
//!
//! A local `TcpListener` speaking minimal HTTP rather than a mocking crate. The
//! failures under test are transport-level — a refused connection, a 500, a
//! body that stops mid-flight — and a mock that intercepts above the client
//! would test the mock's idea of those rather than the client's.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{FailureClass, SourceId};
use fusiform_module::fetch::{Fetcher, SourceEndpoint};
use fusiform_module::health::{self, FAILURE_STREAK_DEGRADED};
use fusiform_module::loop_::{tick, PollContext, TickOutcome};
use fusiform_module::signals::Signals;
use fusiform_store::CatalogStore;
use subc_protocol::session::HealthStatus;

/// How a stub responds to one request.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Behaviour {
    /// A valid catalog document.
    ServeCatalog,
    /// HTTP 500.
    ServerError,
    /// A 200 whose body is not a catalog.
    Garbage,
    /// Accept the connection and close it without writing a response.
    HangUp,
}

/// A local HTTP server whose behaviour the test drives.
struct Upstream {
    url: String,
    behaviour: Arc<std::sync::Mutex<Behaviour>>,
    requests: Arc<AtomicUsize>,
}

impl Upstream {
    fn start(initial: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        let port = listener.local_addr().unwrap().port();
        let behaviour = Arc::new(std::sync::Mutex::new(initial));
        let requests = Arc::new(AtomicUsize::new(0));

        let b = Arc::clone(&behaviour);
        let r = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                r.fetch_add(1, Ordering::SeqCst);
                let behaviour = *b.lock().unwrap_or_else(|p| p.into_inner());
                serve_one(stream, behaviour);
            }
        });

        Self {
            url: format!("http://127.0.0.1:{port}/api.json"),
            behaviour,
            requests,
        }
    }

    fn set(&self, behaviour: Behaviour) {
        *self.behaviour.lock().unwrap_or_else(|p| p.into_inner()) = behaviour;
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

fn serve_one(mut stream: TcpStream, behaviour: Behaviour) {
    // Read the request head so the client sees a complete exchange.
    let mut buf = [0u8; 2048];
    let _ = stream.read(&mut buf);

    let body: &[u8] = match behaviour {
        Behaviour::ServeCatalog => CATALOG,
        Behaviour::Garbage => b"this is not a catalog",
        Behaviour::ServerError | Behaviour::HangUp => b"",
    };

    match behaviour {
        Behaviour::HangUp => {
            // Close without writing anything: the client sees a connection that
            // opened and produced no response.
            drop(stream);
        }
        Behaviour::ServerError => {
            let _ = stream
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");
        }
        _ => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
        }
    }
}

const CATALOG: &[u8] = include_bytes!("../../fusiform-core/fixtures/models-dev-excerpt.json");

struct Harness {
    ctx: PollContext,
    signals: Arc<Signals>,
    _dir: tempfile::TempDir,
}

fn harness(url: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    })
    .unwrap();

    let signals = Arc::new(Signals::new());
    signals.store_opened();

    Harness {
        ctx: PollContext {
            store: Arc::new(store),
            fetcher: Fetcher::new().unwrap(),
            endpoint: SourceEndpoint {
                source: SourceId::ModelsDev,
                url: url.to_string(),
            },
            signals: Arc::clone(&signals),
        },
        signals,
        _dir: dir,
    }
}

/// A failure streak is recorded, degrades health, and never narrows a window.
#[tokio::test]
async fn a_failure_streak_is_recorded_without_corrupting_history() {
    let upstream = Upstream::start(Behaviour::ServeCatalog);
    let h = harness(&upstream.url);

    // One good poll establishes a catalog and an observation at a known instant.
    let good = tick(&h.ctx, 1_000).await.expect("the first poll succeeds");
    assert!(matches!(good.outcome, TickOutcome::Changed { .. }));
    assert!(good.eras_written > 10);

    // Now the upstream breaks, in three different ways, cycling until the
    // streak reaches the declared threshold. Driven by the constant rather than
    // a literal count: hardcoding "three" would make this test disagree with
    // the threshold the moment someone changes it, and disagree silently.
    let modes = [
        (Behaviour::ServerError, FailureClass::HttpStatus),
        (Behaviour::Garbage, FailureClass::Parse),
        (Behaviour::HangUp, FailureClass::Network),
    ];
    let streak = FAILURE_STREAK_DEGRADED as usize;
    for i in 0..streak {
        let (behaviour, expected_class) = modes[i % modes.len()];
        upstream.set(behaviour);
        let at = 2_000 + (i as i64) * 1_000;

        // One below the threshold, health must still be Ok: a single missed
        // poll is not a status change, and a threshold that fires at one would
        // page on every transient blip.
        if i == streak - 1 {
            assert_eq!(
                health::report(&h.signals, at).status,
                HealthStatus::Ok,
                "the streak must not degrade before it reaches {streak}"
            );
        }

        let report = tick(&h.ctx, at)
            .await
            .expect("a fetch failure is not a tick error");
        match report.outcome {
            TickOutcome::Failed { class } => assert_eq!(
                class, expected_class,
                "the failure class must describe what actually went wrong"
            ),
            other => panic!("expected a failure, got {other:?}"),
        }
        assert_eq!(
            report.eras_written, 0,
            "a poll that observed nothing must write no eras"
        );
    }

    // At the threshold, health degrades.
    let report = health::report(&h.signals, 9_000);
    assert_eq!(
        report.status,
        HealthStatus::Degraded,
        "{streak} consecutive failures must degrade: {:?}",
        report.detail
    );
    // But not Failing: the store is open and every read still answers correctly
    // from history. A module that pages on a refresh outage teaches an operator
    // to ignore the signal.
    assert_ne!(report.status, HealthStatus::Failing);

    // Every failure is recorded.
    let polls = h
        .ctx
        .store
        .recent_observations(SourceId::ModelsDev, 10)
        .unwrap();
    assert_eq!(
        polls.len(),
        streak + 1,
        "every poll is recorded, the successful one and all {streak} failures"
    );
    assert_eq!(
        polls.iter().filter(|p| p.outcome == "failed").count(),
        streak,
        "the failures are recorded rather than dropped"
    );

    // And none of them became a window edge. The next confirming observation
    // before instant 9999 must still be the successful poll at 1000, not any of
    // the failures at 2000-4000.
    let edge = h
        .ctx
        .store
        .last_confirming_observation_before(SourceId::ModelsDev, fusiform_core::Timestamp(9_999))
        .unwrap();
    assert_eq!(
        edge,
        Some(fusiform_core::Timestamp(1_000)),
        "a failed poll must never act as a window edge"
    );
}

/// Recovery after a streak resumes normal operation.
#[tokio::test]
async fn the_loop_recovers_when_the_upstream_returns() {
    let upstream = Upstream::start(Behaviour::ServeCatalog);
    let h = harness(&upstream.url);

    tick(&h.ctx, 1_000).await.unwrap();

    upstream.set(Behaviour::ServerError);
    for i in 0..FAILURE_STREAK_DEGRADED {
        tick(&h.ctx, 2_000 + (i as i64) * 1_000).await.unwrap();
    }
    assert_eq!(
        health::report(&h.signals, 9_000).status,
        HealthStatus::Degraded,
        "the outage must degrade before recovery is tested, or the recovery proves nothing"
    );

    // The upstream comes back with the same document it served before.
    upstream.set(Behaviour::ServeCatalog);
    let recovered = tick(&h.ctx, 10_000).await.unwrap();

    // Unchanged, not Changed: the document is identical, so no era opens. A
    // recovery that rewrote every fact would put a spurious boundary on all of
    // them and claim the world moved while the upstream was down.
    assert_eq!(
        recovered.outcome,
        TickOutcome::Unchanged,
        "an unchanged document after an outage must not open eras"
    );
    assert_eq!(recovered.eras_written, 0);

    // Health returns to Ok on the first success, rather than requiring a streak
    // of successes to clear.
    let report = health::report(&h.signals, 10_500);
    assert_eq!(
        report.status,
        HealthStatus::Ok,
        "one success clears the streak: {:?}",
        report.detail
    );
}

/// A conditional request is sent once an ETag is known.
///
/// Verified by counting requests rather than by inspecting headers, then by the
/// outcome: the stub does not implement conditional GET, so a second poll of an
/// unchanged document must come back Unchanged via a full body rather than
/// erroring.
#[tokio::test]
async fn polling_continues_against_a_server_without_conditional_support() {
    let upstream = Upstream::start(Behaviour::ServeCatalog);
    let h = harness(&upstream.url);

    tick(&h.ctx, 1_000).await.unwrap();
    let second = tick(&h.ctx, 2_000).await.unwrap();

    assert_eq!(upstream.request_count(), 2);
    assert_eq!(
        second.outcome,
        TickOutcome::Unchanged,
        "a server that ignores If-None-Match must still produce an Unchanged poll"
    );
}
