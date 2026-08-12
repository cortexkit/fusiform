//! What health reports immediately after a restart.
//!
//! A probe for a specific worry: the staleness signal lives in memory, and a
//! restart empties it. If nothing primes it from the store, a module that has
//! been unable to reach the upstream for hours reports healthy the moment it is
//! restarted — and a restart is exactly what an operator does when something
//! looks wrong.

use std::sync::Arc;

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{FailureClass, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::health::{self, LOOP_SILENT_AFTER_MS, STALE_AFTER_MS};
use fusiform_module::signals::Signals;
use fusiform_store::{CatalogStore, NewObservation};
use subc_protocol::session::HealthStatus;

/// A restart must not reset the staleness clock.
///
/// The store holds an observation from six hours ago. A fresh process has an
/// empty signal set. Health must reflect the catalog's real age rather than the
/// process's uptime, because those are different facts and only one of them is
/// about whether the catalog can be trusted.
#[test]
fn a_restart_does_not_reset_the_staleness_clock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let descriptor = StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_string_lossy().to_string(),
        },
    };

    // Six hours ago, relative to a fixed "now" so the test does not depend on
    // the wall clock.
    let now = 1_786_000_000_000i64;
    let six_hours_ago = now - 6 * 60 * 60 * 1_000;

    let store = CatalogStore::open(&descriptor).unwrap();
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(six_hours_ago),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some("h".to_string()),
            raw_hash: None,
            etag: Some("\"e\"".to_string()),
            duration_ms: Some(200),
            detail: None,
        })
        .unwrap();
    drop(store);

    // The process restarts: a new store handle and a brand new signal set,
    // exactly as `on_hello_ack` builds them.
    let store = Arc::new(CatalogStore::open(&descriptor).unwrap());
    let signals = Arc::new(Signals::new());
    signals.store_opened();
    // Reproduce what the daemon does when the store opens: adopt the store's
    // last confirming observation into the fresh signal set, so the staleness
    // clock reflects the catalog rather than the process uptime.
    if let Ok(Some(at)) = store.last_confirming_observation(SourceId::ModelsDev) {
        signals.adopt_last_observation(at.0);
    }

    let report = health::report(&signals, now);
    eprintln!("status after restart: {:?}", report.status);
    eprintln!("detail: {:?}", report.detail);
    eprintln!(
        "metrics: {}",
        serde_json::to_string(&report.metrics).unwrap()
    );

    // What the store actually knows.
    let last = store
        .last_confirming_observation(SourceId::ModelsDev)
        .unwrap();
    eprintln!("store's last confirming observation: {last:?}");
    eprintln!(
        "real catalog age: {} minutes",
        (now - last.unwrap().0) / 60_000
    );

    assert_eq!(
        report.status,
        HealthStatus::Degraded,
        "a six-hour-old catalog is stale regardless of how recently the process \
         started; reporting {:?} makes a restart look like a fix",
        report.status
    );
}

/// And with the upstream unreachable, the restart must not mask it either.
///
/// This is the case that matters: an operator restarts because something looks
/// wrong, the upstream is still down, and the module reports healthy for two
/// hours until the failure streak reaches its threshold.
#[test]
fn a_restart_with_an_unreachable_upstream_still_reports_the_stale_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let descriptor = StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_string_lossy().to_string(),
        },
    };

    let now = 1_786_000_000_000i64;
    let six_hours_ago = now - 6 * 60 * 60 * 1_000;

    let store = CatalogStore::open(&descriptor).unwrap();
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(six_hours_ago),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some("h".to_string()),
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    drop(store);

    let store = Arc::new(CatalogStore::open(&descriptor).unwrap());
    let signals = Arc::new(Signals::new());
    signals.store_opened();
    if let Ok(Some(at)) = store.last_confirming_observation(SourceId::ModelsDev) {
        signals.adopt_last_observation(at.0);
    }

    // One failed poll after the restart: the upstream is still down.
    signals.failed(FailureClass::Network);

    let report = health::report(&signals, now);
    eprintln!("status: {:?} detail: {:?}", report.status, report.detail);

    assert_ne!(
        report.status,
        HealthStatus::Ok,
        "the catalog is six hours old and the upstream is unreachable; \
         reporting Ok means a restart silences the signal"
    );
}

