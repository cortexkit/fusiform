//! Billing-plane reads, driven through the real tool route and the shipped
//! planes file. These tests use their own synthetic upstream rows, so each
//! refusal and each never-overwrite-a-published-price check can be seen on its
//! own without adding models to the golden store, whose counts other tests pin.

use super::*;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::BoundaryKind;
use fusiform_store::NewEra;
use serde_json::{json, Value};

fn store() -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    })
    .unwrap();
    (store, dir)
}

fn seed(store: &CatalogStore, provider: &str, model: &str, extra: Vec<(&str, Value)>) {
    let mut facts = BTreeMap::from([
        ("existence", json!("present")),
        ("model.family", json!("real-family")),
        ("model.open_weights", json!(false)),
        ("capability.reasoning", json!(true)),
    ]);
    facts.extend(extra);
    let eras: Vec<_> = facts
        .into_iter()
        .map(|(key, value)| NewEra {
            source: SourceId::ModelsDev,
            provider_id: provider.into(),
            model_id: model.into(),
            fact_key: FactKey::from_stored(key.into()),
            value_json: value.to_string(),
            boundary_at: Timestamp(1_000),
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        })
        .collect();
    store.append_eras(&eras).unwrap();
}

fn priced(units: i64) -> Value {
    json!({"state":"priced", "units":units, "exponent":9, "currency":"USD", "unit_provenance":"source"})
}

fn card() -> Vec<(&'static str, Value)> {
    vec![
        ("rate.input", priced(100)),
        ("rate.input.above_context.272000", priced(200)),
        ("rate.output", priced(300)),
        ("rate.output.above_context.272000", priced(400)),
        ("rate.cache_read", json!({"state":"stated_zero"})),
        (
            "rate.reasoning",
            json!({"state":"unpriced", "reason":"missing_rate"}),
        ),
        ("rate.cache_write", priced(500)),
        ("rate.cache_write.above_context.272000", priced(600)),
        ("rate.input.mode.fast", priced(700)),
        ("rate.output.mode.fast.above_context.272000", priced(800)),
    ]
}

fn get(store: &CatalogStore, args: Value) -> Result<CatalogGetResponse, RouteError> {
    let body = json!({"name":"catalog.get", "arguments":args}).to_string();
    match serve_tool_call(store, body.as_bytes())? {
        ToolResponse::Catalog(response) => Ok(response),
        other => panic!("catalog.get must return a catalog, got {other:?}"),
    }
}

fn request(provider: &str, model: Option<&str>, method: &str, prefix: Option<&str>) -> Value {
    let mut args = json!({"provider_id":provider, "auth_method":method});
    if let Some(model) = model {
        args["model_id"] = json!(model);
    }
    if let Some(prefix) = prefix {
        args["fact_prefixes"] = json!([prefix]);
    }
    args
}

#[test]
fn auth_method_accepts_exactly_the_five_pinned_strings() {
    assert_eq!(
        billing_planes::AUTH_METHODS,
        &["apikey", "chatgpt", "oauth", "antigravity", "gateway"]
    );
    let (store, _dir) = store();
    seed(&store, "openai", "test-model", card());
    for method in ["apikey", "chatgpt", "oauth", "antigravity", "gateway"] {
        let result = get(&store, request("openai", None, method, None));
        assert!(
            result.is_ok() || result.unwrap_err().code == "no_coverage",
            "{method} must parse"
        );
    }
    for method in ["bogus", "", "APIKEY", "ChatGPT", "api_key"] {
        let error = get(&store, request("openai", None, method, None)).unwrap_err();
        assert_eq!(error.code, "bad_request");
        assert_eq!(error.message, format!("unknown auth_method {method:?}; expected apikey, chatgpt, oauth, antigravity or gateway"));
    }
}

#[test]
fn auth_method_requires_a_provider() {
    let (store, _dir) = store();
    for model in [None, Some("test-model")] {
        let mut args = json!({"auth_method":"chatgpt"});
        if let Some(model) = model {
            args["model_id"] = json!(model);
        }
        let error = get(&store, args).unwrap_err();
        assert_eq!(error.code, "bad_request");
        assert_eq!(error.message, "auth_method requires provider_id");
    }
}

#[test]
fn plane_views_refuse_every_point_in_time_read() {
    let (store, _dir) = store();
    seed(&store, "openai", "test-model", card());
    for model in [None, Some("test-model")] {
        for method in ["chatgpt", "apikey"] {
            for at_ms in [0, 1_000, i64::MAX] {
                let mut args = request("openai", model, method, None);
                args["at_ms"] = json!(at_ms);
                let error = get(&store, args).unwrap_err();
                assert_eq!(error.code, "no_coverage");
                assert_eq!(
                    error.message,
                    "billing plane views are current-only: auth_method cannot be used with at_ms"
                );
            }
        }
    }
}

#[test]
fn undeclared_subscription_planes_never_fall_back_to_api_rows() {
    let (store, _dir) = store();
    for provider in ["openai", "google"] {
        seed(&store, provider, "test-model", card());
    }
    for (provider, method) in [
        ("openai", "oauth"),
        ("google", "antigravity"),
        ("unknown", "chatgpt"),
    ] {
        for model in [None, Some("test-model"), Some("unknown-model")] {
            let error = get(&store, request(provider, model, method, None)).unwrap_err();
            assert_eq!(error.code, "no_coverage");
            assert_eq!(error.message, format!("no billing plane declared for ({provider}, {method}); only an apikey read is served the API price card without one"));
        }
    }
}

#[test]
fn an_undeclared_api_plane_serves_the_unmodified_published_card_without_provenance() {
    let (store, _dir) = store();
    seed(&store, "openai", "test-model", card());
    for model in [None, Some("test-model")] {
        let response = get(&store, request("openai", model, "apikey", None)).unwrap();
        let plane = serde_json::to_value(response.plane.unwrap()).unwrap();
        assert_eq!(
            plane,
            json!({"provider_id":"openai", "auth_method":"apikey", "kind":"upstream"})
        );
        assert!(response.billing_rules.is_empty());
        for (key, value) in card() {
            assert_eq!(response.models["openai/test-model"][key], value);
        }
    }
}

