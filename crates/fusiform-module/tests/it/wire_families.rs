use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, SourceId, Timestamp};
use fusiform_protocol::CatalogGetResponse;
use fusiform_store::{ingest::plan_ingest, CatalogStore};
use serde_json::{
    json,
    value::{to_raw_value, RawValue},
    Value,
};
use std::collections::BTreeMap;

// Every rule has a real seed witness. Additional witnesses cover each package
// and shape combination that the seed actually publishes.
const CASES: &[(&str, &str, &str)] = &[
    ("openai", "gpt-6.1-sol", "openai.responses"),
    ("infer", "infer/gpt-6-astra:official", "openai.responses"),
    ("xai", "grok-4.20-multi-agent-0309", "openai.responses"),
    ("zenifra", "zenifra/qwen3.7-max", "openai.chat_completions"),
    ("sakana", "fugu-ultra-20260615", "openai.responses"),
    ("amazon-bedrock", "openai.gpt-5.5", "openai.responses"),
    ("anthropic", "claude-sonnet-5-5", "anthropic.messages"),
    ("google", "gemini-3.8-flash", "google.generate_content"),
    (
        "amazon-bedrock",
        "us.anthropic.claude-haiku-4-5-20251001-v1:0",
        "aws.bedrock_converse",
    ),
    ("aki-io", "gemma4-26b", "openai.chat_completions"),
    ("openrouter", "scaledown/extract", "openai.chat_completions"),
    ("cerebras", "qwen-3.8-27b", "openai.chat_completions"),
];

fn store() -> (CatalogStore, tempfile::TempDir) {
    let extras = [
        ("perplexity", "sonar", ""),
        ("google", "gemini-4-argon", ""),
        ("google", "gemini-4-carbon", ""),
        ("google", "gemini-4-hydrogen", ""),
        ("google", "gemini-4-neon", ""),
        ("google", "gemini-3.1-flash-image", ""),
        ("anthropic", "claude-opus-5-5", ""),
    ];
    let rows: Vec<_> = CASES
        .iter()
        .chain(&extras)
        .map(|&(provider, model, _)| (provider, model))
        .collect();
    let mut upstream = super::curated_wire::seed_cut(&rows);
    let flash = upstream
        .get_mut("google")
        .unwrap()
        .models
        .get_mut("gemini-3.1-flash-image")
        .unwrap();
    let mut row: BTreeMap<String, Box<RawValue>> = serde_json::from_str(flash.get()).unwrap();
    // Use the image model's GenerateContent entry: the current-only case tests
    // SDK rules, not the recorded images protocol that also serves in history.
    let mut apis: BTreeMap<String, Box<RawValue>> = serde_json::from_str(row["api"].get()).unwrap();
    let generate = apis.remove("generate-content").unwrap();
    row.insert(
        "api".into(),
        to_raw_value(&BTreeMap::from([("generate-content", generate)])).unwrap(),
    );
    let generate_only = to_raw_value(&row).unwrap();
    *flash = &generate_only;
    build_store(&serde_json::to_vec(&upstream).unwrap())
}

