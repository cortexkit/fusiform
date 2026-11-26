use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::{serve_tool_call, ToolResponse};
use fusiform_protocol::CatalogGetResponse;
use fusiform_store::{ingest::plan_ingest, CatalogStore, FactKey, NewEra, NewObservation};
use serde::{Deserialize, Serialize};
use serde_json::{
    json,
    value::{to_raw_value, RawValue},
    Value,
};
use std::collections::BTreeMap;

const FLASH: &str = "google/antigravity-gemini-3.8-flash";
const ARGON: &str = "google/antigravity-gemini-4-argon";
const SONNET: &str = "google/antigravity-claude-sonnet-5-5";
const OPUS: &str = "google/antigravity-claude-opus-5-5";
const RETIRED: &str = "google/antigravity-gemini-3.7-flash";

#[derive(Deserialize, Serialize)]
pub(super) struct FixtureProvider<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(borrow)]
    pub models: BTreeMap<String, &'a RawValue>,
}

pub(super) type FixtureDocument<'a> = BTreeMap<String, FixtureProvider<'a>>;

pub(super) fn seed_providers() -> BTreeMap<&'static str, &'static RawValue> {
    const SEED: &str = include_str!("../../data/models-dev-seed.json");
    serde_json::from_str(SEED).unwrap()
}

pub(super) fn seed_cut(rows: &[(&str, &str)]) -> FixtureDocument<'static> {
    let seed = seed_providers();
    let mut cut = FixtureDocument::new();
    for &(provider, _) in rows {
        cut.entry(provider.into())
            .or_insert_with(|| serde_json::from_str(seed[provider].get()).unwrap());
    }
    for (provider, row) in &mut cut {
        row.models
            .retain(|model, _| rows.contains(&(provider, model)));
    }
    for &(provider, model) in rows {
        assert!(
            cut[provider].models.contains_key(model),
            "missing seed row {provider}/{model}"
        );
    }
    // Keep each model raw: the parser uses document order for API and TTL defaults.
    cut
}

pub(crate) fn store() -> (CatalogStore, tempfile::TempDir) {
    store_with_anthropic_api(None)
}