/// A genuinely fresh install is still Ok.
///
/// The other side of the priming fix: adopting the store's last observation
/// must not turn "never observed" into something alarming. A fresh install has
/// no observation because it has not polled yet, and the correct response is to
/// wait one cadence rather than to page someone.
#[test]
fn a_fresh_install_with_no_observation_is_still_ok() {
    let dir = tempfile::tempdir().unwrap();
    let descriptor = StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    };

    let store = Arc::new(CatalogStore::open(&descriptor).unwrap());
    let signals = Arc::new(Signals::new());
    signals.store_opened();

    // The startup path finds nothing to adopt, which is the true answer.
    let adopted = store
        .last_confirming_observation(SourceId::ModelsDev)
        .unwrap();
    assert_eq!(adopted, None, "a fresh store has no observation to adopt");
    if let Some(at) = adopted {
        signals.adopt_last_observation(at.0);
    }

    let report = health::report(&signals, 1_786_000_000_000);
    assert_eq!(
        report.status,
        HealthStatus::Ok,
        "a fresh install has not polled yet; that is not a fault: {:?}",
        report.detail
    );
}

/// A poll loop that has STOPPED is reported as itself, not as staleness.
///
/// Found by probing rather than by reasoning, and the numbers are why it
/// mattered: before this, a dead loop reported Ok for two hours while an
/// alive-but-failing loop reported Degraded in thirty minutes. The worse
/// condition was the quieter one — failures announce themselves and absence
/// does not.
///
/// The gap existed because health is stateless: it is called fresh on every
/// probe with no memory of the last one, so the attempt COUNTER could never say
/// it had stopped advancing. Only an instant can.
///
/// The distinction earns its keep beyond tidiness. A failing upstream may
/// recover on its own; a loop that is not running never will, and an operator
/// told "the catalog is two hours old" goes to look at the upstream.
#[test]
fn a_stopped_loop_is_named_rather_than_reported_as_staleness() {
    let signals = Signals::default();
    signals.store_opened();

    // One healthy poll, then nothing ever again.
    signals.observed(0);
    signals.attempted(0);

    // Within the silence budget: still Ok, because a single missed cadence is
    // scheduler slop rather than a dead loop.
    let early = health::report(&signals, 60 * 60_000);
    assert_eq!(
        early.status,
        HealthStatus::Ok,
        "one hour of silence is inside the budget: {:?}",
        early.detail
    );

    // Past it: named as a stopped loop, with the action.
    let late = health::report(&signals, 90 * 60_000);
    assert_eq!(late.status, HealthStatus::Degraded);
    let detail = late.detail.unwrap();
    assert!(
        detail.contains("not attempted a fetch"),
        "the detail must name the loop, not the catalog's age: {detail}"
    );
    assert!(
        detail.contains("restart the module"),
        "and say what to do about it: {detail}"
    );

    // The probe instant above is only meaningful while it sits inside the
    // staleness window — otherwise this test would pass on a report that said
    // "stale" rather than "stopped". Compile-time, so moving STALE_AFTER_MS
    // below it fails the build rather than quietly weakening the test.
    const _: () = assert!(
        90 * 60_000 < STALE_AFTER_MS,
        "the 90-minute probe must sit inside the staleness window"
    );
}

/// A loop that is alive and failing is NOT reported as stopped.
///
/// The distinguishing case, and the reason `attempted` is stamped separately
/// from `observed`: a failing poll advances the attempt instant and not the
/// observation instant. Without that split, every prolonged outage would be
/// misreported as a dead loop and send an operator to restart a module that is
/// working correctly.
#[test]
fn an_alive_but_failing_loop_is_not_called_stopped() {
    let signals = Signals::default();
    signals.store_opened();
    signals.observed(0);

    // Three hours of polling, every poll failing. The loop is alive and
    // stamping attempts; only observation is stuck.
    let mut at = 0;
    for _ in 0..6 {
        at += 30 * 60_000;
        signals.attempted(at);
        signals.failed(FailureClass::Network);
    }

    let report = health::report(&signals, at + 60_000);
    assert_eq!(report.status, HealthStatus::Degraded);
    let detail = report.detail.unwrap();
    assert!(
        !detail.contains("not attempted"),
        "an alive loop must not be reported as stopped: {detail}"
    );
    assert!(
        detail.contains("consecutive fetch failures") || detail.contains("catalog is"),
        "it must be reported as what it is: {detail}"
    );
}

