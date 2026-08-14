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

/// Serve one request and close.
///
/// `Connection: close` on every response is load-bearing, not politeness. This
/// function handles ONE request per connection and then drops the socket, while
/// an HTTP/1.1 response carrying `Content-Length` and no close header advertises
/// a reusable connection. A pooling client — reqwest's default — takes that at
/// its word, keeps the socket, and sends its next request into a server that
/// already hung up.
///
/// That mismatch is a race by construction: whether it fails depends on whether
/// the client notices the close before writing, which depends on scheduling.
///
/// NOT PROVEN to be the cause of the one CI failure this file has had. Thirty
/// runs with the close header removed produced zero failures locally, so the
/// theory survives reading and was not reproduced. It is fixed anyway because
/// the mismatch is real and removing it costs one header — but "flaky test
/// fixed" would be a claim the evidence does not support, and the assertion
/// reordering below is what makes the next occurrence say what actually
/// happened.
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
            let _ = stream.write_all(
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\
                  Connection: close\r\n\r\n",
            );
        }
        _ => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
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

/// Polling continues against a server that ignores conditional requests.
///
/// The property under test is the OUTCOME: the stub does not implement
/// conditional GET, so a second poll of an unchanged document must come back
/// Unchanged via a full body rather than erroring.
///
/// The outcomes are asserted before the request count, and that ordering is the
/// point rather than style. `tick` RECORDS a failed poll and returns `Ok`,
/// because a failure is a normal outcome of polling rather than an error of the
/// loop. So a first tick that failed for an environmental reason — a slow CI
/// runner, a socket that was not ready — leaves the count at 1 and everything
/// else looking fine.
///
/// This test failed exactly that way on CI once, on a docs-only commit, and
/// passed on the commits either side. The failure message was `left: 1, right:
/// 2` with no indication that a poll had failed at all, because the count was
/// checked first. Forty local runs never reproduced it.
///
/// A count mismatch now cannot be the first thing you see: if a poll failed,
/// the assertion that fires says so, with the failure class the tick recorded.
#[tokio::test]
async fn polling_continues_against_a_server_without_conditional_support() {
    let upstream = Upstream::start(Behaviour::ServeCatalog);
    let h = harness(&upstream.url);

    let first = tick(&h.ctx, 1_000).await.unwrap();
    assert!(
        matches!(first.outcome, TickOutcome::Changed { .. }),
        "the first poll of a fresh store must ingest the document, got {:?}",
        first.outcome
    );

    let second = tick(&h.ctx, 2_000).await.unwrap();
    assert_eq!(
        second.outcome,
        TickOutcome::Unchanged,
        "a server that ignores If-None-Match must still produce an Unchanged poll"
    );

    // Both polls reached the server. Checked last because it is corroboration
    // for the outcomes above rather than the property itself — and because a
    // count is the least informative thing to fail on.
    assert_eq!(
        upstream.request_count(),
        2,
        "both polls must have reached the upstream"
    );
}

/// Health names WHAT is failing, not just that something is.
///
/// A count of failures says a module is unwell; it does not say whether the
/// upstream is unreachable, answering with an error status, or serving a body
/// that will not parse. Those have different fixes and different owners.
///
/// Recording the cause in the store is not enough on its own. A sibling module
/// ran three days with one of its data sources failing every poll and the
/// reason recorded in a store row throughout, while the health surface its
/// operator watched reported only that something was wrong.
#[tokio::test]
async fn health_names_the_cause_of_a_failure_streak() {
    let upstream = Upstream::start(Behaviour::ServeCatalog);
    let h = harness(&upstream.url);
    tick(&h.ctx, 1_000).await.unwrap();

    // Each failure mode must produce its own description, so an operator can
    // tell an outage from a bad payload without opening the store.
    let cases = [
        (Behaviour::HangUp, "upstream unreachable", "network"),
        (
            Behaviour::ServerError,
            "upstream returned an error status",
            "http_status",
        ),
        (Behaviour::Garbage, "upstream body did not parse", "parse"),
    ];

    let mut at = 2_000i64;
    for (behaviour, expected_detail, expected_metric) in cases {
        upstream.set(behaviour);
        // Drive a full streak so health reaches Degraded and prints a detail.
        for _ in 0..FAILURE_STREAK_DEGRADED {
            tick(&h.ctx, at).await.unwrap();
            at += 1_000;
        }

        let report = health::report(&h.signals, at);
        let detail = report
            .detail
            .clone()
            .expect("a degraded report explains itself");
        assert!(
            detail.contains(expected_detail),
            "health must name the cause; got {detail:?}"
        );

        let metrics = report.metrics.clone().expect("metrics are always present");
        assert_eq!(
            metrics.get("last_failure_class").and_then(|v| v.as_str()),
            Some(expected_metric),
            "a machine reading health must branch on the cause without parsing prose"
        );

        // Recover, so the next case starts from a clean streak.
        upstream.set(Behaviour::ServeCatalog);
        tick(&h.ctx, at).await.unwrap();
        at += 1_000;
    }
}