fn store_with_anthropic_api(api: Option<&str>) -> (CatalogStore, tempfile::TempDir) {
    let mut upstream = seed_cut(&[
        ("opencode-go", "claude-haiku-5-5"),
        ("opencode-go", "minimax-m2.7"),
        ("opencode-go", "minimax-m3"),
        ("google", "gemini-3.8-flash"),
        ("google", "gemini-4-argon"),
        ("google", "gemini-3.1-flash-image"),
        ("openai", "gpt-6.1-sol"),
        ("anthropic", "claude-opus-5-5"),
        ("anthropic", "claude-sonnet-5-5"),
        ("xai", "grok-4.7"),
        ("cerebras", "qwen-3.8-27b"),
    ]);
    let mut observed: BTreeMap<String, Box<RawValue>> =
        serde_json::from_str(upstream["openai"].models["gpt-6.1-sol"].get()).unwrap();
    observed.insert("id".into(), to_raw_value("observed-api").unwrap());
    observed.insert(
        "api".into(),
        to_raw_value(&json!({"responses":{
            "base_url":"https://observed.example/model/v1","sdk":{"aisdk":"@ai-sdk/openai"}
        }}))
        .unwrap(),
    );
    let observed = to_raw_value(&observed).unwrap();
    upstream
        .get_mut("openai")
        .unwrap()
        .models
        .insert("observed-api".into(), &observed);

    // Exercise endpoint fallback with a single API that names an SDK but no base URL.
    let without_endpoints: Vec<_> = [
        ("openai", "gpt-6.1-sol", "responses", "@ai-sdk/openai"),
        (
            "anthropic",
            "claude-opus-5-5",
            "messages",
            "@ai-sdk/anthropic",
        ),
        (
            "anthropic",
            "claude-sonnet-5-5",
            "messages",
            "@ai-sdk/anthropic",
        ),
        (
            "google",
            "gemini-3.8-flash",
            "generate-content",
            "@ai-sdk/google",
        ),
        ("xai", "grok-4.7", "responses", "@ai-sdk/xai"),
        (
            "cerebras",
            "qwen-3.8-27b",
            "chat-completions",
            "@ai-sdk/cerebras",
        ),
    ]
    .into_iter()
    .map(|(provider, id, protocol, sdk)| {
        let mut row: BTreeMap<String, Box<RawValue>> =
            serde_json::from_str(upstream[provider].models[id].get()).unwrap();
        row.insert(
            "api".into(),
            to_raw_value(&json!({protocol:{"sdk":{"aisdk":sdk}}})).unwrap(),
        );
        if provider == "openai" {
            // Endpoint fallback should still render the ordinary price card.
            let mut cost: BTreeMap<String, Box<RawValue>> =
                serde_json::from_str(row["cost"].get()).unwrap();
            let mut rules: Vec<&RawValue> =
                serde_json::from_str(cost["multipliers"].get()).unwrap();
            rules.retain(|rule| {
                let rule: Value = serde_json::from_str(rule.get()).unwrap();
                rule.pointer("/when/not/region").is_none()
            });
            cost.insert("multipliers".into(), to_raw_value(&rules).unwrap());
            row.insert("cost".into(), to_raw_value(&cost).unwrap());
        } else if provider == "anthropic" {
            // The alias assertions compare base rates, not regional or service-tier prices.
            let (input, read, write, output) = if id == "claude-opus-5-5" {
                (4, 0.2, 5.0, 20)
            } else {
                (2, 0.1, 2.5, 10)
            };
            row.insert(
                "cost".into(),
                to_raw_value(&json!({"text":{
                    "input":{"price":input,"cache":{"read":read,"write":write}},
                    "output":{"price":output}
                }}))
                .unwrap(),
            );
        }
        (provider, id, to_raw_value(&row).unwrap())
    })
    .collect();
    for (provider, id, row) in &without_endpoints {
        upstream
            .get_mut(*provider)
            .unwrap()
            .models
            .insert((*id).into(), row);
    }
    // Remove the image price, so the row looks as it did before models.dev
    // published one, when the hand-kept image-price override was still needed.
    let flash = upstream
        .get_mut("google")
        .unwrap()
        .models
        .get_mut("gemini-3.1-flash-image")
        .unwrap();
    let mut row: BTreeMap<String, Box<RawValue>> = serde_json::from_str(flash.get()).unwrap();
    let mut cost: Value = serde_json::from_str(row["cost"].get()).unwrap();
    assert!(cost.as_object_mut().unwrap().remove("image").is_some());
    row.insert("cost".into(), to_raw_value(&cost).unwrap());
    let flash_without_image_price = to_raw_value(&row).unwrap();
    *flash = &flash_without_image_price;
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
            normalized_hash: Some("seed".into()),
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let catalog = fusiform_core::normalize_models_dev_v2_with(
        &serde_json::to_vec(&upstream).unwrap(),
        &fusiform_module::input_audio::evidence(),
    )
    .unwrap()
    .catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    if let Some(api) = api {
        // Provider-origin endpoints predate the model-only API entries in models.dev v2.
        store
            .append_eras(&[NewEra {
                source: SourceId::ModelsDev,
                provider_id: "anthropic".into(),
                model_id: "claude-opus-5-5".into(),
                fact_key: FactKey::from_stored("wire.api".into()),
                value_json: json!({"value":api,"from":"provider"}).to_string(),
                boundary_at: Timestamp(1_000),
                boundary_kind: BoundaryKind::Seed,
                observation_id: None,
            }])
            .unwrap();
    }
    // Opaque wire shapes predate the protocol-derived shapes in models.dev v2.
    let old_shapes = ["claude-haiku-5-5", "minimax-m2.7"].map(|id| NewEra {
        source: SourceId::ModelsDev,
        provider_id: "opencode-go".into(),
        model_id: id.into(),
        fact_key: FactKey::from_stored("wire.shape".into()),
        value_json: json!({"value":"observed-shape","from":"model"}).to_string(),
        boundary_at: Timestamp(1_000),
        boundary_kind: BoundaryKind::Seed,
        observation_id: None,
    });
    store.append_eras(&old_shapes).unwrap();
    (store, dir)
}

pub(crate) fn get(store: &CatalogStore, arguments: Value) -> CatalogGetResponse {
    let request =
        serde_json::to_vec(&json!({"name":"catalog.get", "arguments":arguments})).unwrap();
    let ToolResponse::Catalog(response) = serve_tool_call(store, &request).unwrap() else {
        panic!("expected catalog")
    };
    response
}

