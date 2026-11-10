use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, ChargeBasis, Timestamp, TokenClass};
use fusiform_store::{ingest::plan_ingest, CatalogStore};
use serde_json::{json, Value};

/// Curated endpoints are their own documented claims, never provider defaults.
/// Rows without a declaration must keep every wire fact absent.
pub(crate) fn expected_curated_wire(model: &str) -> std::collections::BTreeMap<String, Value> {
    let (api, family) = match model {
        "google/gemini-3.8-flash-tts" => (
            "https://generativelanguage.googleapis.com/v1beta",
            "google.generate_content",
        ),
        "elevenlabs/eleven_flash_v2_5" => {
            ("https://api.elevenlabs.io/v1", "elevenlabs.text_to_speech")
        }
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
fn speech_catalog_has_no_unadmitted_image_or_video_roles() {
    for role in ["rate.input_image", "rate.output_image", "rate.output_video"] {
        assert!(!fusiform_protocol::SERVED_FACTS
            .iter()
            .any(|fact| fact.key == role));
        assert!(!fusiform_module::curated_models::models()
            .values()
            .any(|row| row.facts.contains_key(role)));
    }
}
