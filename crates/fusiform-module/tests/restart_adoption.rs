//! What health says about writes after a restart.
//!
//! Written as a probe \u2014 print, assert nothing, read the output \u2014 after
//! production reported this minutes after a placement, with 68,768 eras in the
//! store:
//!
//!     "observation_age_ms": 92678,   <- adopted from the store at startup
//!     "last_write_age_ms": null      <- not adopted
//!
//! Two adjacent metrics of the same shape, one restored across a restart and
//! one not. Null is a strong claim rather than a missing value: it says
//! fusiform has never written anything, which is the correct answer for a
//! fresh install and a false one for a restart. The metric could not tell them
//! apart.
//!
//! The probe also found a second defect nobody was looking for \u2014 see
//! `the_two_constructors_agree_about_never`.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::{health, signals::Signals};
use fusiform_store::{CatalogStore, FactKey, NewEra, NewObservation};

fn open(path: &std::path::Path) -> CatalogStore {
    CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.join("store.db").to_string_lossy().to_string(),
        },
    })
    .unwrap()
}

const HOUR: i64 = 3_600_000;

/// Write a store with history: observed and written three hours ago.
fn seed_history(dir: &std::path::Path, at: Timestamp) {
    let store = open(dir);
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: at,
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some("h".into()),
            raw_hash: Some("r".into()),
            etag: None,
            duration_ms: Some(100),
            detail: None,
        })
        .unwrap();
    store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "p".into(),
            model_id: "m".into(),
            fact_key: FactKey::existence(),
            value_json: "\"present\"".into(),
            boundary_at: at,
            // A seed boundary carries no observation; the schema enforces it,
            // which is what caught the first version of this probe.
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        }])
        .unwrap();
}

/// A restart: a fresh process opening an existing store.
///
/// Calls the SAME function the daemon's startup path calls, rather than
/// reproducing what it does. The first version of this helper adopted the two
/// instants itself, and a mutation deleting the adoption from the startup path
/// reddened nothing — it proved the helper worked, not that anything called it.
fn restart(dir: &std::path::Path) -> (CatalogStore, Signals) {
    let store = open(dir);
    let signals = Signals::new();
    signals.store_opened();
    signals.adopt_from_store(&store, SourceId::ModelsDev);
    (store, signals)
}

/// A restart must not claim the catalog has never been written.
///
/// The write clock lives in an atomic that a restart empties, so without
/// adopting the newest era's boundary a module holding a full catalog reports
/// `last_write_age_ms: null`. An operator reads that as a statement about the
/// catalog; it is a statement about the process.
#[test]
fn a_restart_adopts_the_instant_the_catalog_last_changed() {
    let dir = tempfile::tempdir().unwrap();
    let now = 10 * HOUR;
    seed_history(dir.path(), Timestamp(now - 3 * HOUR));

    let (_store, signals) = restart(dir.path());
    let metrics = health::report(&signals, now)
        .metrics
        .expect("metrics are always present");

    assert_eq!(
        metrics["last_write_age_ms"],
        3 * HOUR,
        "a restarted module must report the real age of its newest era, not null"
    );
    assert_eq!(
        metrics["observation_age_ms"],
        3 * HOUR,
        "and the observation clock, which was already adopted"
    );
}

/// A genuinely empty store still reports null, because null is then true.
///
/// The pair matters: adopting an instant unconditionally would replace one
/// wrong answer with another, and a fresh install reporting a write age would
/// be worse than the defect being fixed.
#[test]
fn a_fresh_install_still_reports_no_write() {
    let dir = tempfile::tempdir().unwrap();
    let (_store, signals) = restart(dir.path());

    let metrics = health::report(&signals, 10 * HOUR)
        .metrics
        .expect("metrics are always present");

    assert!(
        metrics["last_write_age_ms"].is_null(),
        "an empty store has genuinely never been written: {}",
        metrics["last_write_age_ms"]
    );
}

/// Both constructors mean the same thing by "never".
///
/// Found by the probe above while looking for something else. `Signals` derived
/// `Default`, and `AtomicI64::default()` is ZERO \u2014 a real instant, the unix
/// epoch \u2014 while `new()` uses `i64::MIN` as its sentinel. So a defaulted
/// `Signals` claimed fusiform last wrote in 1970 and reported an age of the
/// entire unix clock, where production correctly reported null.
///
/// The defect was not either value: it was that every test used `Default` and
/// production used `new()`, so the sentinel under test was never the sentinel
/// that ships. A test can only catch what it constructs.
#[test]
fn the_two_constructors_agree_about_never() {
    let now = 10 * HOUR;
    let from_new = health::report(&Signals::new(), now).metrics.unwrap();
    let from_default = health::report(&Signals::default(), now).metrics.unwrap();

    assert_eq!(
        from_new, from_default,
        "Signals::new() and Signals::default() must describe the same module"
    );
    assert!(
        from_new["last_write_age_ms"].is_null() && from_new["observation_age_ms"].is_null(),
        "a module that has done nothing reports null, not an age since the epoch: {from_new}"
    );
}