#[test]
fn quota_proxies_mark_each_unmarked_rate_with_its_real_family() {
    let (store, _dir) = store();
    for (provider, method) in [("openai", "chatgpt"), ("anthropic", "oauth")] {
        let mut rates = card();
        rates.push(("rate.cached_input_audio", priced(400_000_000)));
        seed(&store, provider, "test-model", rates);
        for model in [None, Some("test-model")] {
            let response = get(&store, request(provider, model, method, Some("rate."))).unwrap();
            let facts = &response.models[&format!("{provider}/test-model")];
            assert!(facts.keys().all(|key| key.starts_with("rate.")));
            for key in [
                "rate.input",
                "rate.input.above_context.272000",
                "rate.output",
                "rate.output.above_context.272000",
                "rate.cache_read",
                "rate.reasoning",
                "rate.cached_input_audio",
            ] {
                assert_eq!(
                    facts[key]["inherited_from"],
                    json!({"provider_id":provider, "family":"real-family", "basis":"quota_proxy"}),
                    "{provider}/{key}"
                );
            }
            let plane = response.plane.unwrap();
            assert_eq!(plane.kind, "quota_proxy");
            assert_eq!(
                plane.source_ref.as_deref(),
                Some(if provider == "openai" {
                    billing_planes::OPENAI_CHATGPT_SOURCE
                } else {
                    "https://support.anthropic.com/en/articles/9797557"
                })
            );
            assert_eq!(plane.review_by_ms, Some(1796515200000));
        }
    }
}

#[test]
fn quota_proxies_drop_all_mode_rates() {
    let (store, _dir) = store();
    seed(&store, "anthropic", "test-model", card());
    for model in [None, Some("test-model")] {
        let response = get(&store, request("anthropic", model, "oauth", None)).unwrap();
        assert!(response.models["anthropic/test-model"]
            .keys()
            .all(|key| !key.contains(".mode.")));
        assert_eq!(
            response.models["anthropic/test-model"]["rate.input"]["units"],
            100
        );
    }
    let control = get(
        &store,
        json!({"provider_id":"anthropic", "model_id":"test-model"}),
    )
    .unwrap();
    assert_eq!(
        control.models["anthropic/test-model"]["rate.input.mode.fast"]["units"],
        700
    );
}

#[test]
fn kimi_k31_pro_quota_proxy_borrows_the_api_reasoning_rule() {
    let (store, _dir) = store();
    seed(
        &store,
        "moonshotai",
        "kimi-k3.1-pro",
        vec![("rate.output", priced(300))],
    );
    seed(
        &store,
        "kimi-code-plan-global",
        "k3.1-pro",
        vec![("model.canonical_id", json!("moonshotai/kimi-k3.1-pro"))],
    );
    let read = get(
        &store,
        request(
            "kimi-code-plan-global",
            Some("k3.1-pro"),
            "apikey",
            Some("rate."),
        ),
    )
    .unwrap();
    let identity = "kimi-code-plan-global/k3.1-pro";
    let facts = &read.models[identity];
    assert_eq!(
        facts["rate.output"]["inherited_from"]["provider_id"],
        "moonshotai"
    );
    assert_eq!(
        facts.get("rate.reasoning"),
        Some(&json!({"state":"billed_as", "key":"rate.output"}))
    );
    let claim = &read.fact_overrides[0];
    assert_eq!(claim.model, identity);
    assert_eq!(claim.provider_id, "moonshotai");
    assert_eq!(claim.model_id, "*");
}

#[test]
fn kimi_plan_borrows_only_exact_canonical_api_cards_without_changing_storage() {
    let (store, _dir) = store();
    let provider = "kimi-code-plan-global";
    let api_card: Vec<_> = card()
        .into_iter()
        .map(|(key, mut value)| {
            if value["state"] == "priced" {
                value["unit_provenance"] = json!({"kind":"stated"});
            }
            (key, value)
        })
        .collect();
    seed(&store, "moonshotai", "kimi-k3", api_card.clone());
    // Similar names and a matching family must not rescue an absent target.
    seed(&store, "moonshotai", "kimi-for-coding", card());
    for (id, canonical) in [
        ("k3", Some("moonshotai/kimi-k3")),
        ("k3-256k", Some("moonshotai/kimi-k3")),
        ("kimi-for-coding", Some("moonshotai/kimi-k2.8-preview")),
        ("wrong-prefix", Some("other/kimi-k3")),
        ("no-canonical", None),
    ] {
        let mut extra = vec![
            ("model.family", json!("plan-family")),
            ("rate.input", json!({"state":"stated_zero"})),
            ("rate.output", json!({"state":"stated_zero"})),
            (
                "rate.output.above_context.999",
                json!({"state":"stated_zero"}),
            ),
        ];
        if let Some(canonical) = canonical {
            extra.push(("model.canonical_id", json!(canonical)));
        }
        seed(&store, provider, id, extra);
    }
    let plain = || {
        let mut response = get(&store, json!({"provider_id":provider})).unwrap();
        response.resolved_at_ms = 0;
        serde_json::to_vec(&response).unwrap()
    };
    let before = plain();
    let version = store.catalog_version().unwrap();
    for prefix in [None, Some("rate."), Some("rate.input"), Some("capability.")] {
        let bulk = get(&store, request(provider, None, "apikey", prefix)).unwrap();
        assert_eq!(bulk.plane.as_ref().unwrap().kind, "quota_proxy");
        for id in [
            "k3",
            "k3-256k",
            "kimi-for-coding",
            "wrong-prefix",
            "no-canonical",
        ] {
            let named = get(&store, request(provider, Some(id), "apikey", prefix)).unwrap();
            let key = format!("{provider}/{id}");
            let facts = &named.models[&key];
            assert_eq!(facts, &bulk.models[&key]);
            if prefix == Some("capability.") {
                assert_eq!(
                    facts,
                    &BTreeMap::from([("capability.reasoning".into(), json!(true))])
                );
                continue;
            }
            if id == "k3" || id == "k3-256k" {
                let mut count = 0;
                for (key, mut expected) in api_card.clone() {
                    if key.contains(".mode.") {
                        assert!(!facts.contains_key(key), "{id}/{key}");
                        continue;
                    }
                    if prefix.is_some_and(|p| !key.starts_with(p)) {
                        continue;
                    }
                    name_the_provenance_of_a_pre_0_11_row(&mut expected);
                    expected["inherited_from"] = json!({
                        "provider_id":"moonshotai", "model_id":"kimi-k3",
                        "family":"real-family", "basis":"quota_proxy"
                    });
                    assert_eq!(facts[key], expected, "{id}/{key}");
                    let _: fusiform_protocol::RateValue =
                        serde_json::from_value(facts[key].clone()).unwrap();
                    count += 1;
                }
                assert_eq!(
                    facts.keys().filter(|k| k.starts_with("rate.")).count(),
                    count
                );
                assert!(!facts.contains_key("rate.output.above_context.999"));
            } else {
                let mut count = 0;
                for (key, value) in facts.iter().filter(|(key, _)| key.starts_with("rate.")) {
                    assert_eq!(
                        value,
                        &json!({"state":"unpriced", "reason":"no_api_card"}),
                        "{id}/{key}"
                    );
                    let _: fusiform_protocol::RateValue =
                        serde_json::from_value(value.clone()).unwrap();
                    count += 1;
                }
                assert!(count > 0);
            }
        }
    }
    assert_eq!(
        before,
        plain(),
        "plane reads must not change ordinary wire bytes"
    );
    assert_eq!(version, store.catalog_version().unwrap());
    let replay = get(&store, json!({"provider_id":provider, "at_ms":1000})).unwrap();
    assert_eq!(
        replay.models["kimi-code-plan-global/k3"]["rate.input"],
        json!({"state":"stated_zero"})
    );
    let error = get(
        &store,
        json!({"provider_id":provider, "auth_method":"apikey", "at_ms":1000}),
    )
    .unwrap_err();
    assert_eq!(error.code, "no_coverage");
    assert_canonical_borrow_uses_the_required_empty_family_sentinel();
}