#[test]
fn provider_api_defaults_fill_only_missing_current_endpoints() {
    let (store, _dir) = store();
    for (provider, id, api) in [
        ("openai", "gpt-6.1-sol", "https://api.openai.com/v1"),
        (
            "anthropic",
            "claude-opus-5-5",
            "https://api.anthropic.com/v1",
        ),
        (
            "google",
            "gemini-3.8-flash",
            "https://generativelanguage.googleapis.com/v1beta",
        ),
        ("xai", "grok-4.7", "https://api.x.ai/v1"),
        // models.dev names only the SDK package for Cerebras and leaves the
        // endpoint to that SDK's default, so without this row a runner that
        // opens rows through wire.api cannot open Cerebras at all.
        ("cerebras", "qwen-3.8-27b", "https://api.cerebras.ai/v1"),
    ] {
        let identity = format!("{provider}/{id}");
        for arguments in [
            json!({}),
            json!({"provider_id":provider}),
            json!({"provider_id":provider,"model_id":id}),
            json!({"fact_prefixes":["wire.api"]}),
            json!({"paged":true}),
        ] {
            let response = get(&store, arguments);
            assert_eq!(
                response.models[&identity]["wire.api"],
                json!({"value":api,"from":"curated"})
            );
            let disclosure = response
                .wire_corrections
                .iter()
                .find(|row| row.provider == provider && row.model == id)
                .unwrap();
            assert_eq!(disclosure.field, "wire.api");
            assert!(!disclosure.withheld);
            assert_eq!(disclosure.established_by, "fusiform");
            assert_eq!(
                disclosure.review_by_ms - disclosure.established_at_ms,
                60 * 86_400_000
            );
        }
        let historical = get(
            &store,
            json!({"provider_id":provider,"model_id":id,"at_ms":1_000}),
        );
        assert!(!historical.models[&identity].contains_key("wire.api"));
        assert!(historical.wire_corrections.is_empty());
        let request = json!({"name":"catalog.history","arguments":{"provider_id":provider,"model_id":id,"fact_key":"wire.api"}});
        let ToolResponse::History(history) =
            serve_tool_call(&store, &serde_json::to_vec(&request).unwrap()).unwrap()
        else {
            panic!("expected history")
        };
        assert!(history.eras.is_empty());
    }
    let observed = get(
        &store,
        json!({"provider_id":"openai","model_id":"observed-api"}),
    );
    assert_eq!(
        observed.models["openai/observed-api"]["wire.api"],
        json!({"value":"https://observed.example/model/v1","from":"model"})
    );
    assert!(observed.wire_corrections.is_empty());
    let (observed_store, _observed_dir) =
        store_with_anthropic_api(Some("https://observed.example/provider/v1"));
    let observed = get(
        &observed_store,
        json!({"provider_id":"anthropic","model_id":"claude-opus-5-5"}),
    );
    assert_eq!(
        observed.models["anthropic/claude-opus-5-5"]["wire.api"],
        json!({"value":"https://observed.example/provider/v1","from":"provider"})
    );
    assert!(observed.wire_corrections.is_empty());
    let filtered = get(&store, json!({"fact_prefixes":["wire.npm"]}));
    assert!(filtered.wire_corrections.is_empty());
    assert!(filtered
        .models
        .values()
        .all(|facts| !facts.contains_key("wire.api")));
    let response = get(&store, json!({}));
    assert!(!response.aliased.is_empty());
    assert!(!response.curated.is_empty());
    for row in &response.aliased {
        if let Some(facts) = response.models.get(&row.model) {
            assert!(!facts.contains_key("wire.api"));
        }
        assert!(!response
            .wire_corrections
            .iter()
            .any(
                |correction| format!("{}/{}", correction.provider, correction.model) == row.model
            ));
    }
    for row in &response.curated {
        let wire: std::collections::BTreeMap<_, _> = response.models[&row.model]
            .iter()
            .filter(|(key, _)| key.starts_with("wire."))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        assert_eq!(
            wire,
            super::media_catalog::expected_curated_wire(&row.model)
        );
        assert!(!response
            .wire_corrections
            .iter()
            .any(
                |correction| format!("{}/{}", correction.provider, correction.model) == row.model
            ));
    }
    assert!(response.wire_corrections.iter().all(|row| response
        .models
        .contains_key(&format!("{}/{}", row.provider, row.model))));
}

