//! Cache lifetimes through current, historical and billing-plane reads.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::serve_catalog_get;
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};
use serde_json::{json, Value};

const SEED: &str = include_str!("../../data/models-dev-seed.json");

pub(super) fn store(mismatch: Option<(&str, &str)>) -> (CatalogStore, tempfile::TempDir) {
    let mut doc: Value = serde_json::from_str(SEED).unwrap();
    doc.as_object_mut()
        .unwrap()
        .retain(|provider, _| provider == "openai" || provider == "anthropic");
    if let Some((provider, model)) = mismatch {
        doc[provider]["models"][model]["cost"]["cache_write"] = json!(99);
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
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1000),
            outcome: ObservationOutcome::Seeded,
            normalized_hash: Some("h".into()),
            raw_hash: Some("r".into()),
            etag: None,
            duration_ms: Some(1),
            detail: None,
        })
        .unwrap();
    let catalog = normalize_models_dev(doc.to_string().as_bytes())
        .unwrap()
        .catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

fn get(store: &CatalogStore, args: Value) -> Value {
    serde_json::to_value(serve_catalog_get(store, &serde_json::to_vec(&args).unwrap()).unwrap())
        .unwrap()
}

fn price(facts: &Value, key: &str, units: i64) {
    assert_eq!(facts[key]["state"], "priced", "{key}: {}", facts[key]);
    assert_eq!(facts[key]["units"], units, "{key}");
    assert_eq!(facts[key]["exponent"], 9, "{key}");
    assert_eq!(
        facts[key]["unit_provenance"],
        facts["rate.cache_write"]["unit_provenance"]
    );
    assert!(facts[key].get("inherited_from").is_none());
}

#[test]
fn exact_cache_lifetime_prices_and_context_tiers() {
    let (store, _dir) = store(None);
    let before = (
        store.era_count(SourceId::ModelsDev).unwrap(),
        store.catalog_version().unwrap(),
    );
    let response = get(&store, json!({}));
    let opus = &response["models"]["anthropic/claude-opus-5-5"];
    assert_eq!(opus["capability.cache_lifetimes"], json!(["5m", "1h"]));
    price(opus, "rate.cache_write.ttl.5m", 5_000_000_000);
    price(opus, "rate.cache_write.ttl.1h", 8_000_000_000);
    let sol = &response["models"]["openai/gpt-6.1-sol"];
    assert_eq!(sol["capability.cache_lifetimes"], json!(["30m", "12h"]));
    for (key, units) in [
        ("rate.cache_write.ttl.30m", 2_500_000_000),
        ("rate.cache_write.ttl.12h", 5_000_000_000),
        (
            "rate.cache_write.above_context.272000.ttl.30m",
            5_000_000_000,
        ),
        (
            "rate.cache_write.above_context.272000.ttl.12h",
            10_000_000_000,
        ),
    ] {
        price(sol, key, units);
    }
    let haiku = &response["models"]["anthropic/claude-haiku-5-5"];
    price(
        haiku,
        "rate.cache_write.above_context.100000.ttl.5m",
        625_000_000,
    );
    price(
        haiku,
        "rate.cache_write.above_context.100000.ttl.1h",
        1_000_000_000,
    );
    assert_eq!(
        before,
        (
            store.era_count(SourceId::ModelsDev).unwrap(),
            store.catalog_version().unwrap()
        )
    );
    for (model, lifetimes) in [
        ("gpt-5.4", json!(["in_memory", "24h"])),
        ("gpt-5.5", json!(["24h"])),
    ] {
        let facts = &response["models"][format!("openai/{model}")];
        assert_eq!(facts["capability.cache_lifetimes"], lifetimes);
        assert!(!facts
            .as_object()
            .unwrap()
            .keys()
            .any(|key| key.contains(".ttl.")));
    }
    let mismatches: Vec<_> = response["models"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, facts)| {
            facts["capability.cache_lifetimes"]
                .as_array()
                .is_some_and(|ttls| {
                    ttls == &vec![json!("5m"), json!("1h")]
                        || ttls == &vec![json!("30m"), json!("12h")]
                })
                && !facts
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|key| key.contains(".ttl."))
        })
        .map(|(id, _)| id)
        .collect();
    assert!(
        mismatches.is_empty(),
        "short-write guard mismatches: {mismatches:?}"
    );
}