#[test]
fn kimi_highspeed_quota_proxy_uses_its_own_api_card() {
    let (store, _dir) = store();
    for (id, input, output) in [("kimi-k3.1-pro", 2, 10), ("kimi-k3.1-pro-highspeed", 4, 20)] {
        seed(
            &store,
            "moonshotai",
            id,
            vec![
                ("rate.input", priced(input)),
                ("rate.output", priced(output)),
            ],
        );
    }
    for id in ["k3.1-pro", "k3.1-pro-highspeed"] {
        seed(
            &store,
            "kimi-code-plan-global",
            id,
            vec![
                ("model.canonical_id", json!("moonshotai/kimi-k3.1-pro")),
                ("rate.input", json!({"state":"stated_zero"})),
            ],
        );
    }
    let response = get(
        &store,
        request(
            "kimi-code-plan-global",
            Some("k3.1-pro-highspeed"),
            "apikey",
            Some("rate."),
        ),
    )
    .unwrap();
    let facts = &response.models["kimi-code-plan-global/k3.1-pro-highspeed"];
    assert_eq!(facts["rate.input"]["units"], 4);
    assert_eq!(facts["rate.output"]["units"], 20);
    assert_eq!(
        facts["rate.input"]["inherited_from"]["model_id"],
        "kimi-k3.1-pro-highspeed"
    );
}

fn seed_speed_twins(store: &CatalogStore) {
    for (id, multiplier) in [("kimi-k3.1-pro", 1), ("kimi-k3.1-pro-highspeed", 2)] {
        let rates = card()
            .into_iter()
            .map(|(key, mut value)| {
                if let Some(units) = value["units"].as_i64() {
                    value["units"] = json!(units * multiplier);
                }
                (key, value)
            })
            .collect();
        seed(store, "moonshotai", id, rates);
    }
    for id in [
        "k3.1-pro",
        "k3.1-pro-highspeed",
        "kimi-for-coding",
        "kimi-for-coding-highspeed",
    ] {
        seed(
            store,
            "kimi-code-plan-global",
            id,
            vec![
                ("model.canonical_id", json!("moonshotai/kimi-k3.1-pro")),
                ("rate.input", json!({"state":"stated_zero"})),
                ("rate.output", json!({"state":"stated_zero"})),
            ],
        );
    }
}

#[test]
fn speed_twin_does_not_duplicate_the_reasoning_addition_under_fast() {
    let (store, _dir) = store();
    let provider = "moonshotai";
    let model = "kimi-k3.1-pro";
    seed(&store, provider, model, vec![("rate.output", priced(300))]);
    seed(
        &store,
        provider,
        "kimi-k3.1-pro-highspeed",
        vec![("rate.output", priced(600))],
    );
    for args in [
        json!({"provider_id":provider, "model_id":model}),
        json!({"provider_id":provider}),
    ] {
        let read = get(&store, args).unwrap();
        let facts = &read.models["moonshotai/kimi-k3.1-pro"];
        assert_eq!(
            facts["rate.reasoning"],
            json!({"state":"billed_as", "key":"rate.output"})
        );
        assert_eq!(facts["rate.output.mode.fast"]["units"], 600);
        assert!(!facts.contains_key("rate.reasoning.mode.fast"));
    }
}

#[test]
fn speed_twins_plain_named_bulk_and_paged_reads_agree() {
    let (store, _dir) = store();
    seed_speed_twins(&store);
    let whole = get(&store, json!({})).unwrap();
    let paged = get(&store, json!({"paged":true})).unwrap();
    assert_eq!(whole.speed_twins.len(), 2);
    assert_eq!(whole.speed_twins, paged.speed_twins);
    for (provider, base, twin, expected_rate) in [
        (
            "kimi-code-plan-global",
            "k3.1-pro",
            "k3.1-pro-highspeed",
            json!({"state":"stated_zero"}),
        ),
        (
            "moonshotai",
            "kimi-k3.1-pro",
            "kimi-k3.1-pro-highspeed",
            priced(200),
        ),
    ] {
        let bulk = get(&store, json!({"provider_id":provider})).unwrap();
        for id in [base, twin] {
            let identity = format!("{provider}/{id}");
            let named = get(&store, json!({"provider_id":provider,"model_id":id})).unwrap();
            for other in [&bulk, &whole, &paged] {
                assert_eq!(named.models[&identity], other.models[&identity]);
            }
            assert_eq!(named.speed_twins.len(), 1);
            assert_eq!(named.speed_twins, bulk.speed_twins);
            assert!(whole.speed_twins.contains(&named.speed_twins[0]));
        }
        let base_facts = &bulk.models[&format!("{provider}/{base}")];
        assert_eq!(base_facts["route.mode_wire_ids"], json!({"fast":twin}));
        assert_eq!(base_facts["route.mode_labels"], json!({"fast":"Highspeed"}));
        assert_eq!(base_facts["rate.input.mode.fast"], expected_rate);
        if provider == "moonshotai" {
            assert_eq!(base_facts["rate.reasoning.mode.fast"]["state"], "unpriced");
        }
        assert_eq!(
            bulk.models[&format!("{provider}/{twin}")]["model.mode_of"],
            json!({"model_id":base,"mode":"fast"})
        );
    }
    for id in ["kimi-for-coding", "kimi-for-coding-highspeed"] {
        let facts = &whole.models[&format!("kimi-code-plan-global/{id}")];
        assert!(!facts.contains_key("model.mode_of"));
        assert!(!facts.contains_key("route.mode_wire_ids"));
    }
}

