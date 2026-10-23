use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::{serve_tool_call, ToolResponse};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};

const UPSTREAM: &[u8] = br#"{
    "p": {"npm": "@sdk/provider", "api": "https://provider/v1/", "models": {
        "model": {"provider": {"npm": "@sdk/model", "api": "https://model/v1//", "shape": "responses"}},
        "provider": {}
    }},
    "unknown": {"models": {"none": {}}}
}"#;

fn store() -> (CatalogStore, tempfile::TempDir) {
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
    let catalog = normalize_models_dev(UPSTREAM).unwrap().catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

#[test]
fn published_wire_claims_reach_plain_reads_and_wire_namespace_reads() {
    let (store, _dir) = store();
    let curated = fusiform_module::curated_models::models();
    assert!(
        !curated.is_empty(),
        "the curated population must be exercised"
    );
    for row in curated.values() {
        assert!(!row.facts.keys().any(|key| key.starts_with("wire.")));
    }

    for arguments in [
        serde_json::json!({}),
        serde_json::json!({"fact_prefixes": ["wire."]}),
    ] {
        let request =
            serde_json::to_vec(&serde_json::json!({"name": "catalog.get", "arguments": arguments}))
                .unwrap();
        let ToolResponse::Catalog(response) = serve_tool_call(&store, &request).unwrap() else {
            panic!("expected catalog");
        };
        let model = &response.models["p/model"];
        assert_eq!(
            model["wire.npm"],
            serde_json::json!({"value": "@sdk/model", "from": "model"})
        );
        assert_eq!(
            model["wire.api"],
            serde_json::json!({"value": "https://model/v1//", "from": "model"})
        );
        assert_eq!(
            model["wire.shape"],
            serde_json::json!({"value": "responses", "from": "model"})
        );
        let provider = &response.models["p/provider"];
        assert_eq!(
            provider["wire.npm"],
            serde_json::json!({"value": "@sdk/provider", "from": "provider"})
        );
        assert_eq!(
            provider["wire.api"],
            serde_json::json!({"value": "https://provider/v1/", "from": "provider"})
        );
        assert!(!provider.contains_key("wire.shape"));
        if arguments == serde_json::json!({}) {
            assert!(response.models.contains_key("unknown/none"));
        }
        if let Some(unknown) = response.models.get("unknown/none") {
            assert!(!unknown.keys().any(|key| key.starts_with("wire.")));
        }
        for (provider, model) in curated.keys() {
            if let Some(facts) = response.models.get(&format!("{provider}/{model}")) {
                assert!(!facts.keys().any(|key| key.starts_with("wire.")));
            }
        }
    }
}

#[test]
fn withdrawn_wire_claims_are_absent_from_catalog_but_kept_in_history() {
    let (store, _dir) = store();
    let catalog = normalize_models_dev(
        br#"{
        "p": {"models": {"model": {}, "provider": {}}},
        "unknown": {"models": {"none": {}}}
    }"#,
    )
    .unwrap()
    .catalog;
    let observation = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(2_000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some("withdrawn".into()),
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let plan = plan_ingest(
        &store,
        &catalog,
        Timestamp(2_000),
        BoundaryKind::Observed,
        Some(observation),
    )
    .unwrap();
    assert_eq!(
        plan.eras.len(),
        5,
        "each previously published claim is withdrawn once"
    );
    store.append_eras(&plan.eras).unwrap();
    assert!(plan_ingest(
        &store,
        &catalog,
        Timestamp(3_000),
        BoundaryKind::Observed,
        None
    )
    .unwrap()
    .is_empty());
    for arguments in [
        serde_json::json!({"provider_id": "p", "model_id": "model"}),
        serde_json::json!({"fact_prefixes": ["wire."]}),
    ] {
        let request =
            serde_json::to_vec(&serde_json::json!({"name": "catalog.get", "arguments": arguments}))
                .unwrap();
        let ToolResponse::Catalog(response) = serve_tool_call(&store, &request).unwrap() else {
            panic!("expected catalog");
        };
        for model in ["p/model", "p/provider"] {
            if let Some(facts) = response.models.get(model) {
                assert!(!facts.keys().any(|key| key.starts_with("wire.")));
            }
        }
        if arguments.get("model_id").is_some() {
            assert!(response.models.contains_key("p/model"));
        }
    }
    for key in ["wire.npm", "wire.api", "wire.shape"] {
        let request = serde_json::to_vec(
            &serde_json::json!({"name": "catalog.history", "arguments": {
                "provider_id": "p", "model_id": "model", "fact_key": key
            }}),
        )
        .unwrap();
        let ToolResponse::History(history) = serve_tool_call(&store, &request).unwrap() else {
            panic!("expected history");
        };
        assert_eq!(history.eras.len(), 2);
        assert!(history
            .eras
            .iter()
            .any(|era| era.boundary_at_ms == 1_000 && era.value["from"] == "model"));
        assert!(history
            .eras
            .iter()
            .any(|era| era.boundary_at_ms == 2_000 && era.value.is_null()));
    }
}

#[test]
fn a_wire_claim_change_moves_the_digest_and_opens_one_era() {
    use fusiform_store::ingest::catalog_digest;
    let (store, _dir) = store();
    let before = normalize_models_dev(UPSTREAM).unwrap().catalog;
    let mut upstream: serde_json::Value = serde_json::from_slice(UPSTREAM).unwrap();
    upstream["p"]["models"]["model"]["provider"]["shape"] = serde_json::json!("completions");
    let changed = normalize_models_dev(&serde_json::to_vec(&upstream).unwrap())
        .unwrap()
        .catalog;
    assert_ne!(catalog_digest(&before), catalog_digest(&changed));
    let plan = plan_ingest(
        &store,
        &changed,
        Timestamp(2_000),
        BoundaryKind::Observed,
        None,
    )
    .unwrap();
    assert_eq!(plan.eras.len(), 1);
    assert_eq!(plan.eras[0].fact_key.as_str(), "wire.shape");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&plan.eras[0].value_json).unwrap(),
        serde_json::json!({"value": "completions", "from": "model"})
    );
}