#[test]
fn cache_lifetime_guard_withholds_all_prices_but_keeps_lifetimes() {
    let (store, _dir) = store(Some(("openai", "gpt-6.1-sol")));
    let response = get(
        &store,
        json!({"provider_id":"openai", "model_id":"gpt-6.1-sol"}),
    );
    let facts = &response["models"]["openai/gpt-6.1-sol"];
    assert_eq!(facts["capability.cache_lifetimes"], json!(["30m", "12h"]));
    assert_eq!(facts["rate.cache_write"]["units"], 99_000_000_000_i64);
    assert!(
        !facts
            .as_object()
            .unwrap()
            .keys()
            .any(|key| key.contains(".ttl.")),
        "mismatched row served TTL prices: {facts}"
    );
}

#[test]
fn cache_lifetimes_are_current_only_and_filters_retain_price_inputs() {
    let (store, _dir) = store(None);
    let historical = get(
        &store,
        json!({"provider_id":"openai", "model_id":"gpt-6.1-sol", "at_ms":1000}),
    );
    assert!(!historical["models"]["openai/gpt-6.1-sol"]
        .as_object()
        .unwrap()
        .keys()
        .any(|key| key.contains(".ttl.") || key == "capability.cache_lifetimes"));
    for prefix in [
        "capability.cache_lifetimes",
        "rate.cache_write.ttl.12h",
        "rate.cache_write.above_context.272000.ttl.12h",
    ] {
        for named in [false, true] {
            let mut args = json!({"provider_id":"openai", "fact_prefixes":[prefix]});
            if named {
                args["model_id"] = json!("gpt-6.1-sol");
            }
            let response = get(&store, args);
            let facts = response["models"]["openai/gpt-6.1-sol"]
                .as_object()
                .unwrap();
            assert_eq!(facts.len(), 1, "{prefix}: {facts:?}");
            assert!(facts.contains_key(prefix));
        }
    }
}

#[test]
fn cache_lifetime_billing_planes_strip_chatgpt_and_carry_oauth_and_apikey() {
    let (store, _dir) = store(None);
    for (provider, model, auth, ttl) in [
        ("openai", "gpt-6.1-sol", "apikey", "12h"),
        ("anthropic", "claude-opus-5-5", "apikey", "1h"),
        ("anthropic", "claude-opus-5-5", "oauth", "1h"),
    ] {
        let response = get(
            &store,
            json!({"provider_id":provider, "model_id":model, "auth_method":auth}),
        );
        let facts = &response["models"][format!("{provider}/{model}")];
        let rate = &facts[format!("rate.cache_write.ttl.{ttl}")];
        assert_eq!(rate["state"], "priced", "{response}");
        assert!(facts.get("capability.cache_lifetimes").is_some());
        if auth == "oauth" {
            assert_eq!(response["plane"]["kind"], "quota_proxy");
            assert_eq!(rate["inherited_from"]["basis"], "quota_proxy");
            assert_eq!(
                rate["inherited_from"],
                facts["rate.cache_write"]["inherited_from"]
            );
        } else {
            assert!(rate.get("inherited_from").is_none());
        }
    }
    let response = get(
        &store,
        json!({"provider_id":"openai", "model_id":"gpt-6.1-sol", "auth_method":"chatgpt"}),
    );
    let facts = response["models"]["openai/gpt-6.1-sol"]
        .as_object()
        .unwrap();
    assert!(!facts
        .keys()
        .any(|key| key.contains(".ttl.") || key == "capability.cache_lifetimes"));
    assert_eq!(facts["rate.cache_write"]["state"], "billed_as");
    assert_eq!(facts["rate.cache_write"]["key"], "rate.input");
}