#[test]
fn speed_twins_plane_rates_preserve_origins_and_filter_last() {
    let (store, _dir) = store();
    seed_speed_twins(&store);
    let provider = "kimi-code-plan-global";
    let bulk = get(&store, request(provider, None, "apikey", None)).unwrap();
    let base = &bulk.models["kimi-code-plan-global/k3.1-pro"];
    let twin = &bulk.models["kimi-code-plan-global/k3.1-pro-highspeed"];
    for (key, value) in twin.iter().filter(|(key, _)| key.starts_with("rate.")) {
        assert!(
            !key.contains(".mode."),
            "API modes must still be stripped: {key}"
        );
        let dimension = key.strip_prefix("rate.").unwrap();
        let mode_key = match dimension.split_once('.') {
            Some((dimension, suffix)) => format!("rate.{dimension}.mode.fast.{suffix}"),
            None => format!("rate.{dimension}.mode.fast"),
        };
        assert_eq!(&base[&mode_key], value, "{mode_key}");
        assert_eq!(
            value["inherited_from"]["model_id"],
            "kimi-k3.1-pro-highspeed"
        );
    }
    assert_eq!(base["rate.input"]["units"], 100);
    assert_eq!(base["rate.input.mode.fast"]["units"], 200);
    assert_eq!(
        base["rate.input.mode.fast"]["inherited_from"]["basis"],
        "quota_proxy"
    );
    for id in ["k3.1-pro", "k3.1-pro-highspeed"] {
        for prefix in [
            "rate.input.mode.fast",
            "route.mode_",
            "model.mode_of",
            "capability.",
        ] {
            let named = get(&store, request(provider, Some(id), "apikey", Some(prefix))).unwrap();
            let filtered_bulk =
                get(&store, request(provider, None, "apikey", Some(prefix))).unwrap();
            let identity = format!("{provider}/{id}");
            assert_eq!(named.models[&identity], filtered_bulk.models[&identity]);
            assert!(named.models[&identity]
                .keys()
                .all(|key| key.starts_with(prefix)));
            if prefix == "capability." {
                assert!(named.speed_twins.is_empty());
            }
        }
    }
}

#[test]
fn speed_twins_preserve_plain_inherited_rates_and_stored_history() {
    let (store, _dir) = store();
    seed_speed_twins(&store);
    let mut value = priced(123);
    value["inherited_from"] =
        json!({"provider_id":"origin","family":"weights","basis":"open_weights"});
    store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "kimi-code-plan-global".into(),
            model_id: "k3.1-pro-highspeed".into(),
            fact_key: FactKey::from_stored("rate.input".into()),
            value_json: value.to_string(),
            boundary_at: Timestamp(2_000),
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        }])
        .unwrap();
    let before = store.era_count(SourceId::ModelsDev).unwrap();
    let version = store.catalog_version().unwrap();
    let args = json!({"provider_id":"kimi-code-plan-global","model_id":"k3.1-pro","fact_prefixes":["rate.input.mode.fast"]});
    let response = get(&store, args).unwrap();
    let rate = &response.models["kimi-code-plan-global/k3.1-pro"]["rate.input.mode.fast"];
    assert_eq!(rate["units"], 123);
    assert_eq!(rate["inherited_from"]["provider_id"], "origin");
    assert_eq!(response.speed_twins.len(), 1);
    let replay = get(
        &store,
        json!({"provider_id":"kimi-code-plan-global","at_ms":1000}),
    )
    .unwrap();
    assert!(replay.speed_twins.is_empty());
    for facts in replay.models.values() {
        assert!(!facts.contains_key("model.mode_of"));
        assert!(!facts.contains_key("route.mode_wire_ids"));
        assert!(!facts.contains_key("route.mode_labels"));
    }
    assert_eq!(
        replay.models["kimi-code-plan-global/k3.1-pro"]["rate.input.mode.fast"],
        json!({"state":"stated_zero"})
    );
    assert_eq!(store.era_count(SourceId::ModelsDev).unwrap(), before);
    assert_eq!(store.catalog_version().unwrap(), version);
}