fn build_store(bytes: &[u8]) -> (CatalogStore, tempfile::TempDir) {
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
    let catalog = fusiform_core::normalize_models_dev_v2_with(
        bytes,
        &fusiform_module::input_audio::evidence(),
    )
    .unwrap()
    .catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

fn get(store: &CatalogStore, args: Value) -> CatalogGetResponse {
    super::curated_wire::get(store, args)
}

#[test]
fn images_shape_requires_a_verified_openai_image_sdk() {
    let ids = [
        ("google", "gemini-3.1-flash-image"),
        ("openai", "gpt-image-2.6-flare"),
        ("openrouter", "openai/gpt-image-1"),
    ];
    let (store, _dir) = super::media_catalog::v2_seed_store(&ids);
    for args in [json!({}), json!({"at_ms":1000})] {
        let response = get(&store, args.clone());
        let actual = ids.map(|(provider, model)| {
            let id = format!("{provider}/{model}");
            let row = &response.models[&id];
            assert_eq!(row["wire.shape"]["value"], "images", "{id}");
            row.get("wire.family")
        });
        assert_eq!(
            actual,
            [None, Some(&json!("openai.images")), None],
            "{args}"
        );
    }
}

#[test]
fn opencode_go_serves_saved_v2_protocols_on_plain_and_apikey_reads() {
    let snapshot = super::curated_wire::seed_providers();
    let (store, _dir) = build_store(
        &serde_json::to_vec(&std::collections::BTreeMap::from([(
            "opencode-go",
            snapshot["opencode-go"],
        )]))
        .unwrap(),
    );

    let plain = get(
        &store,
        json!({"provider_id":"opencode-go","fact_prefixes":["wire."]}),
    );
    let apikey = get(
        &store,
        json!({"provider_id":"opencode-go","auth_method":"apikey","fact_prefixes":["wire."]}),
    );
    for (model, family, protocol_path, npm) in [
        (
            "hy4-flash-free",
            "openai.chat_completions",
            "openai",
            "@ai-sdk/openai-compatible",
        ),
        (
            "muse-spark-1.3-contributor",
            "openai.responses",
            "openai",
            "@ai-sdk/openai",
        ),
        (
            "muse-spark-1.2-contributor",
            "openai.responses",
            "openai",
            "@ai-sdk/openai",
        ),
        ("gpt-6-luna", "openai.responses", "openai", "@ai-sdk/openai"),
        ("grok-4.7", "openai.responses", "openai", "@ai-sdk/openai"),
        (
            "qwen4-max",
            "anthropic.messages",
            "anthropic",
            "@ai-sdk/anthropic",
        ),
    ] {
        let id = format!("opencode-go/{model}");
        let row = &plain.models[&id];
        assert_eq!(row.get("wire.family"), Some(&json!(family)), "{id}");
        assert_eq!(
            row["wire.api"],
            json!({
                "value": format!("https://opencode.ai/inference/go/{protocol_path}/v1"),
                "from": "model",
            }),
            "{id}"
        );
        assert_eq!(
            row["wire.npm"],
            json!({"value": npm, "from": "model"}),
            "{id}"
        );
        assert!(!row.contains_key("wire.shape"), "{id}");
    }
    assert_eq!(
        serde_json::to_value(&apikey.plane).unwrap()["kind"],
        "quota_proxy"
    );
    assert_eq!(apikey.models, plain.models);
    assert!(plain.wire_corrections.is_empty());
    assert!(apikey.wire_corrections.is_empty());
}

#[test]
fn npm_rules_serve_verified_protocols_from_real_seed_rows() {
    let (store, _dir) = store();
    let eras = store.era_count(SourceId::ModelsDev).unwrap();
    let version = store.catalog_version().unwrap();
    let bulk = get(&store, json!({}));
    for &(provider, model, expected) in CASES {
        let id = format!("{provider}/{model}");
        assert_eq!(bulk.models[&id]["wire.family"], expected, "{id}");
        let named = get(&store, json!({"provider_id":provider,"model_id":model}));
        assert_eq!(named.models[&id]["wire.family"], expected, "{id}");
        let (stored, _) = store
            .read_model(SourceId::ModelsDev, provider, model, None)
            .unwrap();
        assert!(!stored
            .unwrap()
            .facts
            .keys()
            .any(|key| key.as_str() == "wire.family"));
    }
    assert_eq!(store.era_count(SourceId::ModelsDev).unwrap(), eras);
    assert_eq!(store.catalog_version().unwrap(), version);
    assert!(!bulk.models["perplexity/sonar"].contains_key("wire.family"));
}

#[test]
fn code_assist_is_the_alias_own_protocol_and_retired_aliases_have_none() {
    let (store, _dir) = store();
    let response = get(
        &store,
        json!({"provider_id":"google","include_retired":true}),
    );
    for (provider, model) in fusiform_module::aliases::aliases().keys() {
        let facts = &response.models[&format!("{provider}/{model}")];
        assert_eq!(facts["wire.family"], "google.code_assist");
        assert!(!facts.contains_key("wire.npm"));
    }
    let retired = &response.models["google/antigravity-gemini-3.7-flash"];
    assert_eq!(retired["existence"], "absent");
    assert!(!retired.contains_key("wire.family"));
    let named = get(
        &store,
        json!({"provider_id":"google","model_id":"antigravity-claude-sonnet-5-5","fact_prefixes":["wire.family"]}),
    );
    assert_eq!(
        named.models["google/antigravity-claude-sonnet-5-5"],
        std::collections::BTreeMap::from([("wire.family".into(), json!("google.code_assist"))])
    );
}

#[test]
fn neon_alias_serves_code_assist_with_borrowed_prices_and_target_limits() {
    let (store, _dir) = store();
    let alias_response = get(
        &store,
        json!({"provider_id":"google","model_id":"antigravity-gemini-4-neon"}),
    );
    let alias = alias_response
        .models
        .get("google/antigravity-gemini-4-neon")
        .expect("Neon must be served through its seeded target");
    let response = get(
        &store,
        json!({"provider_id":"google","model_id":"gemini-4-neon"}),
    );
    let target = &response.models["google/gemini-4-neon"];

    assert_eq!(alias["limit.context"], target["limit.context"]);
    assert_eq!(alias["limit.context"], 1_048_576);
    assert_eq!(alias["limit.output"], target["limit.output"]);
    assert_eq!(target["limit.output"], 65_536);
    assert!(alias_response
        .fact_overrides
        .iter()
        .all(|row| row.fact_key != "limit.output"));
    assert_eq!(alias["wire.family"], "google.code_assist");
    assert_eq!(alias["route.reasoning_default"], "high");
    assert_eq!(
        alias["capability.reasoning_options"],
        json!([{"type":"effort","values":["low","medium","high","xhigh"]}])
    );
    assert_eq!(
        alias["route.reasoning_wire_ids"],
        json!({
            "low": "gemini-4-neon-low",
            "medium": "gemini-4-neon-medium",
            "high": "gemini-4-neon-high",
            "xhigh": "gemini-4-neon-xhigh"
        })
    );
    for key in [
        "rate.input",
        "rate.output",
        "rate.cache_read",
        "rate.input_audio",
    ] {
        let mut borrowed = alias[key].as_object().unwrap().clone();
        let origin = borrowed.remove("inherited_from").unwrap();
        assert_eq!(origin["basis"], "alias", "{key}");
        assert_eq!(origin["provider_id"], "google", "{key}");
        assert_eq!(origin["model_id"], "gemini-4-neon", "{key}");
        assert_eq!(&borrowed, target[key].as_object().unwrap(), "{key}");
    }
}

#[test]
fn billing_plane_overrides_only_the_selected_protocol() {
    let (store, _dir) = store();
    for (auth, expected) in [
        (None, "openai.responses"),
        (Some("apikey"), "openai.responses"),
        (Some("chatgpt"), "openai.chatgpt_responses"),
    ] {
        let mut args = json!({"provider_id":"openai","model_id":"gpt-6.1-sol","fact_prefixes":["wire.family"]});
        if let Some(auth) = auth {
            args["auth_method"] = json!(auth);
        }
        let response = get(&store, args);
        assert_eq!(
            response.models["openai/gpt-6.1-sol"]["wire.family"],
            expected
        );
        assert_eq!(response.models["openai/gpt-6.1-sol"].len(), 1);
    }
    let response = get(
        &store,
        json!({"provider_id":"meta","auth_method":"oauth","fact_prefixes":["wire.family"]}),
    );
    assert_eq!(
        response.models["meta/muse-wonder-full-1.0"]["wire.family"],
        "openai.responses"
    );
    let plain = get(
        &store,
        json!({"provider_id":"meta","model_id":"muse-wonder-full-1.0"}),
    );
    assert!(!plain.models["meta/muse-wonder-full-1.0"].contains_key("wire.family"));
}

#[test]
fn protocol_is_current_only_and_respects_fact_prefixes() {
    let (store, _dir) = store();
    for args in [
        json!({"at_ms":1000}),
        json!({"at_ms":1000,"provider_id":"openai","model_id":"gpt-6.1-sol"}),
        json!({"at_ms":1000,"fact_prefixes":["wire.family"]}),
    ] {
        let response = get(&store, args);
        assert!(response
            .models
            .values()
            .all(|facts| !facts.contains_key("wire.family")));
    }
    for prefix in ["wire.family", "wire.", "rate."] {
        for mut args in [
            json!({}),
            json!({"provider_id":"openai"}),
            json!({"provider_id":"openai","model_id":"gpt-6.1-sol"}),
            json!({"paged":true}),
        ] {
            args["fact_prefixes"] = json!([prefix]);
            let response = get(&store, args);
            let facts = &response.models["openai/gpt-6.1-sol"];
            if prefix != "wire." {
                assert!(response.wire_corrections.is_empty());
            }
            assert!(facts.keys().all(|key| key.starts_with(prefix)));
            assert_eq!(
                facts.get("wire.family"),
                prefix
                    .starts_with("wire")
                    .then_some(&json!("openai.responses"))
            );
        }
    }
}

#[test]
fn aliases_and_planes_reject_unknown_protocol_tokens() {
    let mut aliases: Value =
        serde_json::from_str(include_str!("../../data/model-aliases.json")).unwrap();
    aliases["aliases"][0]["wire_family"] = json!("unverified.protocol");
    let (loaded, rejects) = fusiform_module::aliases::load_reporting_rejects(&aliases.to_string());
    assert_eq!(rejects.len(), 1);
    assert!(rejects[0].contains("unknown wire_family"));
    assert_eq!(
        loaded.len(),
        aliases["aliases"].as_array().unwrap().len() - 1
    );
    let mut planes: Value =
        serde_json::from_str(include_str!("../../data/billing-planes.json")).unwrap();
    planes["planes"][0]["wire_family"] = json!("unverified.protocol");
    let (loaded, rejects) =
        fusiform_module::billing_planes::load_reporting_rejects(&planes.to_string());
    assert_eq!(rejects.len(), 1);
    assert!(rejects[0].contains("unknown wire_family"));
    assert_eq!(loaded.len(), planes["planes"].as_array().unwrap().len() - 1);
}
