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
//! **Changing permissions does not revoke already open handles.** A new open
//! in the same directory fails after denying file creation (by mode on Unix,
//! by ACL on Windows), while the live connection keeps committing.
//!
//! **Deleting the file does not work** for the same reason — the writes land on
//! the unlinked inode, which still exists while the descriptor is open.
//!
//! **The writer lease is exclusive and tied to an open handle.** A second
//! instance cannot steal it (its open is refused). Removing the lease pathname
//! or writing a different file at that path does not stop the existing holder
//! from committing: the write fence checks the epoch in the database.
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

// Windows has no chmod mode bits; it uses an ACL or the file's read-only attribute.
#[cfg(unix)]
fn chmod(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(mode);
    std::fs::set_permissions(path, perms).unwrap();
}

#[cfg(windows)]
fn set_readonly(path: &std::path::Path, readonly: bool) {
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_readonly(readonly);
    std::fs::set_permissions(path, perms).unwrap();
}

/// A live store keeps committing through file deletion, permission changes and
/// lease tampering.
///
/// Each step is a real operational event: a cleanup script, a permissions
/// hardening pass, a partial restore. None of them should silently stop a
/// running module from recording what it observed.
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

    // Deny the creation of new files in the directory.
    #[cfg(unix)]
    chmod(dir.path(), 0o500);
    #[cfg(windows)]
    {
        fusiform_testkit::windows_acl::set_file_creation_denied(dir.path(), true);
        let err = std::fs::File::create(dir.path().join("probe"))
            .expect_err("the directory ACL must deny a new file");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }
    store
        .record_observation(&observation(2_000))
        .expect("a read-only directory must not stop a live writer with open handles");

    // A new open needs to create a lease and database. It must not clear an
    // existing deny while securing the directory on Windows.
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
        "a new store must not open when the directory denies file creation"
    );
    #[cfg(unix)]
    chmod(dir.path(), 0o700);
    #[cfg(windows)]
    fusiform_testkit::windows_acl::set_file_creation_denied(dir.path(), false);

    // The database file itself goes read-only.
    #[cfg(unix)]
    chmod(&path, 0o400);
    #[cfg(windows)]
    set_readonly(&path, true);
    store
        .record_observation(&observation(3_000))
        .expect("a read-only file must not stop a live writer, for the same reason");
    #[cfg(unix)]
    chmod(&path, 0o600);
    #[cfg(windows)]
    set_readonly(&path, false);

    // Remove the lease pathname underneath the holder.
    let lease = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "lease"))
        .expect("the store takes a lease");
    #[cfg(unix)]
    std::fs::remove_file(&lease).unwrap();
    // Move it aside so the old name is free even if Windows defers removal
    // of the open file until the last handle closes.
    #[cfg(windows)]
    std::fs::rename(&lease, dir.path().join("displaced-lease")).unwrap();
    store
        .record_observation(&observation(4_000))
        .expect("the write fence does not depend on the lease pathname");

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