#[test]
fn speed_twins_dated_api_rates_follow_twin_eras() {
    const LAUNCH: i64 = 1_792_782_357_000; // 2026-10-23 19:05:57 UTC
    let (store, _dir) = store();
    let provider = "moonshotai";
    let base = "kimi-k3.1-pro";
    let twin = "kimi-k3.1-pro-highspeed";
    let input_key = "rate.input.mode.fast";
    let output_key = "rate.output.mode.fast";
    let input = priced(4_000_000_000);
    let output = priced(20_000_000_000);
    let model = format!("{provider}/{base}");
    store
        .record_observation(&fusiform_store::NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1_000),
            outcome: fusiform_core::ObservationOutcome::Seeded,
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    seed(
        &store,
        provider,
        base,
        vec![("rate.input", priced(2_000_000_000))],
    );
    let append = |at, model_id: &str, key: &str, value: &Value| {
        store
            .append_eras(&[NewEra {
                source: SourceId::ModelsDev,
                provider_id: provider.into(),
                model_id: model_id.into(),
                fact_key: FactKey::from_stored(key.into()),
                value_json: value.to_string(),
                boundary_at: Timestamp(at),
                boundary_kind: BoundaryKind::Seed,
                observation_id: None,
            }])
            .unwrap();
    };
    append(LAUNCH, twin, "existence", &json!("present"));
    append(LAUNCH, twin, "rate.input", &input);
    append(LAUNCH, twin, "rate.output", &output);
    append(LAUNCH + 1, base, "rate.input", &priced(3_000_000_000));
    let read = |at, named| {
        let mut args =
            json!({"provider_id":provider,"at_ms":at,"fact_prefixes":[input_key,output_key]});
        if named {
            args["model_id"] = json!(base);
        }
        get(&store, args).unwrap().models.remove(&model).unwrap()
    };
    let history = |selector: Value| {
        let mut args = json!({"provider_id":provider,"model_id":base});
        args.as_object_mut()
            .unwrap()
            .extend(selector.as_object().unwrap().clone());
        let body = json!({"name":"catalog.history", "arguments":args});
        match serve_tool_call(&store, body.to_string().as_bytes()).unwrap() {
            ToolResponse::History(response) => serde_json::to_value(response).unwrap(),
            other => panic!("catalog.history must return a history, got {other:?}"),
        }
    };
    let before = read(LAUNCH - 1, true);
    assert!(!before.contains_key(input_key));
    assert!(!before.contains_key(output_key));
    let dated = read(LAUNCH, true);
    assert_eq!(dated.get(input_key), Some(&input));
    assert_eq!(dated.get(output_key), Some(&output));
    assert_eq!(dated, read(LAUNCH, false));

    let served = history(json!({"fact_key":input_key,"served":true}));
    assert_eq!(served["eras"], json!([]));
    assert_eq!(
        served["served"],
        json!([
            {"from_ms":1000,"until_ms":LAUNCH,"state":"absent"},
            {"from_ms":LAUNCH,"until_ms":null,"state":"value","value":input,"source":"speed_twin"},
        ])
    );
    let listed = history(json!({"fact_prefix":"rate.input.mode"}));
    assert_eq!(listed["fact_keys"], json!([input_key]));
    let listed_served = history(json!({"fact_prefix":"rate.input.mode","served":true}));
    assert_eq!(listed_served["served_by_key"][input_key], served["served"]);

    let input_withdrawn = LAUNCH + 60_000;
    let twin_withdrawn = LAUNCH + 120_000;
    append(
        input_withdrawn,
        twin,
        "rate.input",
        &json!({"state":"unpriced","reason":"missing_rate"}),
    );
    append(twin_withdrawn, twin, "existence", &json!("absent"));
    let dated = read(input_withdrawn, true);
    assert!(!dated.contains_key(input_key));
    assert_eq!(dated.get(output_key), Some(&output));
    assert!(read(twin_withdrawn, true).is_empty());
    let served = history(json!({"fact_key":input_key,"served":true}));
    assert_eq!(served["served"][1]["until_ms"], input_withdrawn);
    assert_eq!(
        served["served"][2],
        json!({"from_ms":input_withdrawn,"until_ms":null,"state":"absent"})
    );
    let served = history(json!({"fact_key":output_key,"served":true}));
    assert_eq!(served["served"][1]["until_ms"], twin_withdrawn);
    assert_eq!(
        served["served"][2],
        json!({"from_ms":twin_withdrawn,"until_ms":null,"state":"absent"})
    );
}

#[test]
fn speed_twins_do_not_advertise_an_absent_twin() {
    let (store, _dir) = store();
    seed(
        &store,
        "kimi-code-plan-global",
        "k3.1-pro",
        vec![("rate.input", priced(1))],
    );
    let response = get(
        &store,
        json!({"provider_id":"kimi-code-plan-global","model_id":"k3.1-pro"}),
    )
    .unwrap();
    let facts = &response.models["kimi-code-plan-global/k3.1-pro"];
    assert!(!facts.contains_key("route.mode_wire_ids"));
    assert!(!facts.contains_key("rate.input.mode.fast"));
    assert!(response.speed_twins.is_empty());
}

#[test]
fn kimi_quota_proxy_drops_borrowed_mode_rates() {
    let (store, _dir) = store();
    seed(&store, "moonshotai", "kimi-k3", card());
    seed(
        &store,
        "kimi-code-plan-global",
        "k3",
        vec![
            ("model.canonical_id", json!("moonshotai/kimi-k3")),
            ("rate.input", json!({"state":"stated_zero"})),
        ],
    );
    for model in [None, Some("k3")] {
        for prefix in [None, Some("rate."), Some("rate.input.mode.")] {
            let response = get(
                &store,
                request("kimi-code-plan-global", model, "apikey", prefix),
            )
            .unwrap();
            let facts = &response.models["kimi-code-plan-global/k3"];
            assert!(!facts.contains_key("rate.input.mode.fast"));
            assert!(!facts.contains_key("rate.output.mode.fast.above_context.272000"));
            assert!(facts.keys().all(|key| !key.contains(".mode.")));
        }
    }
    let control = get(
        &store,
        json!({"provider_id":"moonshotai", "model_id":"kimi-k3"}),
    )
    .unwrap();
    assert_eq!(
        control.models["moonshotai/kimi-k3"]["rate.input.mode.fast"]["units"],
        700
    );
    assert_eq!(
        control.models["moonshotai/kimi-k3"]["rate.output.mode.fast.above_context.272000"]["units"],
        800
    );
}

