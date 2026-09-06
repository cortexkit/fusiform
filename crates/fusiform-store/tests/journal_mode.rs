//! The journal mode is a load-bearing guarantee, so it is asserted where a
//! reader looking for storage-engine properties will find it.
//!
//! This test lived in `history.rs` — a file about era window derivation —
//! because that is where I was working when I measured it, not because it
//! belongs there. A fence in the wrong file is found by the person who already
//! knows it exists, which is not the person it protects.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, SourceId, Timestamp};
use fusiform_store::{CatalogStore, FactKey, NewEra};

/// The store runs in WAL, because a consumer's read must survive fusiform's
/// write.
///
/// # Why this is pinned here rather than trusted
///
/// The journal mode is set by `cortexkit-store`, in another repository, on a
/// connection fusiform never configures. Fusiform is the one that BREAKS if it
/// changes: a rollback-journal store blocks readers for the length of a write,
/// and fusiform's writes are not small — one poll wrote 17,455 eras.
///
/// Measured on a copy of the live store, 2026-08-17, before writing this:
///
/// - a read during an OPEN write transaction returned 92,631 eras in 2.6ms
/// - 80 reads during repeated RESTART checkpoints: 0 failures, worst 1.6ms
///
/// That is the property every consumer depends on and none of them can see —
/// they observe a timeout, not a journal mode. So the guarantee is asserted
/// from the side that suffers when it is withdrawn, which is the same reason a
/// cross-repo wire tripwire lives in the producer rather than the consumer.
#[test]
fn the_store_runs_in_wal_so_reads_survive_writes() {
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
    let store = CatalogStore::open(&descriptor).unwrap();

    // A write, so the journal is actually in use rather than merely configured.
    store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "p".into(),
            model_id: "m".into(),
            fact_key: FactKey::existence(),
            value_json: "\"present\"".to_string(),
            boundary_at: Timestamp(1_000),
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        }])
        .expect("the era must store");

    let conn = rusqlite::Connection::open(&path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .expect("the journal mode must be readable");
    assert_eq!(
        mode.to_lowercase(),
        "wal",
        "the store must run in WAL. In any rollback journal a reader blocks for \
         the length of a write, and fusiform's writes reach five figures of \
         eras — every consumer read during a changed poll would stall or fail. \
         If cortexkit-store changed this deliberately, fusiform needs to know \
         before its consumers discover it as timeouts."
    );
}