/// A restore rewinds the fence epoch, and the catalog version recovers anyway.
///
/// Two properties measured together, because one is a hazard and the other is
/// the thing that makes it survivable.
///
/// **The fence rewinds.** `cortexkit-store`'s writer fence is a row in the
/// database it protects (`cortexkit_fence`, compared per write inside the
/// transaction). A file-level restore replaces that row along with everything
/// else, so the epoch a restored file carries is whatever the backup held.
/// Measured: 2 before the restore, and the restored file carries 1.
///
/// That is correct for what the fence defends against — a concurrent second
/// writer, where comparing inside the transaction is strictly stronger than an
/// in-process check because it survives a writer forgetting its own state. It
/// is simply not a defence against restores, and it cannot be: a guard stored
/// in the artifact it guards shares that artifact's fate.
///
/// **The catalog version recovers.** It is `max(now_ms, current + 1)`, so the
/// wall clock carries it past any rewound value without needing anything
/// durable. Measured here: 9000 before the restore, 1000 after, and the next
/// version issued is a current millisecond timestamp — past 9000 by four
/// orders of magnitude.
///
/// That is the property consumers depend on. A consumer refuses any version at
/// or below the one it holds, so a rewind that persisted would make every
/// subsequent push refuse forever, silently at the producer and totally at the
/// consumer.
#[test]
fn a_restore_rewinds_the_fence_and_the_catalog_version_recovers() {
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

    let fence_epoch = |p: &std::path::Path| -> i64 {
        let c = rusqlite::Connection::open(p).unwrap();
        c.query_row(
            "SELECT COALESCE((SELECT epoch FROM cortexkit_fence WHERE id = 0), 0)",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };

    // Run 1, then a backup with the store closed — the checkpointed shape
    // engram's online backup API produces.
    let store = CatalogStore::open(&descriptor).unwrap();
    let id = store.record_observation(&observation(1_000)).unwrap();
    store.advance_catalog_version(id, Timestamp(1_000)).unwrap();
    drop(store);
    let backup = dir.path().join("backup.db");
    std::fs::copy(&path, &backup).unwrap();

    // Run 2 advances both the version and the fence epoch.
    let store = CatalogStore::open(&descriptor).unwrap();
    let id = store.record_observation(&observation(2_000)).unwrap();
    let pre_restore_version = store.advance_catalog_version(id, Timestamp(9_000)).unwrap();
    drop(store);
    let pre_restore_fence = fence_epoch(&path);
    assert!(
        pre_restore_fence > 1,
        "run 2 must have claimed a higher epoch, or the rewind below proves nothing"
    );

    // The restore: module stopped, file replaced, sidecars removed.
    std::fs::copy(&backup, &path).unwrap();
    let _ = std::fs::remove_file(dir.path().join("store.db-wal"));
    let _ = std::fs::remove_file(dir.path().join("store.db-shm"));

    assert!(
        fence_epoch(&path) < pre_restore_fence,
        "the fence lives in the restored file, so it must come back rewound — \
         if this ever stops being true, the comment above is wrong"
    );

    // Run 3 comes up on the restored file.
    let store = CatalogStore::open(&descriptor).unwrap();
    assert!(
        store.catalog_version().unwrap() < pre_restore_version,
        "the stored version must also be rewound, or the recovery below is untested"
    );

    let id = store.record_observation(&observation(3_000)).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let recovered = store.advance_catalog_version(id, Timestamp(now)).unwrap();

    assert!(
        recovered > pre_restore_version,
        "the next version after a restore must exceed the pre-restore high water \
         ({recovered} vs {pre_restore_version}) — otherwise every consumer holding \
         the old version refuses every push, correctly by its own rule and \
         permanently"
    );
}

/// `era_count` rewinds on a restore and `catalog_version` does not.
///
/// Both rise monotonically in every observation a consumer can make, so from
/// the payload they are indistinguishable as change signals. They behave
/// differently in exactly one situation, and it is the one that matters: a
/// restore.
///
/// A consumer holding `era_count` as a high-water mark would, after a restore,
/// see the catalog go backwards and then reprocess values it had already seen.
/// A consumer holding `catalog_version` sees it jump forward and keeps working.
///
/// Written after ASTRO found the same class in their own allocator: theirs
/// derives from `maximum + 1` over durable tables with no clock floor, so a
/// restore rewinds every authority together and it reissues versions the
/// receiver has already accepted. Fusiform's version is immune by derivation —
/// this pins that the immunity is real AND that it does not extend to the
/// neighbouring counters, because the wire crate now tells consumers so.
#[test]
fn a_restore_rewinds_the_row_counts_but_not_the_catalog_version() {
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

    let era = |at: i64, fact: &str| fusiform_store::NewEra {
        source: SourceId::ModelsDev,
        provider_id: "anthropic".into(),
        model_id: "claude-sonnet-4-5".into(),
        fact_key: fusiform_store::FactKey::from_stored(fact.to_string()),
        value_json: format!("{at}"),
        boundary_at: Timestamp(at),
        boundary_kind: fusiform_core::BoundaryKind::Seed,
        observation_id: None,
    };

    // Run 1: a small catalog, then a backup.
    let store = CatalogStore::open(&descriptor).unwrap();
    let id = store.record_observation(&observation(1_000)).unwrap();
    store.append_eras(&[era(1_000, "rate.input")]).unwrap();
    store.advance_catalog_version(id, Timestamp(1_000)).unwrap();
    let eras_at_backup = store.era_count(SourceId::ModelsDev).unwrap();
    drop(store);
    let backup = dir.path().join("backup.db");
    std::fs::copy(&path, &backup).unwrap();

    // Run 2: more eras, higher version.
    let store = CatalogStore::open(&descriptor).unwrap();
    let id = store.record_observation(&observation(2_000)).unwrap();
    store
        .append_eras(&[era(2_000, "rate.output"), era(2_000, "limit.context")])
        .unwrap();
    let version_before = store.advance_catalog_version(id, Timestamp(9_000)).unwrap();
    let eras_before = store.era_count(SourceId::ModelsDev).unwrap();
    assert!(
        eras_before > eras_at_backup,
        "run 2 must add eras, or the rewind below proves nothing"
    );
    drop(store);

    // The restore.
    std::fs::copy(&backup, &path).unwrap();
    let _ = std::fs::remove_file(dir.path().join("store.db-wal"));
    let _ = std::fs::remove_file(dir.path().join("store.db-shm"));

    let store = CatalogStore::open(&descriptor).unwrap();

    // The row count went backwards, which is why it must not be a change signal.
    assert_eq!(
        store.era_count(SourceId::ModelsDev).unwrap(),
        eras_at_backup,
        "era_count is a plain row count and a restore rewinds it"
    );

    // And the version, once the module polls again, exceeds the pre-restore
    // high water rather than repeating it.
    let id = store.record_observation(&observation(3_000)).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let version_after = store.advance_catalog_version(id, Timestamp(now)).unwrap();
    assert!(
        version_after > version_before,
        "the catalog version must clear the pre-restore high water \
         ({version_after} vs {version_before}) while the row count does not — \
         that difference is the whole reason one is safe to hold and the other \
         is not"
    );
}
