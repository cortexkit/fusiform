//! The stored failure-class word round-trips, in both directions, for every
//! class.
//!
//! # Why a round-trip rather than a spelling check
//!
//! The class is written by the ingest path and read back by the health and
//! status paths. Those are two directions of one mapping, and a spelling check
//! on either half passes while they disagree — the store would keep writing
//! `http_status` and a reader looking for `http` would report NO CLASS, which
//! an operator reads as "the failure had no cause" rather than as a defect.
//!
//! Exhaustive over the domain enum rather than over a list written here,
//! because a list is a second copy of the thing under test. Adding a
//! `FailureClass` variant without a word fails to compile in
//! `failure_class_word`; adding one whose word does not decode fails here.

use fusiform_core::{FailureClass, ObservationOutcome, SourceId, Timestamp};
use fusiform_store::{failure_class_from_stored, failure_class_word, CatalogStore, NewObservation};

/// Every class, listed by matching exhaustively so the compiler maintains it.
fn every_class() -> Vec<FailureClass> {
    let all = [
        FailureClass::Network,
        FailureClass::HttpStatus,
        FailureClass::Parse,
        FailureClass::Implausible,
    ];
    // The exhaustiveness fence: a new variant makes this match fail to compile,
    // which is the moment to add it to the array above.
    for c in all {
        match c {
            FailureClass::Network
            | FailureClass::HttpStatus
            | FailureClass::Parse
            | FailureClass::Implausible => {}
        }
    }
    all.to_vec()
}

#[test]
fn every_class_survives_the_word_and_back() {
    for class in every_class() {
        let word = failure_class_word(class);
        assert_eq!(
            failure_class_from_stored(word),
            Some(class),
            "{class:?} writes {word:?} and does not read back: the ingest path \
             and the health path disagree about one class, and the health \
             surface would report no cause for a failure that has one"
        );
    }
}

#[test]
fn an_unknown_word_reports_no_class_rather_than_guessing() {
    // A word from a future build, or a hand-edited row. Reporting a WRONG class
    // sends an operator somewhere specific and wrong, which is worse than
    // sending them nowhere.
    assert_eq!(failure_class_from_stored("quota"), None);
    assert_eq!(failure_class_from_stored("http"), None);
    assert_eq!(failure_class_from_stored(""), None);
}

#[test]
fn a_stored_failure_reads_back_with_its_class() {
    // The round trip through SQLite rather than through the two functions, so
    // the column really carries the word.
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&cortexkit_store_types::StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: cortexkit_store_types::Isolation::Module,
        backend: cortexkit_store_types::StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    })
    .unwrap();

    for (i, class) in every_class().into_iter().enumerate() {
        store
            .record_observation(&NewObservation {
                source: SourceId::ModelsDev,
                observed_at: Timestamp(1_000 + i as i64 * 100),
                outcome: ObservationOutcome::Failed { class },
                normalized_hash: None,
                raw_hash: None,
                etag: None,
                duration_ms: Some(10),
                detail: None,
            })
            .unwrap();

        let (count, at, read_class) = store.failure_history(SourceId::ModelsDev).unwrap();
        assert_eq!(count, i as i64 + 1, "each failure must be counted");
        assert_eq!(
            at,
            Some(Timestamp(1_000 + i as i64 * 100)),
            "the instant must be the NEWEST failure's"
        );
        assert_eq!(
            read_class,
            Some(class),
            "the newest failure's class must survive the column: {class:?} was \
             written and {read_class:?} came back"
        );
    }
}
