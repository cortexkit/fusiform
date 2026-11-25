use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, ChargeBasis, Timestamp, TokenClass};
use fusiform_store::{ingest::plan_ingest, CatalogStore};
use serde_json::{json, Value};

/// Curated endpoints are their own documented claims, never provider defaults.
/// Rows without a declaration must keep every wire fact absent.
pub(crate) fn expected_curated_wire(model: &str) -> std::collections::BTreeMap<String, Value> {
    // These transcription and realtime rows have sourced endpoints but no
    // verified request-format token, so each declares an endpoint and no family.
    let endpoint_only = match model {
        "openai/gpt-transcribe" | "openai/gpt-live-transcribe" | "openai/gpt-realtime-2.1" => {
            Some("https://api.openai.com/v1")
        }
        "google/gemini-3.5-transcribe" | "google/gemini-3.5-transcribe-live" => {
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

pub(super) fn v2_seed_store(ids: &[(&str, &str)]) -> (CatalogStore, tempfile::TempDir) {
    let (store, dir) = empty_store();
    let mut catalog = fusiform_module::seed::catalog().unwrap();
    catalog.providers.retain_mut(|provider| {
        provider
            .models
            .retain(|row| ids.contains(&(row.key.provider_id.as_str(), row.key.model_id.as_str())));
        !provider.models.is_empty()
    });
    assert_eq!(catalog.model_count(), ids.len());
    let plan = plan_ingest(&store, &catalog, Timestamp(1000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

fn price(units: i64, exponent: u32) -> Value {
    json!({"state":"priced", "units":units * 10_i64.pow(9 - exponent), "exponent":9,
        "currency":"USD", "unit_provenance":{"kind":"assumed_by_policy", "policy":"models-dev-usd-v1"}})
}

fn assert_v2_media(provider: &str, model: &str, expected: Value) {
    let (store, _dir) = v2_seed_store(&[(provider, model)]);
    let response =
        super::curated_wire::get(&store, json!({"provider_id":provider,"model_id":model}));
    let row = &response.models[&format!("{provider}/{model}")];
    for (key, expected) in expected.as_object().unwrap() {
        assert_eq!(row.get(key), Some(expected), "{provider}/{model}: {key}");
    }
    assert!(response.curated.is_empty());
}

#[test]
fn gemini_flash_tts_keeps_its_speech_rates_and_conversion_on_v2() {
    assert_v2_media(
        "google",
        "gemini-3.8-flash-tts",
        json!({
        "existence":"present", "model.name":"Gemini 3.8 Flash TTS", "model.type":"speech",
        "model.lineage":"gemini",
            "capability.input_modalities":["text"], "capability.output_modalities":["audio"],
            "limit.context":8192, "limit.output":16384, "limit.output_audio_tokens_per_second":25,
            "limit.temperature_min":0.0, "limit.temperature_max":2.0,
            "rate.input":price(50,2), "rate.output_audio":price(9,0),
            "wire.api":{"value":"https://generativelanguage.googleapis.com/v1beta","from":"model"},
            "wire.family":"google.generate_content"
        }),
    );
}

#[test]
fn eleven_flash_serves_only_documented_character_billing() {
    let mut rate = price(40, 0);
    rate["per"] = json!(1_000_000);
    assert_v2_media(
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
fn realtime_audio_is_priced_on_a_fresh_v2_store() {
    let (store, _dir) = empty_store();
    fusiform_module::seed::seed_if_empty(&store).unwrap();
    let response = super::curated_wire::get(
        &store,
        json!({"provider_id":"openai","model_id":"gpt-realtime-2.1"}),
    );
    assert!(response.fact_overrides.is_empty());
    assert!(response.curated.is_empty());
    let expected = json!({
        "existence":"present", "provider.name":"OpenAI", "model.name":"GPT-Realtime-2.1",
        "model.type":"realtime", "model.canonical_id":"openai/gpt-realtime-2.1",
        "model.lineage":"gpt", "capability.input_modalities":["audio","image","text"],
        "capability.output_modalities":["audio","text"], "capability.attachment":true,
        "capability.reasoning":true, "capability.tool_call":true,
        "capability.structured_output":false, "capability.temperature":false,
        "capability.reasoning_options":[{"type":"effort","values":["minimal","low","medium","high","xhigh"]}],
        "limit.context":128000, "limit.output":32000, "limit.images_per_request":1500,
        "rate.input":price(4_000_000_000,9), "rate.cache_read":price(400_000_000,9),
        "rate.output":price(24_000_000_000,9), "rate.input_audio":price(32_000_000_000,9),
        "rate.cached_input_audio":price(400_000_000,9), "rate.output_audio":price(64_000_000_000,9),
        "wire.api":{"value":"https://api.openai.com/v1","from":"model"}
    });
    for (key, expected) in expected.as_object().unwrap() {
        assert_eq!(
            response.models["openai/gpt-realtime-2.1"].get(key),
            Some(expected),
            "{key}"
        );
    }
}

#[test]
fn output_audio_is_sourced_and_seeded() {
    let catalog = fusiform_module::seed::catalog().unwrap();
    let (store, _dir) = empty_store();
    let plan = plan_ingest(&store, &catalog, Timestamp(1000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    for (provider, model, units) in [
        ("google", "gemini-3.1-flash-live-preview", 12),
        ("google", "gemini-3.5-live-translate-preview", 21),
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
    for model in catalog.models().filter(|model| {
        model.key.provider_id != "google"
            && !(model.key.provider_id == "openai" && model.key.model_id == "gpt-realtime-2.1")
    }) {
        assert!(!model.rates.iter().any(|rate| matches!(
            rate.value,
            fusiform_core::RateValue::Priced { .. }
        ) && rate.basis
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
        assert!(fusiform_module::seed::catalog()
            .unwrap()
            .models()
            .any(|model| {
                fusiform_store::ingest::fact_keys_of(model)
                    .into_iter()
                    .any(|key| key.as_str() == role)
            }));
    }
}

#[test]
fn flare_keeps_its_images_protocol_and_rates_on_v2() {
    assert_v2_media(
        "openai",
        "gpt-image-2.6-flare",
        json!({
            "existence":"present", "model.name":"GPT Image 2.6 Flare", "model.type":"image",
            "model.lineage":"gpt",
            "capability.input_modalities":["image","text"], "capability.output_modalities":["image"],
            "limit.images_per_request":1500,
            "capability.temperature":false,
            "rate.input":price(5,0), "rate.cache_read":price(125,2),
            "rate.input_image":price(8,0), "rate.output_image":price(30,0),
            "wire.api":{"value":"https://api.openai.com/v1","from":"model"},
            "wire.family":"openai.images"
        }),
    );
    assert!(fusiform_module::wire_families::is_known("openai.images"));
}

#[test]
fn omni_keeps_its_interactions_protocol_and_media_facts_on_v2() {
    assert_v2_media(
        "google",
        "gemini-omni-1.2-flash",
        json!({
            "existence":"present", "model.name":"Gemini Omni 1.2 Flash", "model.type":"video",
            "model.lineage":"gemini",
            "capability.input_modalities":["image","text","video"], "capability.output_modalities":["text","video"],
            "capability.aspect_ratios":["9:16","16:9"], "capability.output_audio":true,
            "limit.images_per_request":3600,
            "limit.output_duration_ms":15000,
            "limit.temperature_min":0.0, "limit.temperature_max":2.0,
            "rate.input":price(150,2), "rate.output":price(9,0), "rate.output_video":price(12,0),
            "limit.output_video_tokens_per_second.resolution.360p":1931,
            "limit.output_video_tokens_per_second.resolution.720p":5792,
            "limit.output_video_tokens_per_second.resolution.1080p":8688,
            "limit.output_video_tokens_per_second.resolution.4k":17376,
            "wire.api":{"value":"https://generativelanguage.googleapis.com/v1beta","from":"model"},
            "wire.family":"google.interactions"
        }),
    );
    assert!(fusiform_module::wire_families::is_known(
        "google.interactions"
    ));
}

#[test]
fn omni_video_facts_survive_prefix_filters_and_are_not_shared() {
    let (store, _dir) = v2_seed_store(&[
        ("google", "gemini-omni-1.2-flash"),
        ("google", "gemini-omni-1.1-flash"),
    ]);
    let id = "google/gemini-omni-1.2-flash";
    let expected = [
        ("limit.output_duration_ms", json!(15000)),
        ("capability.aspect_ratios", json!(["9:16", "16:9"])),
        ("capability.output_audio", json!(true)),
    ];
    for prefix in ["limit.", "capability."] {
        for named in [false, true] {
            let mut args = json!({"fact_prefixes":[prefix]});
            if named {
                args["provider_id"] = json!("google");
                args["model_id"] = json!("gemini-omni-1.2-flash");
            }
            let response = super::curated_wire::get(&store, args.clone());
            for (key, value) in &expected {
                if key.starts_with(prefix) {
                    assert_eq!(response.models[id].get(*key), Some(value), "{args}: {key}");
                }
            }
            for (model, facts) in &response.models {
                assert!(
                    facts.keys().all(|key| key.starts_with(prefix)),
                    "{args}: {model}"
                );
                if model != id {
                    for (key, _) in &expected {
                        assert!(!facts.contains_key(*key), "{model} must not claim {key}");
                    }
                }
            }
        }
    }
}

fn image_store(output: i64) -> (CatalogStore, tempfile::TempDir) {
    let seed: Value =
        serde_json::from_str(fusiform_core::normalize::legacy_models_dev_snapshot_for_tests())
            .unwrap();
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
    let catalog =
        fusiform_core::normalize_legacy_models_dev_for_tests(&serde_json::to_vec(&doc).unwrap())
            .unwrap()
            .catalog;
    let (store, dir) = empty_store();
    let plan = plan_ingest(&store, &catalog, Timestamp(1000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

#[test]
fn flash_image_split_is_guarded_inherited_and_current_only() {
    const TARGET_ID: &str = "gemini-3.1-flash-image";
    const ALIAS_ID: &str = "fixture-flash-image-alias";
    let (store, _dir) = image_store(3);
    let direct =
        super::curated_wire::get(&store, json!({"provider_id":"google","model_id":TARGET_ID}));

    // No live alias targets this split price card. Project the served target
    // through a fixture alias to check that both rates and the disclosure survive.
    let doc = json!({"aliases":[{
        "provider_id":"google", "model_id":ALIAS_ID,
        "target":{"provider_id":"google", "model_id":TARGET_ID},
        "basis":"Fixture for projecting split rates", "source_ref":"fixture",
        "established_at_ms":1000, "review_by_ms":2000
    }]});
    let (aliases, rejects) = fusiform_curate::aliases::load_reporting_rejects(&doc.to_string());
    assert!(rejects.is_empty(), "{rejects:?}");
    let projected = fusiform_curate::project_alias(
        direct.clone(),
        "google",
        ALIAS_ID,
        &aliases[&("google".into(), ALIAS_ID.into())],
        None,
        None,
    )
    .unwrap();

    for (model, response) in [(TARGET_ID, direct), (ALIAS_ID, projected)] {
        let identity = format!("google/{model}");
        let facts = &response.models[&identity];
        assert_eq!(facts["rate.output"]["units"], 3_000_000_000i64);
        assert_eq!(facts["rate.output_image"]["units"], 60_000_000_000i64);
        for rate in ["rate.output", "rate.output_image"] {
            if model == ALIAS_ID {
                let borrowed = &facts[rate]["inherited_from"];
                assert_eq!(borrowed["basis"], "alias");
                assert_eq!(borrowed["provider_id"], "google");
                assert_eq!(borrowed["model_id"], TARGET_ID);
            } else {
                assert!(facts[rate].get("inherited_from").is_none());
            }
        }
        let output_disclosures: Vec<_> = response
            .fact_overrides
            .iter()
            .filter(|row| row.fact_key.starts_with("rate.output"))
            .collect();
        assert_eq!(
            output_disclosures.len(),
            1,
            "{model}: only the image addition is curated; text is upstream"
        );
        let row = output_disclosures[0];
        assert_eq!(row.model, identity);
        assert_eq!(row.model_id, TARGET_ID);
        assert_eq!(row.fact_key, "rate.output_image");
        assert_eq!(row.value, facts[&row.fact_key]);
        assert!(row.replaces.is_null());

        let reasoning = response
            .fact_overrides
            .iter()
            .find(|row| row.fact_key == "rate.reasoning")
            .expect("the sourced reasoning rule has its own disclosure");
        assert_eq!(reasoning.model, identity);
        assert_eq!(reasoning.provider_id, "google");
        assert_eq!(reasoning.model_id, "*");
        assert_eq!(
            reasoning.value,
            json!({"state":"billed_as", "key":"rate.output"})
        );
        assert_eq!(reasoning.value, facts["rate.reasoning"]);
        assert!(reasoning.replaces.is_null());
    }
    let historical = super::curated_wire::get(
        &store,
        json!({"provider_id":"google","model_id":"gemini-3.1-flash-image","at_ms":1000}),
    );
    let old = &historical.models["google/gemini-3.1-flash-image"];
    assert_eq!(old["rate.output"]["units"], 3_000_000_000i64);
    assert!(!old.contains_key("rate.output_image"));
    assert!(historical.fact_overrides.is_empty());
    let vertex = super::curated_wire::get(
        &store,
        json!({"provider_id":"google-vertex","model_id":"gemini-3.1-flash-image"}),
    );
    assert_eq!(
        vertex.models["google-vertex/gemini-3.1-flash-image"]["rate.output"]["units"],
        3_000_000_000i64
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