/// OpenCode Go sells its subscription as an API key, and its plane names
/// its own provider in `rates_from_provider`. That must mark the row's own
/// published rates in place, exactly as the Anthropic oauth plane does,
/// and never follow `model.canonical_id` to another provider's card. The
/// Kimi plane, whose `rates_from_provider` names a different provider, is
/// the control that still borrows.
#[test]
fn an_own_provider_price_source_marks_its_own_rates_and_borrows_nothing() {
    let (store, _dir) = store();
    let provider = "opencode-go";
    let mut extra = card();
    extra.push(("model.canonical_id", json!("zai/test-model")));
    seed(&store, provider, "test-model", extra);
    // A present card at the canonical target, priced differently, so a
    // borrow would show up as different units and a model_id marker.
    seed(&store, "zai", "test-model", vec![("rate.input", priced(9))]);
    seed(
        &store,
        "moonshotai",
        "kimi-k3",
        vec![("rate.input", priced(7))],
    );
    seed(
        &store,
        "kimi-code-plan-global",
        "k3",
        vec![
            ("model.canonical_id", json!("moonshotai/kimi-k3")),
            ("rate.input", json!({"state":"stated_zero"})),
        ],
    );
    let plain = || {
        let mut response = get(&store, json!({"provider_id":provider})).unwrap();
        response.resolved_at_ms = 0;
        serde_json::to_vec(&response).unwrap()
    };
    let before = plain();
    let marker = json!({"provider_id":provider, "family":"real-family", "basis":"quota_proxy"});
    for model in [None, Some("test-model")] {
        for prefix in [None, Some("rate.")] {
            let response = get(&store, request(provider, model, "apikey", prefix)).unwrap();
            assert_eq!(
                serde_json::to_value(response.plane.as_ref().unwrap()).unwrap(),
                json!({
                    "provider_id":provider, "auth_method":"apikey", "kind":"quota_proxy",
                    "source_ref":"https://opencode.ai/v2/docs/console/go/",
                    "review_by_ms":1797811200000_i64
                })
            );
            assert!(response.billing_rules.is_empty());
            let facts = &response.models["opencode-go/test-model"];
            let mut count = 0;
            for (key, mut expected) in card() {
                if key.contains(".mode.") {
                    assert!(!facts.contains_key(key), "{key}");
                    continue;
                }
                expected["inherited_from"] = marker.clone();
                assert_eq!(facts[key], expected, "{key}");
                count += 1;
            }
            assert_eq!(
                facts.keys().filter(|k| k.starts_with("rate.")).count(),
                count
            );
        }
    }
    assert_eq!(before, plain(), "a plain read must keep its bytes");
    let control = get(
        &store,
        json!({"provider_id":provider, "model_id":"test-model"}),
    )
    .unwrap();
    for (key, value) in card() {
        assert_eq!(control.models["opencode-go/test-model"][key], value);
    }

    let kimi = get(
        &store,
        request("kimi-code-plan-global", Some("k3"), "apikey", Some("rate.")),
    )
    .unwrap();
    let rate = &kimi.models["kimi-code-plan-global/k3"]["rate.input"];
    assert_eq!(rate["units"], 7);
    assert_eq!(
        rate["inherited_from"],
        json!({
            "provider_id":"moonshotai", "model_id":"kimi-k3",
            "family":"real-family", "basis":"quota_proxy"
        })
    );
}

fn assert_canonical_borrow_uses_the_required_empty_family_sentinel() {
    let (store, _dir) = store();
    for id in ["absent-family", "null-family"] {
        let mut facts = vec![
            ("existence", json!("present")),
            (
                "rate.input",
                json!({"state":"priced", "units":100, "exponent":9, "currency":"USD", "unit_provenance":{"kind":"stated"}}),
            ),
        ];
        if id == "null-family" {
            facts.push(("model.family", Value::Null));
        }
        let eras: Vec<_> = facts
            .into_iter()
            .map(|(key, value)| NewEra {
                source: SourceId::ModelsDev,
                provider_id: "moonshotai".into(),
                model_id: id.into(),
                fact_key: FactKey::from_stored(key.into()),
                value_json: value.to_string(),
                boundary_at: Timestamp(1_000),
                boundary_kind: BoundaryKind::Seed,
                observation_id: None,
            })
            .collect();
        store.append_eras(&eras).unwrap();
        seed(
            &store,
            "kimi-code-plan-global",
            id,
            vec![
                ("model.canonical_id", json!(format!("moonshotai/{id}"))),
                ("rate.input", json!({"state":"stated_zero"})),
            ],
        );
    }
    let bulk = get(
        &store,
        request("kimi-code-plan-global", None, "apikey", Some("rate.")),
    )
    .unwrap();
    for id in ["absent-family", "null-family"] {
        let named = get(
            &store,
            request("kimi-code-plan-global", Some(id), "apikey", Some("rate.")),
        )
        .unwrap();
        let identity = format!("kimi-code-plan-global/{id}");
        assert_eq!(named.models[&identity], bulk.models[&identity]);
        let rate = &named.models[&identity]["rate.input"];
        assert_eq!(rate["state"], "priced");
        assert_eq!(rate["units"], 100);
        assert_eq!(
            rate["inherited_from"],
            json!({
                "provider_id":"moonshotai", "model_id":id, "family":"", "basis":"quota_proxy"
            })
        );
        let _: fusiform_protocol::RateValue = serde_json::from_value(rate.clone()).unwrap();
    }
}

#[test]
fn reasoning_addition_keeps_open_weight_price_inheritance() {
    let (store, _dir) = store();
    seed(
        &store,
        "zai",
        "open-weight",
        vec![("rate.input", priced(100)), ("rate.output", priced(200))],
    );
    seed(
        &store,
        "openai",
        "open-weight",
        vec![
            ("model.family", json!("glm")),
            ("model.open_weights", json!(true)),
        ],
    );
    let read = get(
        &store,
        json!({"provider_id":"openai", "model_id":"open-weight"}),
    )
    .unwrap();
    let facts = &read.models["openai/open-weight"];
    for (key, units) in [("rate.input", 100), ("rate.output", 200)] {
        assert_eq!(facts[key]["units"], units);
        assert_eq!(facts[key]["inherited_from"]["provider_id"], "zai");
    }
    assert_eq!(
        facts["rate.reasoning"],
        json!({"state":"billed_as", "key":"rate.output"})
    );
    assert_eq!(read.fact_overrides[0].provider_id, "openai");
}

