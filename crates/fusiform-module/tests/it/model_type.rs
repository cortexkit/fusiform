use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::NormalizedCatalog;
use fusiform_core::{normalize_models_dev, BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::serve_catalog_get;
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};
use serde_json::json;

fn ingest(store: &CatalogStore, catalog: &NormalizedCatalog, at: i64) {
    let observation = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(at),
            outcome: ObservationOutcome::Changed { snapshot_seq: at },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let plan = plan_ingest(
        store,
        catalog,
        Timestamp(at),
        if at == 1_000 {
            BoundaryKind::Seed
        } else {
            BoundaryKind::Observed
        },
        if at == 1_000 { None } else { Some(observation) },
    )
    .unwrap();
    store.append_eras(&plan.eras).unwrap();
}

#[test]
fn missing_null_and_withdrawn_types_are_absent_but_history_keeps_labels() {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    let mut catalog = normalize_models_dev(
        br#"{"p":{"models":{
        "omitted":{"type":"transcription","api":{},"capabilities":{}},
        "null":{"type":"transcription","api":{},"capabilities":{}},
        "unlabelled":{"type":"chat","api":{},"capabilities":{}}
    }}}"#,
    )
    .unwrap()
    .catalog;
    // Use historical normalized rows for missing and null types: models.dev
    // v2 requires a type and refuses an unreadable one rather than withdrawing it.
    for model in &mut catalog.providers[0].models {
        if model.key.model_id == "unlabelled" {
            model.model_type = None;
        }
    }
    ingest(&store, &catalog, 1_000);
    for model in &mut catalog.providers[0].models {
        model.model_type = None;
    }
    ingest(&store, &catalog, 2_000);

    for args in [json!({}), json!({"at_ms":2_000})] {
        let got = serve_catalog_get(&store, &serde_json::to_vec(&args).unwrap()).unwrap();
        for id in ["p/omitted", "p/null", "p/unlabelled"] {
            assert!(!got.models[id].contains_key("model.type"), "{id}");
        }
    }
    let past = serve_catalog_get(&store, br#"{"provider_id":"p","at_ms":1000}"#).unwrap();
    for id in ["p/omitted", "p/null"] {
        assert_eq!(past.models[id]["model.type"], "transcription");
    }
    assert!(!past.models["p/unlabelled"].contains_key("model.type"));
}
