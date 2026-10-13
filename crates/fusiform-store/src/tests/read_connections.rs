use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use cortexkit_store::{Isolation, StorageBackend, StorageDescriptor};
use rusqlite::ErrorCode;

use crate::CatalogStore;

const READ_DEADLINE: Duration = Duration::from_secs(2);
const HOLD_DEADLINE: Duration = Duration::from_secs(10);

fn open(dir: &tempfile::TempDir) -> CatalogStore {
    CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into(),
        },
    })
    .unwrap()
}

#[test]
fn catalog_read_completes_during_writer_transaction_and_sees_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    assert_eq!(store.catalog_version().unwrap(), 0);

    let (ready_tx, ready_rx) = mpsc::channel();
    let (commit_tx, commit_rx) = mpsc::channel();
    let (read_tx, read_rx) = mpsc::channel();
    let during_write = thread::scope(|scope| {
        let store = &store;
        let writer = scope.spawn(move || {
            store
                .inner
                .with_conn_fenced(|tx| {
                    tx.execute("UPDATE catalog_version SET version = 1 WHERE id = 1", [])?;
                    assert_eq!(
                        tx.query_row(
                            "SELECT version FROM catalog_version WHERE id = 1",
                            [],
                            |r| { r.get::<_, i64>(0) }
                        )?,
                        1,
                        "the writer reads its own uncommitted change"
                    );
                    ready_tx.send(()).unwrap();
                    commit_rx.recv_timeout(HOLD_DEADLINE).unwrap();
                    Ok(())
                })
                .unwrap();
        });
        ready_rx.recv_timeout(HOLD_DEADLINE).unwrap();
        let reader = scope.spawn(move || read_tx.send(store.catalog_version()).unwrap());
        let result = read_rx.recv_timeout(READ_DEADLINE);

        // Release before asserting or joining: routing a read onto the writer
        // must fail by the deadline rather than deadlock the test's cleanup.
        commit_tx.send(()).unwrap();
        writer.join().unwrap();
        reader.join().unwrap();
        result
    });

    assert_eq!(
        during_write
            .expect("catalog read waited on the writer instead of completing within two seconds")
            .unwrap(),
        0,
        "a concurrent catalog read must return the last committed version"
    );
    assert_eq!(
        store.catalog_version().unwrap(),
        1,
        "the next catalog read must see the committed write"
    );
}

#[test]
fn every_read_connection_rejects_writes() {
    fn check(store: &CatalogStore, remaining: usize) {
        store
            .raw_conn(|conn| {
                let err = conn
                    .execute("UPDATE catalog_version SET version = 99 WHERE id = 1", [])
                    .expect_err("a read connection must reject even a valid write");
                assert_eq!(err.sqlite_error_code(), Some(ErrorCode::ReadOnly));
                // Nested checkouts hold the previous connections so this probes
                // every pool member, not the same idle connection four times.
                if remaining > 1 {
                    check(store, remaining - 1);
                }
                Ok(())
            })
            .unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    check(&store, 4);
    assert_eq!(store.catalog_version().unwrap(), 0);
}

#[test]
fn a_slow_read_does_not_serialize_other_catalog_reads() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (read_tx, read_rx) = mpsc::channel();
    let parallel_read = thread::scope(|scope| {
        let store = &store;
        let slow_reader = scope.spawn(move || {
            store
                .raw_conn(|conn| {
                    let tx = conn.unchecked_transaction()?;
                    let version = tx.query_row(
                        "SELECT version FROM catalog_version WHERE id = 1",
                        [],
                        |r| r.get::<_, i64>(0),
                    )?;
                    ready_tx.send(()).unwrap();
                    release_rx.recv_timeout(HOLD_DEADLINE).unwrap();
                    tx.commit()?;
                    Ok(version)
                })
                .unwrap()
        });
        ready_rx.recv_timeout(HOLD_DEADLINE).unwrap();
        let reader = scope.spawn(move || read_tx.send(store.catalog_version()).unwrap());
        let result = read_rx.recv_timeout(READ_DEADLINE);
        release_tx.send(()).unwrap();
        assert_eq!(slow_reader.join().unwrap(), 0);
        reader.join().unwrap();
        result
    });
    assert_eq!(
        parallel_read
            .expect("a catalog read waited on a slow reader despite an idle pool connection")
            .unwrap(),
        0
    );
}
