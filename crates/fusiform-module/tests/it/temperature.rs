use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::serve_catalog_get;
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};
use serde_json::{json, Value};

const ESTABLISHED: i64 = 1794441600000;

#[test]
fn temperature_corrections_cover_exactly_the_seed_google_gemini_36_and_newer_rows() {
    let rows: Vec<_> = fusiform_module::fact_overrides::overrides()
        .values()
        .filter(|row| row.fact_key == "capability.temperature")
        .collect();
    assert_eq!(rows.len(), 5);
    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    let ids = [
        "gemini-3.6-flash",
        "gemini-3.7-flash",
        "gemini-3.8-flash",
        "gemini-4-argon",
        "gemini-4-carbon",
    ];
    for id in ids {
        let row = rows
            .iter()
            .find(|row| row.provider_id == "google" && row.model_id == id)
            .unwrap();
        assert_eq!(row.replaces, true);
        assert_eq!(row.value, false);
        assert_eq!(seed["google"]["models"][id]["temperature"], true);
    }
    let upstream = json!({"google":{"models":ids.into_iter().map(|id| (id.to_string(), json!({"temperature":true}))).collect::<serde_json::Map<_, _>>()}});
    let (store, _dir) = store(upstream);
    for id in ids {
        let response = get(&store, json!({"provider_id":"google","model_id":id}));
        let facts = &response["models"][format!("google/{id}")];
        assert_eq!(facts["capability.temperature"], false);
        assert!(facts.get("limit.temperature_min").is_none());
    }
}

pub(super) fn store(document: Value) -> (CatalogStore, tempfile::TempDir) {
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
            normalized_hash: Some("temperature".into()),
            raw_hash: Some("temperature".into()),
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    ingest(&store, document, 1_000);
    (store, dir)
}

fn ingest(store: &CatalogStore, document: Value, at: i64) {
    let catalog = normalize_models_dev(&serde_json::to_vec(&document).unwrap())
        .unwrap()
        .catalog;
    let plan = plan_ingest(store, &catalog, Timestamp(at), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
}

fn get(store: &CatalogStore, args: Value) -> Value {
    serde_json::to_value(serve_catalog_get(store, &serde_json::to_vec(&args).unwrap()).unwrap())
        .unwrap()
}

#[test]
fn temperature_published_boolean_reaches_wire_and_absence_stays_unknown() {
    let (store, _dir) = store(json!({"acme":{"models":{
        "unknown":{}, "supported":{"temperature":true}, "unsupported":{"temperature":false}
    }}}));
    let response = get(
        &store,
        json!({"provider_id":"acme", "fact_prefixes":["capability.temperature"]}),
    );
    assert_eq!(
        response["models"]["acme/supported"]["capability.temperature"],
        true
    );
    assert_eq!(
        response["models"]["acme/unsupported"]["capability.temperature"],
        false
    );
    assert!(response["models"]
        .as_object()
        .unwrap()
        .values()
        .all(|facts| facts.get("capability.temperature") != Some(&Value::Null)));
    assert!(response["models"]
        .get("acme/unknown")
        .is_none_or(|facts| facts.get("capability.temperature").is_none()));
}

#[test]
fn temperature_ranges_are_current_first_party_and_guarded_before_prefix_filtering() {
    let (store, _dir) = store(json!({
        "openai":{"models":{"gpt-4.1":{"temperature":true}, "unknown":{}, "none":{"temperature":false}}},
        "gateway":{"models":{"gpt-4.1":{"temperature":true}}}
    }));
    for prefixes in [json!(["limit."]), json!(["limit.temperature_"])] {
        let response = get(&store, json!({"fact_prefixes":prefixes}));
        assert_eq!(
            response["models"]["openai/gpt-4.1"]["limit.temperature_min"],
            0
        );
        assert_eq!(
            response["models"]["openai/gpt-4.1"]["limit.temperature_max"],
            2
        );
        assert_eq!(
            response["models"]["openai/unknown"]["limit.temperature_max"],
            2
        );
        for id in ["openai/none", "gateway/gpt-4.1"] {
            assert!(response["models"][id]
                .get("limit.temperature_max")
                .is_none());
        }
    }
    let historical = get(
        &store,
        json!({"provider_id":"openai","model_id":"gpt-4.1","at_ms":1_000}),
    );
    assert!(historical["models"]["openai/gpt-4.1"]
        .get("limit.temperature_max")
        .is_none());
}

#[test]
fn temperature_google_correction_is_dated_and_stops_when_upstream_moves() {
    let document =
        |temperature| json!({"google":{"models":{"gemini-4-carbon":{"temperature":temperature}}}});
    let (store, _dir) = store(document(true));
    fusiform_module::fact_overrides::apply_to_store(
        &store,
        fusiform_module::fact_overrides::overrides(),
        ESTABLISHED,
    )
    .unwrap();
    let args = json!({"provider_id":"google","model_id":"gemini-4-carbon"});
    let current = get(&store, args.clone());
    assert_eq!(
        current["models"]["google/gemini-4-carbon"]["capability.temperature"],
        false
    );
    assert!(current["models"]["google/gemini-4-carbon"]
        .get("limit.temperature_max")
        .is_none());
    assert!(current["fact_overrides"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["fact_key"] == "capability.temperature"));
    let mut historical = args.clone();
    historical["at_ms"] = json!(ESTABLISHED - 1);
    assert_eq!(
        get(&store, historical.clone())["models"]["google/gemini-4-carbon"]
            ["capability.temperature"],
        true
    );
    historical["at_ms"] = json!(ESTABLISHED);
    assert_eq!(
        get(&store, historical.clone())["models"]["google/gemini-4-carbon"]
            ["capability.temperature"],
        false
    );
    ingest(&store, document(false), ESTABLISHED + 1);
    historical["at_ms"] = json!(ESTABLISHED + 1);
    for response in [get(&store, args), get(&store, historical)] {
        assert!(!response["fact_overrides"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|row| row["fact_key"] == "capability.temperature"));
    }
}

#[test]
fn temperature_aliases_inherit_only_safe_false_and_never_ranges() {
    let (store, _dir) = store(json!({
        "anthropic":{"models":{"claude-sonnet-5-5":{"temperature":false}, "claude-opus-5-5":{"temperature":false}}},
        "google":{"models":{"gemini-4-carbon":{"temperature":true}, "gemini-4-argon":{"temperature":true}}}
    }));
    for alias in [
        "antigravity-claude-sonnet-5-5",
        "antigravity-claude-opus-5-5",
        "antigravity-gemini-4-carbon",
    ] {
        let response = get(&store, json!({"provider_id":"google","model_id":alias}));
        let facts = &response["models"][format!("google/{alias}")];
        assert_eq!(facts["capability.temperature"], false, "{alias}");
        assert!(facts.get("limit.temperature_max").is_none());
    }
    ingest(
        &store,
        json!({"google":{"models":{"gemini-3.1-flash-image":{"temperature":true}}}}),
        ESTABLISHED,
    );
    let response = get(
        &store,
        json!({"provider_id":"google","model_id":"antigravity-gemini-3.1-flash-image"}),
    );
    let facts = &response["models"]["google/antigravity-gemini-3.1-flash-image"];
    assert!(facts.get("capability.temperature").is_none());
    assert!(facts.get("limit.temperature_max").is_none());
}
