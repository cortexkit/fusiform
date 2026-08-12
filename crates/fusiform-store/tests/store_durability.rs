//! What does NOT break a live store.
//!
//! Written after trying to make a store refuse a write, so a test could drive
//! the "store is broken" path behaviourally rather than by checking source
//! ordering. Five approaches, all of which the store survived — so the
//! behavioural test is not available, and the reasons are worth pinning as
//! durability properties in their own right.
//!
//! # Why every attempt failed
//!
//! **Permission checks happen at `open(2)`, not at `write(2)`.** SQLite holds a
//! file descriptor from before any permission change, so nothing done to the
//! directory or the file afterwards can affect writes through it. Confirmed
//! rather than assumed: a NEW open in the same read-only directory fails with
//! `PermissionDenied` while the live connection keeps committing.
//!
//! **Deleting the file does not work** for the same reason — the writes land on
//! the unlinked inode, which still exists while the descriptor is open.
//!
//! **The writer lease is exclusive and in-memory.** A second instance cannot
//! steal it (its open is refused), and deleting or overwriting the lease FILE
//! does not affect the holder, because the fence is an in-process epoch check
//! rather than a per-write disk read.
//!
//! So a write failure in this store requires a genuine I/O error — a full disk,
//! failing hardware — which is not reachable without fault injection at the VFS
//! layer. The ordering guard in `fusiform-module` is a source check for that
//! reason, and it says so.
//!
//! These are all real operational events, so the properties are worth having
//! regardless of what motivated the search.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{ObservationOutcome, SourceId, Timestamp};
use fusiform_store::{CatalogStore, NewObservation};

fn observation(at: i64) -> NewObservation {
    NewObservation {
        source: SourceId::ModelsDev,
        observed_at: Timestamp(at),
        outcome: ObservationOutcome::Unchanged,
        normalized_hash: Some(format!("h{at}")),
        raw_hash: None,
        etag: None,
        duration_ms: Some(10),
        detail: None,
    }
}

#[cfg(unix)]
fn chmod(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(mode);
    std::fs::set_permissions(path, perms).unwrap();
}

/// A live store keeps committing through file deletion, permission changes and
/// lease tampering.
///
/// Each step is a real operational event: a cleanup script, a permissions
/// hardening pass, a partial restore. None of them should silently stop a
/// running module from recording what it observed.
#[cfg(unix)]
#[test]
fn a_live_store_survives_the_data_directory_being_tampered_with() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_string_lossy().to_string(),
        },
    })
    .unwrap();

    store
        .record_observation(&observation(1_000))
        .expect("a healthy store writes");

    // The directory goes read-only.
    chmod(dir.path(), 0o500);
    store
        .record_observation(&observation(2_000))
        .expect("a read-only directory must not stop a live writer: the fd predates it");

    // A NEW open in the same directory fails, which is what proves the
    // explanation above rather than leaving it a plausible story.
    let blocked = CatalogStore::open(&StorageDescriptor {
        module_id: "other".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir
                .path()
                .join("elsewhere.db")
                .to_string_lossy()
                .to_string(),
        },
    });
    assert!(
        blocked.is_err(),
        "a NEW open in a read-only directory must fail \u{2014} otherwise the \
         permission change did nothing at all and this test proves nothing"
    );
    chmod(dir.path(), 0o700);

    // The database file itself goes read-only.
    chmod(&path, 0o400);
    store
        .record_observation(&observation(3_000))
        .expect("a read-only file must not stop a live writer, for the same reason");
    chmod(&path, 0o600);

    // The lease file is deleted underneath the holder.
    let lease = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "lease"))
        .expect("the store takes a lease");
    std::fs::remove_file(&lease).unwrap();
    store
        .record_observation(&observation(4_000))
        .expect("the fence is an in-process epoch check, not a per-write disk read");

    // And rewritten to name a different holder.
    std::fs::write(&lease, br#"{"holder":"someone-else","pid":1}"#).unwrap();
    store
        .record_observation(&observation(5_000))
        .expect("a forged lease file must not fence the real holder");

    // Everything landed. The point is not that tampering is harmless — it is
    // that a module which is running and observing correctly keeps its record,
    // and that none of these produce a silent write failure.
    let polls = store
        .recent_observations(SourceId::ModelsDev, 10)
        .expect("reads still work");
    assert_eq!(polls.len(), 5, "every write must have landed: {polls:?}");
}

/// A second instance cannot take the writer lease from a live one.
///
/// The exclusivity is what makes lease theft unusable as a way to simulate a
/// broken store, and it is the property that matters in production: two
/// supervised instances of the same module must not both believe they may
/// write.
#[test]
fn a_second_instance_cannot_steal_the_writer_lease() {
    let dir = tempfile::tempdir().unwrap();
    let descriptor = StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    };

    let first = CatalogStore::open(&descriptor).expect("the first open takes the lease");
    let second = CatalogStore::open(&descriptor);
    assert!(
        second.is_err(),
        "a second instance must be refused rather than fencing the first"
    );

    // And the first is unaffected by the attempt.
    first
        .record_observation(&observation(1_000))
        .expect("the holder keeps writing after a refused challenger");
}