#[test]
fn quota_proxies_preserve_the_first_hop_of_inherited_rates() {
    let (store, _dir) = store();
    seed(
        &store,
        "zai",
        "test-model",
        vec![
            ("rate.input", priced(100)),
            ("rate.input.above_context.272000", priced(200)),
        ],
    );
    seed(
        &store,
        "openai",
        "test-model",
        vec![
            ("model.family", json!("glm")),
            ("model.open_weights", json!(true)),
        ],
    );
    for model in [None, Some("test-model")] {
        let response = get(
            &store,
            request("openai", model, "chatgpt", Some("rate.cache_write")),
        )
        .unwrap();
        assert_eq!(
            response.models["openai/test-model"]["rate.cache_write"],
            json!({"state":"billed_as", "key":"rate.input"})
        );
        assert_eq!(
            response.models["openai/test-model"]["rate.cache_write.above_context.272000"],
            json!({"state":"billed_as", "key":"rate.input.above_context.272000"})
        );
        let response = get(
            &store,
            request("openai", model, "chatgpt", Some("rate.input")),
        )
        .unwrap();
        for key in ["rate.input", "rate.input.above_context.272000"] {
            assert_eq!(
                response.models["openai/test-model"][key]["inherited_from"],
                json!({"provider_id":"zai", "family":"glm", "basis":"open_weights"})
            );
        }
    }
}

#[test]
fn quota_rules_replace_published_prices_and_narrow_disclosures_last() {
    let (store, _dir) = store();
    seed(&store, "openai", "test-model", card());
    for model in [None, Some("test-model")] {
        for prefix in [
            None,
            Some("rate."),
            Some("rate.cache_write"),
            Some("rate.cache_write.above_context."),
        ] {
            let response = get(&store, request("openai", model, "chatgpt", prefix)).unwrap();
            let facts = &response.models["openai/test-model"];
            let expected_keys: Vec<_> = if prefix == Some("rate.cache_write.above_context.") {
                vec!["rate.cache_write.above_context.272000"]
            } else {
                vec!["rate.cache_write", "rate.cache_write.above_context.272000"]
            };
            assert_eq!(
                response
                    .billing_rules
                    .iter()
                    .map(|r| r.fact_key.as_str())
                    .collect::<Vec<_>>(),
                expected_keys
            );
            for rule in &response.billing_rules {
                let target = if rule.fact_key == "rate.cache_write" {
                    "rate.input"
                } else {
                    "rate.input.above_context.272000"
                };
                assert_eq!(
                    facts[&rule.fact_key],
                    json!({"state":"billed_as", "key":target})
                );
                assert_eq!(
                    serde_json::to_value(rule).unwrap(),
                    json!({"model":"openai/test-model", "fact_key":rule.fact_key, "state":"billed_as", "as_class":"input", "source_ref":"https://help.openai.com/en/articles/20001106", "established_by":"fusiform", "established_at_ms":1791331200000_i64, "review_by_ms":1796515200000_i64})
                );
            }
            if let Some(prefix) = prefix {
                assert!(facts.keys().all(|key| key.starts_with(prefix)));
            }
        }
        for prefix in ["capability.", "no_such_fact."] {
            let response = get(&store, request("openai", model, "chatgpt", Some(prefix))).unwrap();
            assert!(response.billing_rules.is_empty());
            let wire = serde_json::to_value(&response).unwrap();
            assert!(wire.get("billing_rules").is_none());
            assert!(response
                .models
                .values()
                .all(|facts| facts.keys().all(|key| key.starts_with(prefix))));
        }
    }
}

#[test]
fn missing_billed_as_targets_are_disclosed_as_not_established() {
    let (store, _dir) = store();
    seed(
        &store,
        "openai",
        "unpriced-model",
        vec![("rate.cache_write.above_context.42", priced(500))],
    );
    for model in [None, Some("unpriced-model")] {
        let response = get(
            &store,
            request("openai", model, "chatgpt", Some("rate.cache_write")),
        )
        .unwrap();
        assert_eq!(response.billing_rules.len(), 2);
        for key in ["rate.cache_write", "rate.cache_write.above_context.42"] {
            assert_eq!(
                response.models["openai/unpriced-model"][key],
                json!({"state":"unpriced", "reason":"not_established"})
            );
            let rule = response
                .billing_rules
                .iter()
                .find(|r| r.fact_key == key)
                .unwrap();
            assert_eq!(rule.state, "not_established");
            assert_eq!(rule.as_class.as_deref(), Some("input"));
        }
    }
}

#[test]
fn upstream_rules_never_overwrite_a_published_key_but_fill_each_missing_tier() {
    let (store, _dir) = store();
    for provider in ["xai", "deepseek"] {
        let published = [
            priced(500),
            json!({"state":"stated_zero"}),
            json!({"state":"unpriced", "reason":"missing_rate"}),
        ];
        for (index, value) in published.into_iter().enumerate() {
            let model_id = format!("test-{index}");
            seed(
                &store,
                provider,
                &model_id,
                vec![
                    ("rate.input", priced(100)),
                    ("rate.input.above_context.1", priced(200)),
                    ("rate.input.above_context.2", priced(300)),
                    ("rate.cache_write", value.clone()),
                    ("rate.cache_write.above_context.1", value.clone()),
                    ("rate.cache_write.above_context.3", value.clone()),
                ],
            );
            for model in [None, Some(model_id.as_str())] {
                let response = get(
                    &store,
                    request(provider, model, "apikey", Some("rate.cache_write")),
                )
                .unwrap();
                let identity = format!("{provider}/{model_id}");
                let facts = &response.models[&identity];
                for key in [
                    "rate.cache_write",
                    "rate.cache_write.above_context.1",
                    "rate.cache_write.above_context.3",
                ] {
                    assert_eq!(facts[key], value);
                }
                assert_eq!(
                    facts["rate.cache_write.above_context.2"],
                    json!({"state":"billed_as", "key":"rate.input.above_context.2"})
                );
                let applied: Vec<_> = response
                    .billing_rules
                    .iter()
                    .filter(|r| r.model == identity)
                    .collect();
                assert_eq!(applied.len(), 1);
                assert_eq!(applied[0].fact_key, "rate.cache_write.above_context.2");
                assert!(response.plane.unwrap().source_ref.is_some());
            }
        }
        seed(
            &store,
            provider,
            "no-write",
            vec![
                ("rate.input", priced(100)),
                ("rate.input.above_context.272000", priced(200)),
            ],
        );
        let response = get(
            &store,
            request(
                provider,
                Some("no-write"),
                "apikey",
                Some("rate.cache_write"),
            ),
        )
        .unwrap();
        assert_eq!(
            response.models[&format!("{provider}/no-write")]["rate.cache_write"],
            json!({"state":"billed_as", "key":"rate.input"})
        );
        assert_eq!(response.billing_rules.len(), 2);
    }
}