/// A recovery clears the STREAK and keeps the cause, and health stops
/// explaining a failure that is over.
///
/// # This test's name and assertion both changed, which is the record
///
/// It read "a recovery clears the recorded cause", and that was right while the
/// class described the current streak: a class outliving its streak would have
/// health explaining a failure that is no longer happening.
///
/// The class became DURABLE — adopted from the store so a healed failure keeps
/// its cause, which is what an operator asks about after the fact. So clearing
/// it is now the defect: production showed `ck models status` reporting
/// "(network)" from the store while `ck health` reported null, because a
/// successful poll erased what startup adoption had stamped.
///
/// The original concern survives and is asserted below, in the place it
/// actually belongs: health must not PRESENT an old failure as the current
/// state. That is a property of the report's prose, not of whether the field
/// retains a value — and separating them is what lets both be true.
#[tokio::test]
async fn a_recovery_keeps_the_cause_and_stops_explaining_it() {
    let upstream = Upstream::start(Behaviour::ServeCatalog);
    let h = harness(&upstream.url);
    tick(&h.ctx, 1_000).await.unwrap();

    upstream.set(Behaviour::ServerError);
    for i in 0..FAILURE_STREAK_DEGRADED {
        tick(&h.ctx, 2_000 + (i as i64) * 1_000).await.unwrap();
    }
    assert!(h.signals.last_failure_class().is_some());

    upstream.set(Behaviour::ServeCatalog);
    tick(&h.ctx, 9_000).await.unwrap();

    assert_eq!(
        h.signals.last_failure_class(),
        Some(fusiform_core::FailureClass::HttpStatus),
        "the cause must SURVIVE recovery: it describes the last failure, which \
         stays true after the next poll succeeds, and an operator asking what \
         went wrong needs it once the streak has cleared"
    );
    assert_eq!(
        h.signals.consecutive_failures(),
        0,
        "the streak is what clears — it is the field that says whether \
         something is failing NOW"
    );
    let report = health::report(&h.signals, 9_500);
    assert_eq!(report.status, HealthStatus::Ok);
    assert!(
        !report
            .detail
            .clone()
            .unwrap_or_default()
            .contains("error status"),
        "a recovered module must not still be explaining an old failure"
    );
}

/// A real tick stamps the attempt, whatever the outcome.
///
/// This is the test that was missing, and a mutation is what showed it. The
/// attempt stamp originally lived in the poll loop in `main.rs`; no test
/// exercises that loop, since every test calls `tick` directly, so deleting the
/// call reddened nothing. The signal that tells an operator the loop is alive
/// could have been silently removed.
///
/// Moved into `tick` so the caller cannot forget it and this test reaches it.
#[tokio::test]
async fn a_tick_stamps_the_attempt_on_every_outcome() {
    let upstream = Upstream::start(Behaviour::ServeCatalog);
    let h = harness(&upstream.url);

    assert_eq!(
        h.signals.attempt_age_ms(1_000),
        None,
        "nothing has been attempted yet"
    );

    // A successful tick stamps it.
    tick(&h.ctx, 1_000).await.unwrap();
    assert_eq!(
        h.signals.attempt_age_ms(1_000),
        Some(0),
        "a successful tick must stamp the attempt"
    );

    // And so does a failing one — which is the case that matters, because a
    // loop that is alive and failing must not be reported as stopped.
    upstream.set(Behaviour::ServerError);
    tick(&h.ctx, 5_000).await.unwrap();
    assert_eq!(
        h.signals.attempt_age_ms(5_000),
        Some(0),
        "a FAILING tick must stamp the attempt too, or a prolonged outage \
         reads as a dead loop and sends an operator to restart a module that \
         is working correctly"
    );

    // Health agrees: alive and failing, not stopped.
    let report = health::report(&h.signals, 5_000);
    assert!(
        !report
            .detail
            .as_deref()
            .unwrap_or("")
            .contains("not attempted"),
        "a loop that just attempted must not be called stopped: {:?}",
        report.detail
    );
}

/// The in-memory failure signal is stamped BEFORE the observation write.
///
/// ASTRO's fourth variant, checked for here on their report: a failure path
/// that depends on the failing subsystem cannot report on it. `signals.failed`
/// is process-local and exists precisely to be readable when the store is not —
/// and it was stamped AFTER the observation write, so the `?` on that write
/// returned early and a store outage suppressed the record of a fetch outage
/// happening at the same time.
///
/// Measured with a probe before the fix: network down and disk broken
/// together, health reported `consecutive_failures: 0` and
/// `last_failure_class: null` for three hours while the attempt ledger
/// correctly reported the store problem. The upstream outage was invisible.
///
/// # Why this is a source check rather than a runtime one
///
/// Making a real store refuse a write is not reachable from a test. Deleting
/// the file does not work — SQLite holds an open descriptor and writes to the
/// unlinked inode. Taking the writer lease from a second instance does not
/// either: the lease is exclusive, so the second open is refused rather than
/// the first being fenced. A mock store would prove only that a fake behaved as
/// instructed.
///
/// So this asserts the ordering in the shipped source. It is narrower than a
/// behavioural test and it is not vacuous: swapping the two lines reddens it by
/// name, which is the mutation that reintroduces the defect. Stated plainly
/// rather than dressed up, because a test that cannot reach the failure it
/// guards should say so.
#[test]
fn the_failure_signal_is_stamped_before_the_store_write() {
    const SOURCE: &str = include_str!("../src/loop_.rs");

    let body = SOURCE
        .split_once("fn record_failure(")
        .expect("record_failure must exist")
        .1;

    let stamp = body
        .find("ctx.signals.failed(class);")
        .expect("record_failure must stamp the in-memory failure signal");
    let write = body
        .find(".record_observation(")
        .expect("record_failure must write an observation row");

    assert!(
        stamp < write,
        "`signals.failed` must be stamped BEFORE `record_observation`. It is \
         the signal that survives a store outage, and writing it behind the \
         store means a broken disk hides every simultaneous upstream failure — \
         measured at consecutive_failures: 0 through a three-hour network outage."
    );
}
