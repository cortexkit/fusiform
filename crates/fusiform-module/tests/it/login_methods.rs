use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, Timestamp};
use fusiform_store::{ingest::plan_ingest, CatalogStore};
use serde_json::{json, Value};

pub(crate) fn store() -> (CatalogStore, tempfile::TempDir) {
    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    let mut upstream = json!({"openai": seed["openai"].clone()});
    upstream["openai"]["models"] = json!({});
    for id in ["gpt-6.1-sol", "gpt-5.5", "gpt-4o"] {
        assert!(seed["openai"]["models"][id].is_object(), "{id}");
        upstream["openai"]["models"][id] = seed["openai"]["models"][id].clone();
    }
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
    let catalog = fusiform_core::normalize_models_dev(&serde_json::to_vec(&upstream).unwrap())
        .unwrap()
        .catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

fn get(store: &CatalogStore, args: Value) -> Value {
    serde_json::to_value(super::curated_wire::get(store, args)).unwrap()
}

#[test]
fn chatgpt_login_allowlist_discloses_and_refuses_only_established_rows() {
    let (store, _dir) = store();
    let version = store.catalog_version().unwrap();
    let plain = get(&store, json!({"provider_id":"openai"}));
    let rows = plain["login_methods"].as_array().expect("login disclosure");
    assert_eq!(rows.len(), 2);
    assert!(!plain["curated"].as_array().unwrap().iter().any(|row| {
        ["openai/gpt-6.1-sol", "openai/gpt-5.5", "openai/gpt-4o"]
            .contains(&row["model"].as_str().unwrap())
    }));
    for (id, methods) in [
        ("openai/gpt-6.1-sol", json!(["apikey", "chatgpt"])),
        ("openai/gpt-5.5", json!(["apikey"])),
    ] {
        let row = rows.iter().find(|row| row["model"] == id).unwrap();
        assert_eq!(row["auth_methods"], methods);
        assert_eq!(
            row["source_ref"],
            "https://developers.openai.com/codex/models"
        );
        assert!(row["review_by_ms"].as_i64().unwrap() > 0);
        assert_eq!(row.as_object().unwrap().len(), 4);
    }
    let plane = get(
        &store,
        json!({"provider_id":"openai", "auth_method":"chatgpt"}),
    );
    assert!(plane["models"].get("openai/gpt-6.1-sol").is_some());
    assert!(plane["models"].get("openai/gpt-5.5").is_none());
    assert!(plane["models"].get("openai/gpt-4o").is_some());
    assert_eq!(plane["login_methods"].as_array().unwrap().len(), 1);
    let refused = get(
        &store,
        json!({"provider_id":"openai", "model_id":"gpt-5.5", "auth_method":"chatgpt"}),
    );
    assert_eq!(refused["models"], json!({}));
    assert!(refused.get("login_methods").is_none());
    let api = get(
        &store,
        json!({"provider_id":"openai", "model_id":"gpt-5.5", "auth_method":"apikey"}),
    );
    assert!(api["models"].get("openai/gpt-5.5").is_some());
    let sol = get(
        &store,
        json!({"provider_id":"openai", "model_id":"gpt-6.1-sol", "auth_method":"chatgpt"}),
    );
    assert!(sol["models"].get("openai/gpt-6.1-sol").is_some());
    let unlisted = get(&store, json!({"provider_id":"openai", "model_id":"gpt-4o"}));
    assert!(unlisted.get("login_methods").is_none());
    assert_eq!(store.catalog_version().unwrap(), version);
}

#[test]
fn login_disclosure_survives_fact_filters_and_paging_but_not_historical_reads() {
    let (store, _dir) = store();
    for args in [
        json!({"provider_id":"openai", "model_id":"gpt-6.1-sol", "fact_prefixes":["rate."]}),
        json!({"provider_id":"openai", "model_id":"gpt-6.1-sol", "fact_prefixes":["missing."]}),
        json!({"provider_id":"openai", "auth_method":"chatgpt", "fact_prefixes":["rate."]}),
    ] {
        let response = get(&store, args);
        assert_eq!(response["login_methods"][0]["model"], "openai/gpt-6.1-sol");
    }
    let page = get(&store, json!({"paged":true, "fact_prefixes":["rate."]}));
    assert_eq!(page["login_methods"].as_array().unwrap().len(), 2);
    for row in page["login_methods"].as_array().unwrap() {
        assert!(page["models"].get(row["model"].as_str().unwrap()).is_some());
    }
    let historical = get(
        &store,
        json!({"provider_id":"openai", "model_id":"gpt-5.5", "at_ms":1000}),
    );
    assert!(historical["models"].get("openai/gpt-5.5").is_some());
    assert!(historical.get("login_methods").is_none());
}
