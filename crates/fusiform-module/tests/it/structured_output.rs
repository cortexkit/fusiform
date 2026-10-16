use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::{serve_tool_call, ToolResponse};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};

const UPSTREAM: &str = r#"{
  "acme": {
    "id": "acme",
    "name": "Acme",
    "models": {
      "unknown": { "id": "unknown" },
      "supported": { "id": "supported", "structured_output": true },
      "unsupported": { "id": "unsupported", "structured_output": false }
    }
  }
}"#;

fn store() -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    })
    .unwrap();
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1_000),
            outcome: ObservationOutcome::Seeded,
            normalized_hash: Some("normalized".into()),
            raw_hash: Some("raw".into()),
            etag: None,
            duration_ms: Some(10),
            detail: None,
        })
        .unwrap();
    let catalog = normalize_models_dev(UPSTREAM.as_bytes())
        .expect("the upstream document must normalize")
        .catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None)
        .expect("the catalog must plan for ingestion");
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

fn read(store: &CatalogStore, model_id: &str) -> serde_json::Value {
    let body = format!(
        r#"{{"name":"catalog.get","arguments":{{"provider_id":"acme","model_id":"{model_id}","fact_prefixes":["capability.structured_output"]}}}}"#
    );
    let response = match serve_tool_call(store, body.as_bytes()).expect("must serve") {
        ToolResponse::Catalog(response) => serde_json::to_value(response).unwrap(),
        other => panic!("expected a catalog response, got {other:?}"),
    };
    response["models"][format!("acme/{model_id}")].clone()
}

/// The served fact is the upstream boolean, and an absent key creates no fact.
#[test]
fn published_boolean_is_served_and_absence_stays_unknown() {
    let (store, _dir) = store();

    let unknown = read(&store, "unknown");
    let supported = read(&store, "supported");
    let unsupported = read(&store, "unsupported");

    assert!(
        !unknown
            .as_object()
            .expect("model facts are an object")
            .contains_key("capability.structured_output"),
        "no structured_output fact is written when models.dev omits the key"
    );
    assert_eq!(supported["capability.structured_output"], true);
    assert_eq!(unsupported["capability.structured_output"], false);
}
