//! Attachment bounds are current provider evidence, not stored model claims.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::serve_catalog_get;
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};
use serde_json::{json, Value};

const KEYS: &[&str] = &[
    "limit.images_per_request",
    "limit.documents_per_request",
    "limit.image_bytes",
    "limit.image_long_edge_px",
    "limit.document_pages",
    "limit.document_bytes",
];
const PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "xai",
    "meta",
    "moonshotai",
    "deepseek",
    "google-vertex",
    "openrouter",
    "vercel",
    "opencode",
];

fn store() -> (CatalogStore, tempfile::TempDir) {
    let mut doc = json!({});
    for provider in PROVIDERS {
        doc[provider] = json!({"id": provider, "name": provider, "models": {}});
        for (model, modalities, context) in [
            ("both", json!(["text", "image", "pdf"]), json!(200000)),
            ("text", json!(["text"]), json!(200000)),
            ("image", json!(["text", "image"]), json!(200000)),
            ("pdf", json!(["text", "pdf"]), json!(200000)),
            ("million", json!(["image", "pdf"]), json!(1000000)),
            ("other", json!(["image", "pdf"]), json!(300000)),
            ("unknown", json!(["image", "pdf"]), Value::Null),
        ] {
            doc[provider]["models"][model] = json!({
                "id": model, "name": model,
                "limit": {"context": context, "output": 4096},
                "modalities": {"input": modalities, "output": ["text"]}
            });
        }
    }
    doc["google"]["models"]["gemini-3.8-flash"] = doc["google"]["models"]["both"].clone();
    doc["google"]["models"]["gemini-3.8-flash"]["id"] = json!("gemini-3.8-flash");
    doc["anthropic"]["models"]["claude-sonnet-5-5"] = doc["anthropic"]["models"]["million"].clone();
    doc["anthropic"]["models"]["claude-sonnet-5-5"]["id"] = json!("claude-sonnet-5-5");
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

fn limits(facts: &Value) -> Value {
    let facts = facts.as_object().expect("model must be served");
    Value::Object(
        facts
            .iter()
            .filter(|(key, _)| KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}

#[test]
fn attachment_limits_match_each_first_party_provider_exactly() {
    let (store, _dir) = store();
    let response = get(&store, json!({}));
    for (provider, expected) in [
        (
            "anthropic",
            json!({"limit.images_per_request":100,"limit.image_bytes":7500000,"limit.image_long_edge_px":3000,"limit.document_pages":100}),
        ),
        (
            "openai",
            json!({"limit.images_per_request":1500,"limit.document_bytes":50000000}),
        ),
        (
            "google",
            json!({"limit.images_per_request":3600,"limit.document_pages":1000,"limit.document_bytes":50000000}),
        ),
        ("xai", json!({"limit.image_bytes":20971520})),
        (
            "meta",
            json!({"limit.images_per_request":50,"limit.image_bytes":50000000,"limit.document_bytes":50000000}),
        ),
        ("moonshotai", json!({})),
        (
            "deepseek",
            json!({"limit.images_per_request":600,"limit.image_bytes":33554432,"limit.image_long_edge_px":4096}),
        ),
    ] {
        assert_eq!(
            limits(&response["models"][format!("{provider}/both")]),
            expected,
            "{provider}"
        );
        assert_eq!(
            limits(&response["models"][format!("{provider}/text")]),
            json!({}),
            "{provider}"
        );
        for model in ["image", "pdf"] {
            let mut expected = expected.as_object().unwrap().clone();
            expected.retain(|key, _| {
                if model == "image" {
                    key.starts_with("limit.image")
                } else {
                    key.starts_with("limit.document")
                }
            });
            assert_eq!(
                limits(&response["models"][format!("{provider}/{model}")]),
                Value::Object(expected),
                "{provider}/{model}"
            );
        }
    }
}

#[test]
fn attachment_limits_derive_anthropic_counts_only_from_known_context() {
    let (store, _dir) = store();
    let response = get(&store, json!({"provider_id":"anthropic"}));
    for (model, images, pages) in [
        ("both", 100, 100),
        ("million", 600, 600),
        ("other", 600, 100),
    ] {
        let facts = &response["models"][format!("anthropic/{model}")];
        assert_eq!(facts["limit.images_per_request"], images, "{model}");
        assert_eq!(facts["limit.document_pages"], pages, "{model}");
    }
    assert_eq!(
        limits(&response["models"]["anthropic/unknown"]),
        json!({"limit.image_bytes":7500000,"limit.image_long_edge_px":3000})
    );
}

#[test]
fn attachment_limits_exclude_gateways_and_historical_reads_and_never_write() {
    let (store, _dir) = store();
    let before = (
        store.era_count(SourceId::ModelsDev).unwrap(),
        store.catalog_version().unwrap(),
    );
    let current = get(&store, json!({}));
    for provider in ["google-vertex", "openrouter", "vercel", "opencode"] {
        assert_eq!(
            limits(&current["models"][format!("{provider}/both")]),
            json!({}),
            "{provider}"
        );
    }
    for provider in PROVIDERS {
        let past = get(
            &store,
            json!({"provider_id":provider,"model_id":"both","at_ms":1000}),
        );
        assert_eq!(
            limits(&past["models"][format!("{provider}/both")]),
            json!({}),
            "{provider}"
        );
        let stored = store
            .read_model(SourceId::ModelsDev, provider, "both", None)
            .unwrap()
            .0
            .unwrap();
        assert!(stored.facts.keys().all(|key| !KEYS.contains(&key.as_str())));
    }
    assert_eq!(
        before,
        (
            store.era_count(SourceId::ModelsDev).unwrap(),
            store.catalog_version().unwrap()
        )
    );
}

#[test]
fn attachment_limits_survive_prefix_plane_paged_and_alias_reads() {
    let (store, _dir) = store();
    for model in ["gemini-3.8-flash", "antigravity-gemini-3.8-flash"] {
        for args in [
            json!({"provider_id":"google","model_id":model,"fact_prefixes":["limit.images_per_request"]}),
            json!({"provider_id":"google","model_id":model,"auth_method":"apikey","fact_prefixes":["limit.images_per_request"]}),
            json!({"provider_id":"google","fact_prefixes":["limit.images_per_request"]}),
            json!({"paged":true,"fact_prefixes":["limit.images_per_request"]}),
        ] {
            let response = get(&store, args.clone());
            assert_eq!(
                response["models"][format!("google/{model}")],
                json!({"limit.images_per_request":3600}),
                "{args}"
            );
            for facts in response["models"].as_object().unwrap().values() {
                assert!(facts
                    .as_object()
                    .unwrap()
                    .keys()
                    .all(|key| key == "limit.images_per_request"));
            }
        }
    }
    let response = get(
        &store,
        json!({"provider_id":"anthropic","model_id":"million","fact_prefixes":["limit.document_pages"]}),
    );
    assert_eq!(
        response["models"]["anthropic/million"],
        json!({"limit.document_pages":600})
    );
    let response = get(
        &store,
        json!({"provider_id":"google","model_id":"antigravity-claude-sonnet-5-5","fact_prefixes":["limit.image", "limit.document"]}),
    );
    assert_eq!(
        response["models"]["google/antigravity-claude-sonnet-5-5"],
        json!({"limit.images_per_request":600,"limit.image_bytes":7500000,"limit.image_long_edge_px":3000,"limit.document_pages":600})
    );
}
