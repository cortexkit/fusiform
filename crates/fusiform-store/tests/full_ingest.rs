//! Ingest a whole live document into a real store.
//!
//! The excerpt tests prove the rules on shapes someone selected. This runs the
//! shipped ingest over every row of a complete fetch, which is the instrument
//! that has found every real defect in this repository so far.
//!
//! Skipped when the payload is absent, so the suite stays runnable without a
//! capture.

use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};

fn payload() -> Option<Vec<u8>> {
    let path = std::env::var("FUSIFORM_FULL_PAYLOAD").ok()?;
    std::fs::read(path).ok()
}

fn store(dir: &tempfile::TempDir) -> CatalogStore {
    let path = dir.path().join("store.db");
    CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_string_lossy().to_string(),
        },
    })
    .expect("store opens")
}

fn observe(s: &CatalogStore, at: i64, outcome: ObservationOutcome) -> i64 {
    s.record_observation(&NewObservation {
        source: SourceId::ModelsDev,
        observed_at: Timestamp(at),
        outcome,
        normalized_hash: Some(format!("h{at}")),
        raw_hash: None,
        etag: None,
        duration_ms: None,
        detail: None,
    })
    .unwrap()
}

/// A whole document seeds, and re-ingesting it writes nothing.
///
/// The second half is the property that keeps the era table finite. Polling
/// every 30 minutes for a year is 17,520 observations; if an unchanged document
/// wrote even one era per model, that is 109 million rows describing no change.
#[test]
fn a_whole_document_seeds_once_and_is_then_stable() {
    let Some(bytes) = payload() else {
        eprintln!("skipped: set FUSIFORM_FULL_PAYLOAD to a captured models.dev document");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let catalog = normalize_models_dev(&bytes)
        .expect("live document normalizes")
        .catalog;

    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();

    assert_eq!(
        plan.new_models,
        catalog.model_count(),
        "every model in a first ingest is new"
    );
    assert_eq!(plan.changed_facts, 0);
    assert_eq!(plan.disappeared_models, 0);

    let written = store.append_eras(&plan.eras).unwrap();
    eprintln!(
        "seeded {} models as {written} eras ({:.1} facts per model)",
        plan.new_models,
        written as f64 / plan.new_models as f64
    );

    // The same document again: nothing moved, so nothing is written.
    observe(&store, 2_000, ObservationOutcome::Unchanged);
    let again = plan_ingest(
        &store,
        &catalog,
        Timestamp(3_000),
        BoundaryKind::Observed,
        None,
    )
    .unwrap();
    assert!(
        again.is_empty(),
        "re-ingesting an unchanged document wrote {} eras",
        again.eras.len()
    );
}

/// A single rate change in a 3.6 MB document opens exactly one era.
///
/// Runs the real diff over every row, so a rule that accidentally treats an
/// untouched field as changed shows up as a count far above one rather than as
/// a subtle wrong value.
#[test]
fn one_changed_rate_in_a_whole_document_opens_one_era() {
    let Some(bytes) = payload() else {
        eprintln!("skipped: set FUSIFORM_FULL_PAYLOAD to a captured models.dev document");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);

    let catalog = normalize_models_dev(&bytes).unwrap().catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    observe(&store, 2_000, ObservationOutcome::Unchanged);
    let detecting = observe(
        &store,
        3_000,
        ObservationOutcome::Changed { snapshot_seq: 1 },
    );

    // Move one rate on one model, through the parsed form so the mutation does
    // not depend on the document's whitespace.
    let mut doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let cost = doc
        .get_mut("anthropic")
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.get_mut("claude-sonnet-4-5"))
        .and_then(|m| m.get_mut("cost"))
        .and_then(|c| c.as_object_mut())
        .expect("the fixture model must exist in the live document");
    let before = cost.get("input").cloned().unwrap();
    cost.insert("input".to_string(), serde_json::json!(99));
    assert_ne!(
        before,
        serde_json::json!(99),
        "the mutation must change the value"
    );
    let mutated = serde_json::to_vec(&doc).unwrap();

    let changed = normalize_models_dev(&mutated).unwrap().catalog;
    let plan = plan_ingest(
        &store,
        &changed,
        Timestamp(3_000),
        BoundaryKind::Observed,
        Some(detecting),
    )
    .unwrap();

    assert_eq!(
        plan.eras.len(),
        1,
        "one rate moved; got {} eras: {:?}",
        plan.eras.len(),
        plan.eras
            .iter()
            .take(10)
            .map(|e| format!("{}/{} {}", e.provider_id, e.model_id, e.fact_key.as_str()))
            .collect::<Vec<_>>()
    );
    assert_eq!(plan.changed_facts, 1);
    assert_eq!(plan.new_models, 0);
    assert_eq!(plan.disappeared_models, 0);
}

/// Serializing and reparsing a document is not a change.
///
/// The reason the diff compares normalized values rather than bytes. A whole
/// document round-tripped through a JSON library has different bytes, different
/// key order and different number rendering, and none of that is a fact about
/// any model.
#[test]
fn reserializing_a_document_is_not_a_change() {
    let Some(bytes) = payload() else {
        eprintln!("skipped: set FUSIFORM_FULL_PAYLOAD to a captured models.dev document");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);

    let catalog = normalize_models_dev(&bytes).unwrap().catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    observe(&store, 2_000, ObservationOutcome::Unchanged);

    // Round-trip through serde: same facts, different bytes.
    let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let reserialized = serde_json::to_vec(&doc).unwrap();
    assert_ne!(reserialized, bytes, "the round trip must change the bytes");

    let same = normalize_models_dev(&reserialized).unwrap().catalog;
    let plan = plan_ingest(
        &store,
        &same,
        Timestamp(3_000),
        BoundaryKind::Observed,
        None,
    )
    .unwrap();

    assert!(
        plan.is_empty(),
        "a reserialized document wrote {} eras; the diff is comparing bytes, not facts",
        plan.eras.len()
    );
}
