//! The design note's table list matches the schema it describes.
//!
//! # Why a test rather than care
//!
//! §9 of `docs/design/schema-and-store.md` named six tables. Four never
//! existed, and the list omitted `era` — the table holding every fact fusiform
//! serves. It had been a specification written before any code, and it sat
//! under a §0.1 row reading **built**, so nothing in the document told a reader
//! it was a plan.
//!
//! That is ASTRO's 2026-08-13 finding in a different artifact: a note cannot
//! say whether it describes an intention or the shipped system, and the reader
//! has no way to tell. Their prose was true about a crate and false about their
//! runtime; mine was true about a design and false about the database. In both
//! cases re-reading confirms it, because the sentence is well formed and
//! describes something that could exist.
//!
//! "Re-verify the notes more often" is attention, and attention fails on the
//! day nobody is watching. A table list is mechanically checkable, so it is
//! checked.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_store::CatalogStore;

/// Every table a store has once it has been written to.
///
/// Written to, not merely opened: the store crate creates `cortexkit_fence` on
/// the FIRST FENCED WRITE rather than at open, measured with a probe. A fixture
/// that only opens would compare the note against a state no running module is
/// ever in, and would report the fence table as a phantom in the note.
fn tables_in_a_real_store() -> Vec<String> {
    use fusiform_core::{ObservationOutcome, SourceId, Timestamp};
    use fusiform_store::NewObservation;

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
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1_000),
            outcome: ObservationOutcome::NotModified,
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: Some(1),
            detail: None,
        })
        .unwrap();
    // Drop the store so the lease is released before opening a read connection.
    drop(store);

    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='table' \
             AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap();
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    names
}

#[test]
fn the_design_note_names_the_tables_that_exist() {
    let note = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/design/schema-and-store.md"),
    )
    .expect("the design note is in the repository");

    // The PARAGRAPH under §9 that states the table list, not its first line.
    // The note is hard-wrapped, so the list spans two lines and reading one of
    // them silently drops half the names — which the first version of this test
    // did, reporting a real table as missing from a note that names it.
    let paragraph = note
        .split("\n\n")
        .find(|p| p.trim_start().starts_with("Tables, as shipped:"))
        .expect("§9 states a table list beginning 'Tables, as shipped:'");
    let line = &paragraph.replace('\n', " ");

    for table in tables_in_a_real_store() {
        assert!(
            line.contains(&format!("`{table}`")),
            "the store creates `{table}` and the design note's §9 does not name it.\n\
             Note says: {line}\n\
             A reader takes that list for the schema, and §0.1 marks §9 as built."
        );
    }

    // And the reverse: a name in the note that no longer exists. Without this,
    // a dropped table leaves a phantom in the document forever — which is the
    // half of the defect that was actually present, four times over.
    let real = tables_in_a_real_store();
    for word in line.split('`').skip(1).step_by(2) {
        assert!(
            real.iter().any(|t| t == word),
            "the design note's §9 names `{word}`, which the store does not create.\n\
             Real tables: {real:?}"
        );
    }
}
