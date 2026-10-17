//! The backup enrollment descriptor the module writes beside its store.
//!
//! Engram, the fleet's backup owner, only captures a module whose directory
//! holds an `engram-catalog.json`. These tests drive the same library function
//! the daemon calls at startup, against real directories, because every
//! property that matters here — exact bytes, mode, no partial file — is a
//! property of the filesystem rather than of a return value.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_module::enrollment::{
    write_beside_store, write_catalog, CatalogWrite, CATALOG_BYTES, CATALOG_FILE_NAME,
    STORE_FILE_NAME,
};
use fusiform_module::{health, signals::Signals};
use fusiform_store::CatalogStore;
use subc_protocol::session::HealthStatus;

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Every name in `dir` other than the ones listed, so a stray temporary file
/// cannot hide.
fn extra_entries(dir: &Path, expected: &[&str]) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !expected.contains(&n.as_str()))
        .collect()
}

#[test]
fn a_fresh_directory_gets_the_exact_bytes_at_mode_0600() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = write_catalog(dir.path()).unwrap();
    assert_eq!(outcome, CatalogWrite::Created);

    let target = dir.path().join(CATALOG_FILE_NAME);
    // Compared with the repository's file read independently of the embedded
    // constant, so a wrong `include_bytes!` path cannot pass by agreeing with
    // itself.
    let repo = fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/data/engram-catalog.json"
    ))
    .unwrap();
    assert_eq!(fs::read(&target).unwrap(), repo);
    assert_eq!(mode(&target), 0o600);
    assert!(
        extra_entries(dir.path(), &[CATALOG_FILE_NAME]).is_empty(),
        "nothing but the descriptor may be left behind"
    );
}

#[test]
fn an_identical_file_is_left_alone_and_its_mode_tightened() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join(CATALOG_FILE_NAME);
    fs::write(&target, CATALOG_BYTES).unwrap();
    chmod(&target, 0o644);

    // Backdated, so a rewrite would show as a changed mtime even on a
    // filesystem with coarse timestamps.
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    fs::File::options()
        .write(true)
        .open(&target)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let before = fs::metadata(&target).unwrap().modified().unwrap();
    let inode_before = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&target).unwrap());

    let outcome = write_catalog(dir.path()).unwrap();
    assert_eq!(outcome, CatalogWrite::Unchanged);

    let meta = fs::metadata(&target).unwrap();
    assert_eq!(
        meta.modified().unwrap(),
        before,
        "an identical file is not rewritten"
    );
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(&meta),
        inode_before,
        "an identical file is not replaced by a rename either"
    );
    assert_eq!(
        mode(&target),
        0o600,
        "but a world-readable copy is tightened"
    );
}

#[test]
fn a_differing_file_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join(CATALOG_FILE_NAME);
    let edited = b"{\"schema_version\": 1, \"module_id\": \"fusiform\", \"entries\": []}";
    fs::write(&target, edited).unwrap();
    chmod(&target, 0o644);

    let outcome = write_catalog(dir.path()).unwrap();
    assert_eq!(
        outcome,
        CatalogWrite::Replaced {
            old_len: Some(edited.len() as u64),
            new_len: CATALOG_BYTES.len() as u64,
        }
    );
    assert_eq!(fs::read(&target).unwrap(), CATALOG_BYTES);
    assert_eq!(mode(&target), 0o600);
    assert!(extra_entries(dir.path(), &[CATALOG_FILE_NAME]).is_empty());
}

/// Whether permission bits actually stop this process writing.
///
/// They do not for root, so a test relying on an unwritable directory would
/// pass for the wrong reason there. Checked by trying, not by asking the uid.
fn permissions_are_enforced(dir: &Path) -> bool {
    let probe = dir.join("probe");
    match fs::write(&probe, b"x") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            false
        }
        Err(_) => true,
    }
}

#[test]
fn an_unwritable_directory_is_an_error_and_leaves_no_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    chmod(dir.path(), 0o500);
    if !permissions_are_enforced(dir.path()) {
        chmod(dir.path(), 0o700);
        eprintln!("skipped: this process can write a 0500 directory (running as root?)");
        return;
    }

    let result = write_catalog(dir.path());
    let entries = extra_entries(dir.path(), &[]);
    chmod(dir.path(), 0o700);

    let err = result.expect_err("writing into a read-only directory must fail");
    assert!(
        err.to_string().contains(&dir.path().display().to_string()),
        "the error names the path: {err}"
    );
    assert!(
        entries.is_empty(),
        "no partial or temporary file: {entries:?}"
    );
}

#[test]
fn the_embedded_descriptor_is_one_engram_accepts() {
    let d: serde_json::Value = serde_json::from_slice(CATALOG_BYTES)
        .expect("an unparseable descriptor fails engram's whole fleet capture run");
    assert_eq!(d["schema_version"], 1, "unsupported schema version");
    assert_eq!(
        d["module_id"], "fusiform",
        "module_id must match the module's directory name"
    );
    let entries = d["entries"].as_array().expect("entries is an array");
    assert!(
        !entries.is_empty(),
        "an enrollment with no entries captures nothing"
    );
    assert_eq!(
        entries[0]["path"], STORE_FILE_NAME,
        "the entry must name the database the descriptor is written beside"
    );
}

fn open_store(dir: &Path) -> CatalogStore {
    CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.join(STORE_FILE_NAME).to_string_lossy().to_string(),
        },
    })
    .unwrap()
}

/// The startup path: the descriptor lands beside the database the store
/// actually opened, and health stays Ok.
#[test]
fn startup_writes_the_descriptor_beside_the_opened_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path());
    let signals = Signals::new();
    signals.store_opened();

    write_beside_store(store.path(), &signals);

    assert_eq!(
        fs::read(dir.path().join(CATALOG_FILE_NAME)).unwrap(),
        CATALOG_BYTES
    );
    assert!(!signals.catalog_write_did_fail());
    assert_eq!(health::report(&signals, 1_000).status, HealthStatus::Ok);
}

/// A failed write at startup does not panic or return an error the daemon
/// could stop on, and health reports Degraded naming the catalog.
#[test]
fn a_failed_startup_write_makes_health_degraded_not_failing() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path());
    let signals = Signals::new();
    signals.store_opened();

    chmod(dir.path(), 0o500);
    if !permissions_are_enforced(dir.path()) {
        chmod(dir.path(), 0o700);
        eprintln!("skipped: this process can write a 0500 directory (running as root?)");
        return;
    }
    write_beside_store(store.path(), &signals);
    chmod(dir.path(), 0o700);

    assert!(signals.catalog_write_did_fail());
    let r = health::report(&signals, 1_000);
    assert_eq!(r.status, HealthStatus::Degraded);
    let detail = r.detail.unwrap_or_default();
    assert!(detail.contains(CATALOG_FILE_NAME), "{detail}");
}
