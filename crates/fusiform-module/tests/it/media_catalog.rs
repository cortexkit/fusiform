use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, ChargeBasis, Timestamp, TokenClass};
use fusiform_store::{ingest::plan_ingest, CatalogStore};
use serde_json::{json, Value};

/// Curated endpoints are their own documented claims, never provider defaults.
/// Rows without a declaration must keep every wire fact absent.
pub(crate) fn expected_curated_wire(model: &str) -> std::collections::BTreeMap<String, Value> {
    // These transcription rows have sourced endpoints but no verified
    // request-format token, so each declares an endpoint and no family.
    let endpoint_only = match model {
        "openai/gpt-transcribe" | "openai/gpt-live-transcribe" => Some("https://api.openai.com/v1"),
        "google/gemini-3.5-transcribe-live" => {
            Some("https://generativelanguage.googleapis.com/v1beta")
        }
        _ => None,
    };
    if let Some(api) = endpoint_only {
        return std::collections::BTreeMap::from([(
            "wire.api".into(),
            json!({"value":api,"from":"curated"}),
        )]);
    }
    let (api, family) = match model {
        "google/gemini-3.8-flash-tts" => (
            "https://generativelanguage.googleapis.com/v1beta",
            "google.generate_content",
        ),
        "elevenlabs/eleven_flash_v2_5" => {
            ("https://api.elevenlabs.io/v1", "elevenlabs.text_to_speech")
        }
        "openai/gpt-image-2.6-flare" => ("https://api.openai.com/v1", "openai.images"),
        "google/gemini-omni-1.2-flash" => (
            "https://generativelanguage.googleapis.com/v1beta",
            "google.interactions",
        ),
        _ => return std::collections::BTreeMap::new(),
    };
    std::collections::BTreeMap::from([
        ("wire.api".into(), json!({"value":api,"from":"curated"})),
        ("wire.family".into(), json!(family)),
    ])
}

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

fn price(units: i64, exponent: u32) -> Value {
    json!({"state":"priced", "units":units, "exponent":exponent,
        "currency":"USD", "unit_provenance":{"kind":"stated"}})
}

fn assert_curated(provider: &str, model: &str, expected: Value) {
    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    assert!(
        seed[provider]["models"].get(model).is_none(),
        "upstream now owns {provider}/{model}"
    );
    let (store, _dir) = empty_store();
    let response =
        super::curated_wire::get(&store, json!({"provider_id":provider,"model_id":model}));
    assert_eq!(
        serde_json::to_value(&response.models[&format!("{provider}/{model}")]).unwrap(),
        expected
    );
    let source = response
        .curated
        .iter()
        .find(|row| row.model == format!("{provider}/{model}"))
        .unwrap();
    let row = &fusiform_module::curated_models::models()[&(provider.into(), model.into())];
    assert_eq!(source.review_by_ms - row.established_at_ms, 60 * 86_400_000);
    assert!(!source.source_ref.is_empty());
}

#[test]
fn gemini_flash_tts_serves_only_documented_speech_facts() {
    assert_curated(
        "google",
        "gemini-3.8-flash-tts",
        json!({
        "existence":"present", "model.name":"Gemini 3.8 Flash TTS", "model.type":"speech",
        "model.lineage":"gemini",
            "capability.input_modalities":["text"], "capability.output_modalities":["audio"],
            "limit.context":8192, "limit.output":16384, "limit.output_audio_tokens_per_second":25,
            "limit.temperature_min":0.0, "limit.temperature_max":2.0,
            "rate.input":price(50,2), "rate.output_audio":price(9,0),
            "wire.api":{"value":"https://generativelanguage.googleapis.com/v1beta","from":"curated"},
            "wire.family":"google.generate_content"
        }),
    );
}

#[test]
fn eleven_flash_serves_only_documented_character_billing() {
    let mut rate = price(4, 2);
    rate["per"] = json!(1000);
    rate["increment"] = json!({"kind":"unknown"});
    rate["minimum_quantity"] = json!({"kind":"unknown"});
    assert_curated(
        "elevenlabs",
        "eleven_flash_v2_5",
        json!({
            "existence":"present", "provider.name":"ElevenLabs",
            "model.name":"Eleven Flash v2.5", "model.type":"speech",
            "capability.input_modalities":["text"], "capability.output_modalities":["audio"],
            "rate.input.per_character":rate,
            "wire.api":{"value":"https://api.elevenlabs.io/v1","from":"curated"},
            "wire.family":"elevenlabs.text_to_speech"
        }),
    );
    assert!(fusiform_module::wire_families::is_known(
        "elevenlabs.text_to_speech"
    ));
}

