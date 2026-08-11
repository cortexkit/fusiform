//! What health reports immediately after a restart.
//!
//! A probe for a specific worry: the staleness signal lives in memory, and a
//! restart empties it. If nothing primes it from the store, a module that has
//! been unable to reach the upstream for hours reports healthy the moment it is
//! restarted — and a restart is exactly what an operator does when something
//! looks wrong.

use std::sync::Arc;

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{ObservationOutcome, SourceId, Timestamp};
use fusiform_module::health;
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
    signals.failed();

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