#[test]
fn go_observed_endpoints_serve_currently_without_changing_history() {
    let (store, _dir) = store();
    let id = "opencode-go/claude-haiku-5-5";
    let expected = json!({"value":"https://opencode.ai/inference/go/anthropic/v1","from":"model"});
    for args in [
        json!({"provider_id":"opencode-go"}),
        json!({"provider_id":"opencode-go","model_id":"claude-haiku-5-5"}),
        json!({"fact_prefixes":["wire.api"],"paged":true}),
        json!({"provider_id":"opencode-go","at_ms":1_000}),
    ] {
        let response = get(&store, args);
        assert_eq!(response.models[id]["wire.api"], expected);
        assert!(response
            .wire_corrections
            .iter()
            .all(|row| row.provider != "opencode-go"));
    }
    let request = json!({"name":"catalog.history","arguments":{"provider_id":"opencode-go","model_id":"claude-haiku-5-5","fact_key":"wire.api"}});
    let ToolResponse::History(history) =
        serve_tool_call(&store, &serde_json::to_vec(&request).unwrap()).unwrap()
    else {
        panic!("expected history")
    };
    assert_eq!(history.eras.len(), 1);
    assert_eq!(history.eras[0].value, expected);
}

#[test]
fn alias_tier_wire_ids_are_route_local_and_obey_filters() {
    let (store, _dir) = store();
    for arguments in [
        json!({}),
        json!({"provider_id":"google"}),
        json!({"provider_id":"google","model_id":"antigravity-gemini-3.8-flash"}),
        json!({"fact_prefixes":["route."]}),
    ] {
        let response = get(&store, arguments);
        let row = &response.models[FLASH];
        assert_eq!(
            row["route.reasoning_wire_ids"],
            json!({"low":"gemini-3.8-flash-low","medium":"gemini-3.8-flash-medium","high":"gemini-3.8-flash-high"})
        );
        assert_eq!(row["route.reasoning_default"], "medium");
        for (id, facts) in &response.models {
            if !matches!(id.as_str(), FLASH | ARGON | SONNET | OPUS) {
                assert!(!facts.keys().any(|key| key.starts_with("route.")), "{id}");
            }
        }
    }
    let argon = get(
        &store,
        json!({"provider_id":"google","model_id":"antigravity-gemini-4-argon"}),
    );
    assert_eq!(
        argon.models[ARGON]["route.reasoning_wire_ids"],
        json!({"low":"gemini-4-argon-low","medium":"gemini-4-argon-medium","high":"gemini-4-argon-high"})
    );
    assert_eq!(argon.models[ARGON]["route.reasoning_default"], "medium");
    for (alias, target) in [(FLASH, "gemini-3.8-flash"), (ARGON, "gemini-4-argon")] {
        let aliased = get(&store, json!({"provider_id":"google"}));
        assert_eq!(
            aliased.models[alias]["capability.reasoning_options"],
            json!([
                {"type":"effort","values":["low","medium","high"]}
            ])
        );
        let direct = get(&store, json!({"provider_id":"google","model_id":target}));
        assert_eq!(
            direct.models[&format!("google/{target}")]["capability.reasoning_options"],
            json!([
                {"type":"effort","values":["low","medium","high"]}
            ])
        );
    }
    let filtered = get(
        &store,
        json!({"provider_id":"google","model_id":"antigravity-gemini-3.8-flash","fact_prefixes":["capability."]}),
    );
    assert!(!filtered.models[FLASH]
        .keys()
        .any(|key| key.starts_with("route.")));
    let historical = get(&store, json!({"provider_id":"google","at_ms":1_000}));
    assert!(!historical.models.contains_key(FLASH));
}