#[test]
fn named_and_bulk_alias_plane_reads_keep_the_family_and_filter_last() {
    let (store, _dir) = store();
    // Google has no declared Antigravity billing plane in production. Plant a
    // plane to exercise the alias on an allowed login without asserting billing.
    let _plane = billing_planes::plant_antigravity();
    seed(&store, "google", "gemini-3.8-flash", card());
    let alias = "google/antigravity-gemini-3.8-flash";
    for model in [None, Some("antigravity-gemini-3.8-flash")] {
        for prefix in [
            None,
            Some("rate."),
            Some("rate.input.above_context."),
            Some("capability."),
        ] {
            let refused = get(&store, request("google", model, "apikey", prefix)).unwrap();
            assert!(!refused.models.contains_key(alias));
            assert!(refused.aliased.is_empty());
            assert!(refused.login_methods.is_empty());

            let response = get(&store, request("google", model, "antigravity", prefix)).unwrap();
            let facts = &response.models[alias];
            assert_eq!(response.login_methods.len(), 1);
            assert_eq!(response.login_methods[0].model, alias);
            assert_eq!(response.login_methods[0].auth_methods, ["antigravity"]);
            assert_eq!(response.plane.unwrap().kind, "upstream");
            assert!(response.billing_rules.is_empty());
            if let Some(prefix) = prefix {
                assert!(facts.keys().all(|key| key.starts_with(prefix)));
            }
            for (key, value) in facts.iter().filter(|(key, _)| key.starts_with("rate.")) {
                assert_eq!(
                    value["inherited_from"],
                    json!({"provider_id":"google", "model_id":"gemini-3.8-flash", "family":"real-family", "basis":"alias"}),
                    "{key}"
                );
            }
        }
    }
}

#[test]
fn plane_views_preserve_non_rate_facts() {
    let (store, _dir) = store();
    seed(
        &store,
        "anthropic",
        "claude-sonnet-4-5",
        vec![
            ("limit.context", json!(1_000_000)),
            ("rate.input", priced(100)),
        ],
    );
    for model in [None, Some("claude-sonnet-4-5")] {
        let response = get(&store, request("anthropic", model, "oauth", Some("limit."))).unwrap();
        assert_eq!(
            response.models["anthropic/claude-sonnet-4-5"]["limit.context"],
            1_000_000
        );
        assert!(response.billing_rules.is_empty());
    }
}

#[test]
fn a_read_without_auth_method_keeps_the_pre_plane_wire_bytes() {
    // These bytes pin a plain `openai` read. The shipped curated table now
    // adds an `openai` decision row to it, which is unrelated to billing
    // planes, so this test reads against an empty curated table.
    let _curated = crate::curated_models::plant(
        r#"{"schema":"curated-models/v1","purpose":"empty","models":[]}"#,
    );
    let (store, _dir) = store();
    seed(
        &store,
        "openai",
        "test-model",
        vec![("rate.input", priced(100))],
    );
    for model in [None, Some("test-model")] {
        let mut args = json!({"provider_id":"openai"});
        if let Some(model) = model {
            args["model_id"] = json!(model);
        }
        let mut response = get(&store, args).unwrap();
        // The response clock and store version differ on every run, so pin
        // them. The exact bytes include current provider temperature bounds,
        // but a read that names no login method (`auth_method`) must still
        // carry no billing-plane fields: no `plane`, no `billing_rules`, and
        // no rate annotations a plane read would add.
        response.resolved_at_ms = 1_000;
        response.catalog_version = 1;
        // Check the endpoint default and its disclosure separately from
        // the pinned fact bytes; neither may add `plane` or `billing_rules`.
        assert_eq!(
            response
                .models
                .get_mut("openai/test-model")
                .unwrap()
                .remove("wire.api"),
            Some(json!({"value":"https://api.openai.com/v1","from":"curated"}))
        );
        assert_eq!(response.wire_corrections.len(), 1);
        assert_eq!(response.wire_corrections[0].provider, "openai");
        assert_eq!(response.wire_corrections[0].model, "test-model");
        response.wire_corrections.clear();
        // The plain API's reasoning rule is curated independently of planes.
        // Check it separately so the previous bytes remain pinned.
        assert_eq!(
            response
                .models
                .get_mut("openai/test-model")
                .unwrap()
                .remove("rate.reasoning"),
            Some(json!({"state":"billed_as", "key":"rate.output"}))
        );
        assert_eq!(response.fact_overrides.len(), 1);
        assert_eq!(response.fact_overrides[0].fact_key, "rate.reasoning");
        response.fact_overrides.clear();
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"source":"models.dev","resolved_at_ms":1000,"catalog_version":1,"models":{"openai/test-model":{"capability.reasoning":true,"existence":"present","limit.temperature_max":2,"limit.temperature_min":0,"model.family":"real-family","model.open_weights":false,"rate.input":{"currency":"USD","exponent":9,"state":"priced","unit_provenance":"source","units":100}}}}"#
        );
    }
}

#[test]
fn plane_views_never_change_stored_rows_or_catalog_version() {
    let (store, _dir) = store();
    seed(&store, "openai", "test-model", card());
    let stored_before = store
        .read_model(SourceId::ModelsDev, "openai", "test-model", None)
        .unwrap()
        .0
        .unwrap()
        .facts;
    let version = store.catalog_version().unwrap();
    for model in [None, Some("test-model")] {
        let response = get(&store, request("openai", model, "chatgpt", None)).unwrap();
        assert_eq!(
            response.models["openai/test-model"]["rate.cache_write"]["state"],
            "billed_as"
        );
    }
    let stored_after = store
        .read_model(SourceId::ModelsDev, "openai", "test-model", None)
        .unwrap()
        .0
        .unwrap()
        .facts;
    assert_eq!(stored_before, stored_after);
    assert_eq!(store.catalog_version().unwrap(), version);
    assert!(stored_after
        .values()
        .all(|value| !value.contains("billed_as")
            && !value.contains("not_established")
            && !value.contains("quota_proxy")));
}
