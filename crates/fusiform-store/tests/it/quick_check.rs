use std::io::{Seek, SeekFrom, Write};

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_store::CatalogStore;

fn descriptor(path: &std::path::Path) -> StorageDescriptor {
    StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_string_lossy().to_string(),
        },
    }
}

#[test]
fn quick_check_of_a_fresh_store_is_ok() {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&descriptor(&dir.path().join("store.db"))).unwrap();
    assert_eq!(store.quick_check(), "ok");
}

#[test]
fn quick_check_reports_a_damaged_table_page_in_an_openable_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let descriptor = descriptor(&path);
    drop(CatalogStore::open(&descriptor).unwrap());

    // Damage a separate table so migrations and lease acquisition can still
    // open the store. Leave the database header and schema pages untouched.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE quick_check_probe (payload TEXT);
         INSERT INTO quick_check_probe VALUES ('a page that must be checked');",
    )
    .unwrap();
    let page_size: u64 = conn
        .query_row("PRAGMA page_size", [], |r| r.get(0))
        .unwrap();
    let root_page: u64 = conn
        .query_row(
            "SELECT rootpage FROM sqlite_schema WHERE name = 'quick_check_probe'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(root_page > 1, "the database header must not be damaged");
    drop(conn);

    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start((root_page - 1) * page_size))
        .unwrap();
    file.write_all(&[0xff; 8]).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let store = CatalogStore::open(&descriptor).expect("the damaged store must still open");
    let result = store.quick_check();
    assert_ne!(result, "ok", "a damaged table page must not pass the check");
    assert!(!result.is_empty(), "SQLite must supply a diagnostic");
}