#[test]
fn claude_aliases_restrict_efforts_without_changing_the_targets() {
    let (store, _dir) = store();
    for (alias, model) in [(SONNET, "claude-sonnet-5-5"), (OPUS, "claude-opus-5-5")] {
        let target = format!("anthropic/{model}");
        let full_options =
            json!([{"type":"effort","values":["low","medium","high","xhigh","max"]}]);
        let direct = get(&store, json!({"provider_id":"anthropic","model_id":model}));
        assert_eq!(
            direct.models[&target]["capability.reasoning_options"],
            full_options
        );

        for arguments in [
            json!({}),
            json!({"provider_id":"google"}),
            json!({"provider_id":"google","model_id":alias.strip_prefix("google/").unwrap()}),
            json!({"provider_id":"google","fact_prefixes":["capability.reasoning_options"]}),
        ] {
            let response = get(&store, arguments);
            let facts = &response.models[alias];
            assert_eq!(
                facts["capability.reasoning_options"],
                json!([{"type":"effort","values":["low","medium","high"]}]),
                "{alias} must advertise only efforts the route can send"
            );
            if let Some(wire_ids) = facts.get("route.reasoning_wire_ids") {
                assert_eq!(
                    wire_ids,
                    &json!({
                        "low":format!("{model}-low"),
                        "medium":format!("{model}-medium"),
                        "high":format!("{model}-high"),
                    })
                );
                assert_eq!(facts["route.reasoning_default"], "low");
                assert_eq!(facts["limit.context"], 1_000_000);
                assert_eq!(facts["limit.output"], 128_000);
                assert_eq!(facts["rate.input"]["state"], "priced");
                assert_eq!(
                    facts["rate.input"]["units"],
                    direct.models[&target]["rate.input"]["units"]
                );
                for (key, value) in facts.iter().filter(|(key, _)| key.starts_with("rate.")) {
                    match value["state"].as_str() {
                        Some("priced" | "stated_zero" | "unpriced") => {
                            assert_eq!(value["inherited_from"]["basis"], "alias", "{key}");
                            assert_eq!(
                                value["inherited_from"]["provider_id"], "anthropic",
                                "{key}"
                            );
                            assert_eq!(value["inherited_from"]["model_id"], model, "{key}");
                        }
                        Some("billed_as") => assert_eq!(key, "rate.reasoning"),
                        state => panic!("{key}: unexpected rate state {state:?}"),
                    }
                }
                assert_eq!(
                    facts["rate.reasoning"],
                    json!({"state":"billed_as", "key":"rate.output"})
                );
            } else {
                assert_eq!(facts.len(), 1, "the capability filter must be applied last");
            }
        }

        // Direct current and historical reads still expose the target's full
        // option list: narrowing belongs only to the alias's served view.
        for arguments in [
            json!({"provider_id":"anthropic","model_id":model}),
            json!({"provider_id":"anthropic","model_id":model,"at_ms":1_000}),
        ] {
            let response = get(&store, arguments);
            assert_eq!(
                response.models[&target]["capability.reasoning_options"],
                full_options
            );
            assert!(!response.models[&target].contains_key("route.reasoning_wire_ids"));
        }
        assert!(!store
            .model_is_known(
                SourceId::ModelsDev,
                "google",
                alias.strip_prefix("google/").unwrap()
            )
            .unwrap());
    }
    let historical = get(&store, json!({"provider_id":"google","at_ms":1_000}));
    assert!(!historical.models.contains_key(SONNET));
    assert!(!historical.models.contains_key(OPUS));
}

#[test]
fn retired_alias_successors_survive_named_bulk_and_filtered_reads() {
    let (store, _dir) = store();
    for model in [
        "antigravity-gemini-3.6-flash",
        "antigravity-gemini-3.7-flash",
    ] {
        let named = get(&store, json!({"provider_id":"google","model_id":model}));
        assert_eq!(named.retired.len(), 1);
        assert_eq!(named.retired[0].successor.as_deref(), Some(FLASH));
        for args in [
            json!({"include_retired":true}),
            json!({"provider_id":"google","model_id":model,"include_retired":true}),
            json!({"include_retired":true,"fact_prefixes":["successor"]}),
        ] {
            let response = get(&store, args);
            assert_eq!(
                response.models[&format!("google/{model}")]["successor"],
                FLASH
            );
            assert!(response.retired.is_empty());
        }
    }
    let all = get(&store, json!({"include_retired":true}));
    assert_eq!(all.models[RETIRED]["existence"], "absent");
    assert!(!all.models["google/antigravity-claude-sonnet-4-6-thinking"].contains_key("successor"));
    let claude = get(
        &store,
        json!({"provider_id":"google","model_id":"antigravity-claude-sonnet-4-6-thinking"}),
    );
    assert_eq!(claude.retired[0].successor, None);
    assert!(!get(
        &store,
        json!({"provider_id":"google","at_ms":1_000,"include_retired":true})
    )
    .models
    .contains_key(RETIRED));
}