/// A slow fetch does not look like a dead loop.
///
/// The attempt is stamped at the TOP of a tick rather than after it, so a fetch
/// running to its full 90-second timeout still counts as the loop being alive.
/// Stamping after would make a slow upstream indistinguishable from a stopped
/// loop — the exact distinction the signal exists to draw.
#[test]
fn the_silence_budget_clears_a_full_timeout() {
    // The worst legitimate gap between attempts: one cadence plus a fetch
    // running to its full timeout.
    const FETCH_TIMEOUT_MS: i64 = 90 * 1_000;
    let worst_legitimate_gap = fusiform_module::loop_::POLL_INTERVAL_MS + FETCH_TIMEOUT_MS;

    assert!(
        LOOP_SILENT_AFTER_MS > worst_legitimate_gap,
        "the budget ({LOOP_SILENT_AFTER_MS}ms) must clear the worst legitimate \
         gap ({worst_legitimate_gap}ms), or a slow upstream reports as a dead loop"
    );
}

/// The silence budget's bounds, enforced by the compiler.
///
/// Both are constants, so a test is the wrong instrument: an out-of-range value
/// should fail the BUILD rather than a test someone has to run. Same treatment
/// as the shrink guard's threshold, for the same reason.
const _: () = assert!(
    LOOP_SILENT_AFTER_MS < STALE_AFTER_MS,
    "the loop check must fire BEFORE staleness, or a stopped loop is reported \
     as its own symptom and an operator goes to look at the upstream"
);
const _: () = assert!(
    LOOP_SILENT_AFTER_MS > 2 * fusiform_module::loop_::POLL_INTERVAL_MS,
    "the budget must clear two cadences, or ordinary scheduler slop reports a \
     healthy loop as dead"
);

/// A store that cannot be written is reported, not left to look like staleness.
///
/// Every fetch result is an OUTCOME and gets recorded, so a tick only returns
/// `Err` when the store itself failed — disk full, lease lost, corruption. In
/// that case nothing is written: no observation row, no failure streak, and the
/// error goes to stderr where nothing reads it.
///
/// Measured with a probe before this existed: eleven consecutive store failures
/// reported Ok for two hours, then Degraded-because-stale for three more, never
/// once mentioning writes. An operator told "the catalog is five hours old"
/// goes to look at the upstream, which is fine.
///
/// The identity is ASTRO's: assert that the buckets SUM to the population
/// rather than that each bucket looks right. It only works because the two
/// counters have independent sources — attempts stamped at the top of a tick
/// before the store is touched, verdicts counted when one is recorded.
#[test]
fn ticks_that_reach_no_verdict_are_reported_as_a_write_problem() {
    let s = Signals::default();
    s.store_opened();
    s.attempted(0);
    s.observed(0);

    // The store breaks. The loop keeps ticking; every tick returns Err before
    // recording anything.
    let cadence = fusiform_module::loop_::POLL_INTERVAL_MS;
    for i in 1..=3 {
        s.attempted(i * cadence);
    }

    let report = health::report(&s, 3 * cadence);
    assert_eq!(report.status, HealthStatus::Degraded);

    let detail = report.detail.unwrap();
    assert!(
        detail.contains("no verdict") && detail.contains("check the disk"),
        "the operator must be pointed at the store rather than the upstream: {detail}"
    );

    // And this must fire BEFORE staleness would, or it reports the symptom.
    // At three cadences the catalog is 90 minutes old, which is stale.
    assert!(
        !detail.contains("minutes old"),
        "the cause must be reported rather than the staleness it causes: {detail}"
    );

    // The metrics carry both halves, so an operator can check the arithmetic
    // rather than take health's word for it.
    let m = report.metrics.unwrap();
    assert_eq!(m["poll_attempts"], 4);
    assert_eq!(m["polls_recorded"], 1);
    assert_eq!(m["polls_unrecorded"], 3);
}

/// One unrecorded tick is not a problem, because it is reachable while healthy.
///
/// A health probe landing between the attempt stamp at the top of a tick and
/// the observation write at its end sees a difference of exactly one, every
/// time, on a module doing nothing wrong. Alerting on that would fire on the
/// normal case.
#[test]
fn a_single_in_flight_tick_is_not_a_write_problem() {
    let s = Signals::default();
    s.store_opened();
    s.attempted(0);
    s.observed(0);

    // A tick starts and has not finished yet.
    s.attempted(1_000);

    let report = health::report(&s, 1_000);
    assert_eq!(
        report.status,
        HealthStatus::Ok,
        "an in-flight tick must not be reported as a failure: {:?}",
        report.detail
    );
}
