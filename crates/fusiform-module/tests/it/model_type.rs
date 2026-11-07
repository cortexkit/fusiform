use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{normalize_models_dev, BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::serve_catalog_get;
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};
use serde_json::json;

fn ingest(store: &CatalogStore, doc: &serde_json::Value, at: i64) {
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
    let catalog = normalize_models_dev(&serde_json::to_vec(doc).unwrap())
        .unwrap()
        .catalog;
    let plan = plan_ingest(
        store,
        &catalog,
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
    let mut doc = json!({"p":{"models":{
        "omitted":{"type":"transcription"},
        "null":{"type":"transcription"},
        "unlabelled":{}
    }}});
    ingest(&store, &doc, 1_000);
    doc["p"]["models"]["omitted"] = json!({});
    doc["p"]["models"]["null"]["type"] = serde_json::Value::Null;
    ingest(&store, &doc, 2_000);

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
