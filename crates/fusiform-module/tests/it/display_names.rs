use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::{serve_catalog_get, CatalogGetResponse};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};
use serde_json::{json, Value};

const UPSTREAM: &[u8] = br#"{
    "openai": {"name":"OpenAI", "models":{"gpt-6.1-sol":{"name":"GPT-6.1 Sol"}}},
    "unknown": {"models":{"none":{}}},
    "empty": {"name":"", "models":{"none":{"name":""}}},
    "spaces": {"name":" Provider ", "models":{"model":{"name":" Model "}}},
    "meta": {"name":"Meta", "models":{"other":{"name":"Other"}}},
    "cloudflare-workers-ai": {"name":"Cloudflare Workers AI", "models":{"other":{}}},
    "google": {"name":"Google", "models":{"other":{}}},
    "anthropic": {"name":"Anthropic", "models":{"claude-sonnet-5-5":{"name":"Target only"}}}
}"#;

fn empty_store() -> (CatalogStore, tempfile::TempDir) {
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
    (store, dir)
}

fn store() -> (CatalogStore, tempfile::TempDir) {
    let (store, dir) = empty_store();
    ingest(&store, UPSTREAM, 1_000);
    (store, dir)
}

fn ingest(store: &CatalogStore, bytes: &[u8], at: i64) {
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
    let catalog = normalize_models_dev(bytes).unwrap().catalog;
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

fn get(store: &CatalogStore, args: Value) -> CatalogGetResponse {
    serve_catalog_get(store, &serde_json::to_vec(&args).unwrap()).unwrap()
}

#[test]
fn upstream_display_names_are_served_verbatim() {
    let (store, _dir) = store();
    let got = get(&store, json!({}));
    assert_eq!(
        got.models["openai/gpt-6.1-sol"]["model.name"],
        "GPT-6.1 Sol"
    );
    assert_eq!(got.models["openai/gpt-6.1-sol"]["provider.name"], "OpenAI");
    assert_eq!(got.models["spaces/model"]["model.name"], " Model ");
    assert_eq!(got.models["spaces/model"]["provider.name"], " Provider ");
    let filtered = get(&store, json!({"fact_prefixes":["provider."]}));
    assert_eq!(
        filtered.models["openai/gpt-6.1-sol"],
        std::collections::BTreeMap::from([("provider.name".to_string(), json!("OpenAI")),])
    );
    let plane = get(
        &store,
        json!({"provider_id":"openai", "auth_method":"apikey", "fact_prefixes":["provider."]}),
    );
    assert_eq!(
        plane.models["openai/gpt-6.1-sol"]["provider.name"],
        "OpenAI"
    );
}

#[test]
fn missing_and_empty_upstream_names_are_absent_not_empty_or_null() {
    let (store, _dir) = store();
    let got = get(&store, json!({}));
    for id in ["unknown/none", "empty/none"] {
        assert!(!got.models[id].contains_key("model.name"));
        assert!(!got.models[id].contains_key("provider.name"));
    }
}

#[test]
fn renames_and_withdrawals_preserve_point_in_time_names() {
    let (store, _dir) = store();
    let mut doc: Value = serde_json::from_slice(UPSTREAM).unwrap();
    doc["openai"]["name"] = json!("Renamed provider");
    doc["openai"]["models"]["gpt-6.1-sol"]["name"] = json!("Renamed model");
    ingest(&store, &serde_json::to_vec(&doc).unwrap(), 2_000);
    doc["openai"]["name"] = json!("");
    doc["openai"]["models"]["gpt-6.1-sol"]
        .as_object_mut()
        .unwrap()
        .remove("name");
    ingest(&store, &serde_json::to_vec(&doc).unwrap(), 3_000);
    for (at, model, provider) in [
        (1_000, "GPT-6.1 Sol", "OpenAI"),
        (2_000, "Renamed model", "Renamed provider"),
    ] {
        let got = get(
            &store,
            json!({"provider_id":"openai", "model_id":"gpt-6.1-sol", "at_ms":at}),
        );
        assert_eq!(got.models["openai/gpt-6.1-sol"]["model.name"], model);
        assert_eq!(got.models["openai/gpt-6.1-sol"]["provider.name"], provider);
    }
    for args in [json!({}), json!({"at_ms":3_000})] {
        let got = get(&store, args);
        assert!(!got.models["openai/gpt-6.1-sol"].contains_key("model.name"));
        assert!(!got.models["openai/gpt-6.1-sol"].contains_key("provider.name"));
    }
}

#[test]
fn curated_names_and_typesafe_provider_are_sourced_labels() {
    let (store, _dir) = store();
    let got = get(&store, json!({}));
    for (id, name, provider) in [
        ("meta/muse-wonder-full-1.0", "Muse Wonder Full", "Meta"),
        (
            "cloudflare-workers-ai/@cf/cloudflare/clef",
            "Clef",
            "Cloudflare Workers AI",
        ),
        (
            "cloudflare-workers-ai/@cf/cloudflare/clef-flash",
            "Clef-flash",
            "Cloudflare Workers AI",
        ),
        (
            "cloudflare-workers-ai/@cf/cloudflare/clairic",
            "Clairic",
            "Cloudflare Workers AI",
        ),
        ("typesafe/jev-1.13.0", "Jev 1.13", "TypeSafe"),
    ] {
        assert_eq!(got.models[id]["model.name"], name);
        assert_eq!(got.models[id]["provider.name"], provider);
        let named = get(
            &store,
            json!({"provider_id": id.split_once('/').unwrap().0, "model_id": id.split_once('/').unwrap().1}),
        );
        assert_eq!(named.models[id]["provider.name"], provider);
    }
}

#[test]
fn aliases_use_the_route_label_and_the_alias_provider_not_the_target() {
    let (store, _dir) = store();
    for args in [
        json!({"provider_id":"google", "model_id":"antigravity-claude-sonnet-5-5"}),
        json!({"provider_id":"google"}),
    ] {
        let got = get(&store, args);
        let alias = &got.models["google/antigravity-claude-sonnet-5-5"];
        assert_eq!(alias["model.name"], "Claude Sonnet 5.5 (Antigravity)");
        assert_eq!(alias["provider.name"], "Google");
    }
}

#[test]
fn embedded_seed_upgrade_records_exactly_the_published_names_as_first_values() {
    let seed = include_bytes!("../../data/models-dev-seed.json");
    let raw: Value = serde_json::from_slice(seed).unwrap();
    let mut expected = 0;
    let mut model_count = 0;
    for provider in raw.as_object().unwrap().values() {
        for model in provider["models"].as_object().unwrap().values() {
            model_count += 1;
            for name in [&provider["name"], &model["name"]] {
                expected += usize::from(name.as_str().is_some_and(|s| !s.is_empty()));
            }
        }
    }
    let catalog = normalize_models_dev(seed).unwrap().catalog;
    let mut old_catalog = catalog.clone();
    for provider in &mut old_catalog.providers {
        for model in &mut provider.models {
            model.display_name = None;
            model.provider_name = None;
        }
    }
    let (store, _dir) = empty_store();
    let baseline = plan_ingest(
        &store,
        &old_catalog,
        Timestamp(1_000),
        BoundaryKind::Seed,
        None,
    )
    .unwrap();
    store.append_eras(&baseline.eras).unwrap();
    let observation = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(2_000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let upgrade = plan_ingest(
        &store,
        &catalog,
        Timestamp(2_000),
        BoundaryKind::Observed,
        Some(observation),
    )
    .unwrap();
    assert!(
        expected > 17_000,
        "the embedded seed must exercise the real population"
    );
    assert_eq!(upgrade.eras.len(), expected);
    assert!(upgrade
        .eras
        .iter()
        .all(|era| matches!(era.fact_key.as_str(), "model.name" | "provider.name")));
    println!(
        "embedded seed: {model_count} models; first-poll name eras: {}",
        upgrade.eras.len()
    );
}