#[test]
fn output_audio_is_sourced_and_seeded() {
    let catalog = fusiform_module::seed::catalog().unwrap();
    let (store, _dir) = empty_store();
    let plan = plan_ingest(&store, &catalog, Timestamp(1000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    for (provider, model, units) in [
        ("openai", "gpt-realtime-2.1", 64),
        ("google", "gemini-3.1-flash-live-preview", 12),
    ] {
        let response = super::curated_wire::get(
            &store,
            json!({"provider_id":provider,"model_id":model,"fact_prefixes":["rate.output_audio"]}),
        );
        let rate = &response.models[&format!("{provider}/{model}")]["rate.output_audio"];
        let decoded: fusiform_protocol::RateValue = serde_json::from_value(rate.clone()).unwrap();
        let fusiform_protocol::RateValue::Priced { amount, per, .. } = decoded else {
            panic!("output audio must be priced: {rate}");
        };
        assert_eq!(amount.units, units * 1_000_000_000);
        assert_eq!(amount.exponent, 9);
        assert_eq!(amount.currency.as_str(), "USD");
        assert!(per.is_none());
    }
    for model in catalog
        .models()
        .filter(|model| !["google", "openai"].contains(&model.key.provider_id.as_str()))
    {
        assert!(!model.rates.iter().any(|rate| rate.basis
            == ChargeBasis::PerMillionTokens {
                class: TokenClass::OutputAudio
            }));
    }
}

#[test]
fn image_and_video_roles_are_explicitly_admitted() {
    for role in ["rate.input_image", "rate.output_image", "rate.output_video"] {
        assert!(fusiform_protocol::SERVED_FACTS
            .iter()
            .any(|fact| fact.key == role));
        assert!(fusiform_module::curated_models::models()
            .values()
            .any(|row| row.facts.contains_key(role)));
    }
}

#[test]
fn flare_serves_only_documented_image_facts() {
    assert_curated(
        "openai",
        "gpt-image-2.6-flare",
        json!({
            "existence":"present", "model.name":"GPT Image 2.6 Flare", "model.type":"image",
            "model.lineage":"gpt",
            "capability.input_modalities":["image","text"], "capability.output_modalities":["image"],
            "limit.images_per_request":1500,
            "limit.temperature_min":0, "limit.temperature_max":2,
            "rate.input":price(5,0), "rate.cache_read":price(125,2),
            "rate.input_image":price(8,0), "rate.output_image":price(30,0),
            "wire.api":{"value":"https://api.openai.com/v1","from":"curated"},
            "wire.family":"openai.images"
        }),
    );
    assert!(fusiform_module::wire_families::is_known("openai.images"));
}

#[test]
fn omni_serves_only_documented_video_facts() {
    assert_curated(
        "google",
        "gemini-omni-1.2-flash",
        json!({
            "existence":"present", "model.name":"Gemini Omni 1.2 Flash", "model.type":"video",
            "model.lineage":"gemini",
            "capability.input_modalities":["image","text","video"], "capability.output_modalities":["text","video"],
            "limit.images_per_request":3600,
            "limit.temperature_min":0.0, "limit.temperature_max":2.0,
            "rate.input":price(150,2), "rate.output":price(9,0), "rate.output_video":price(12,0),
            "limit.output_video_tokens_per_second.resolution.360p":1931,
            "limit.output_video_tokens_per_second.resolution.720p":5792,
            "limit.output_video_tokens_per_second.resolution.1080p":8688,
            "limit.output_video_tokens_per_second.resolution.4k":17376,
            "wire.api":{"value":"https://generativelanguage.googleapis.com/v1beta","from":"curated"},
            "wire.family":"google.interactions"
        }),
    );
    assert!(fusiform_module::wire_families::is_known(
        "google.interactions"
    ));
}

fn image_store(output: i64) -> (CatalogStore, tempfile::TempDir) {
    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    let mut doc = json!({});
    for provider in ["google", "google-vertex"] {
        let mut row = seed[provider]["models"]["gemini-3.1-flash-image"].clone();
        assert!(row.is_object());
        if provider == "google" {
            row["cost"]["output"] = json!(output);
        }
        doc[provider] = seed[provider].clone();
        doc[provider]["models"] = json!({"gemini-3.1-flash-image":row});
    }
    let catalog = fusiform_core::normalize_models_dev(&serde_json::to_vec(&doc).unwrap())
        .unwrap()
        .catalog;
    let (store, dir) = empty_store();
    let plan = plan_ingest(&store, &catalog, Timestamp(1000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

#[test]
fn flash_image_split_is_guarded_inherited_and_current_only() {
    let (store, _dir) = image_store(60);
    for model in [
        "gemini-3.1-flash-image",
        "antigravity-gemini-3.1-flash-image",
    ] {
        let response =
            super::curated_wire::get(&store, json!({"provider_id":"google","model_id":model}));
        let facts = &response.models[&format!("google/{model}")];
        assert_eq!(facts["rate.output"]["units"], 3_000_000_000i64);
        assert_eq!(facts["rate.output_image"]["units"], 60_000_000_000i64);
        assert_eq!(
            response.fact_overrides.len(),
            2,
            "{model}: both split prices must be disclosed"
        );
        for row in &response.fact_overrides {
            assert_eq!(row.value, facts[&row.fact_key]);
            if row.fact_key == "rate.output" {
                serde_json::from_value::<fusiform_protocol::RateValue>(row.replaces.clone())
                    .unwrap();
            }
        }
    }
    let historical = super::curated_wire::get(
        &store,
        json!({"provider_id":"google","model_id":"gemini-3.1-flash-image","at_ms":1000}),
    );
    let old = &historical.models["google/gemini-3.1-flash-image"];
    assert_eq!(old["rate.output"]["units"], 60_000_000_000i64);
    assert!(!old.contains_key("rate.output_image"));
    assert!(historical.fact_overrides.is_empty());
    let vertex = super::curated_wire::get(
        &store,
        json!({"provider_id":"google-vertex","model_id":"gemini-3.1-flash-image"}),
    );
    assert_eq!(
        vertex.models["google-vertex/gemini-3.1-flash-image"]["rate.output"]["units"],
        60_000_000_000i64
    );
    assert!(
        !vertex.models["google-vertex/gemini-3.1-flash-image"].contains_key("rate.output_image")
    );
    assert!(vertex.fact_overrides.is_empty());

    let (changed, _dir) = image_store(7);
    let response = super::curated_wire::get(
        &changed,
        json!({"provider_id":"google","model_id":"gemini-3.1-flash-image"}),
    );
    assert_eq!(
        response.models["google/gemini-3.1-flash-image"]["rate.output"]["units"],
        7_000_000_000i64
    );
    assert!(response
        .fact_overrides
        .iter()
        .all(|row| row.fact_key != "rate.output"));
}
