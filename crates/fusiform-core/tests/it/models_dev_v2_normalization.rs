use std::collections::BTreeMap;

use fusiform_core::normalize::{v2::FindingKey, NormalizeV2Outcome, RefusedFactScope};
use fusiform_core::upstream_facts::{facts_of, FactKey};
use fusiform_core::{normalize_models_dev_v2_with, ChargeUnitEvidence, NormalizeError};
use fusiform_protocol::NormalizationFindingsWire;
use serde_json::{json, Value};

fn document() -> Value {
    json!({"provider": {"name": "Provider", "models": {"model": {
        "type": "transcription", "name": "Example",
        "description": "Unserved presentation copy",
        "capabilities": {"tools": {"supported": false}, "reasoning": {"supported": false}},
        "modalities": {"input": ["audio/wav", "audio/mp3"], "output": ["text"]},
        "limit": {"context": 2048, "output": 256},
        "cost": {"text": {"output": {"price": 0.125}}, "audio": {"input": {"price": 0.625, "unit": "minute"}}},
        "api": {"transcriptions": {"base_url": "https://example.com"}}
    }}}})
}

fn normalize(document: &Value) -> NormalizeV2Outcome {
    normalize_models_dev_v2_with(
        &serde_json::to_vec(document).unwrap(),
        &ChargeUnitEvidence::none(),
    )
    .unwrap()
}

fn facts(outcome: &NormalizeV2Outcome) -> BTreeMap<String, Value> {
    let model = outcome.catalog.models().next().unwrap();
    facts_of(model)
        .values
        .into_iter()
        .map(|(key, value)| {
            (
                key.as_str().to_owned(),
                serde_json::from_str(&value).unwrap(),
            )
        })
        .collect()
}

fn assert_unaffected(facts: &BTreeMap<String, Value>) {
    assert_eq!(facts["model.name"], "Example");
    assert_eq!(facts["limit.context"], 2048);
    assert_eq!(facts["limit.output"], 256);
    assert_eq!(facts["capability.input_modalities"], json!(["audio"]));
    assert_eq!(facts["capability.output_modalities"], json!(["text"]));
    assert_eq!(
        facts["rate.output"],
        json!({"state": "priced", "units": 125000000, "exponent": 9, "currency": "USD"})
    );
    assert!(!facts.contains_key("model.kind"));
}

#[test]
fn v2_unknown_key_is_ignored_and_counted_per_path() {
    // The first model in source order is not first in the sorted catalog.
    let mut first = document()["provider"]["models"]["model"].clone();
    let mut second = first.clone();
    first["future_key"] = json!(true);
    second["future_key"] = json!(false);
    let bytes = [
        r#"{"provider":{"models":{"z-first":"#,
        &first.to_string(),
        r#","a-second":"#,
        &second.to_string(),
        "}}}",
    ]
    .concat();
    let outcome =
        normalize_models_dev_v2_with(bytes.as_bytes(), &ChargeUnitEvidence::none()).unwrap();
    let wire = NormalizationFindingsWire {
        observation_id: 1,
        counts: outcome.finding_counts(),
    };
    assert_eq!(wire.counts, BTreeMap::from([("unknown_key".to_owned(), 2)]));
    assert_eq!(
        outcome.findings.len(),
        1,
        "description is already declared unread"
    );
    let finding = &outcome.findings[0];
    assert!(
        matches!(&finding.key, FindingKey::UnknownKey { object_kind, path } if object_kind == "Model" && path == "model.future_key")
    );
    assert_eq!(finding.occurrences, 2);
    assert_eq!(finding.first_model_id.as_deref(), Some("z-first"));
    let facts = facts(&outcome);
    assert_unaffected(&facts);
    assert_eq!(facts["wire.api"]["value"], "https://example.com");
    assert!(!facts.contains_key("model.future_key"));
}

#[test]
fn v2_unknown_model_type_is_served_verbatim() {
    let mut input = document();
    input["provider"]["models"]["model"]["type"] = json!("future-type");
    let outcome = normalize(&input);
    let facts = facts(&outcome);
    assert_unaffected(&facts);
    assert_eq!(facts["model.type"], "future-type");
    assert_eq!(facts["wire.api"]["value"], "https://example.com");
    assert!(outcome.catalog.models().next().unwrap().refused.is_empty());
    assert_eq!(
        outcome.findings[0].key,
        FindingKey::UnknownModelType {
            value: "future-type".to_owned()
        }
    );
}

#[test]
fn v2_unknown_default_api_refuses_only_wire_facts_without_falling_back() {
    let mut input = document();
    input["provider"]["models"]["model"]["api"] = json!({
        "future-protocol": {"base_url": "https://unknown.invalid", "sdk": {"aisdk": "future-sdk"}},
        "transcriptions": {"base_url": "https://example.com"}
    });
    let outcome = normalize(&input);
    let facts = facts(&outcome);
    assert_unaffected(&facts);
    assert_eq!(facts["model.type"], "transcription");
    assert_eq!(facts["rate.input_audio.per_second"]["per"], 60);
    let model = outcome.catalog.models().next().unwrap();
    assert_eq!(
        model.refused,
        ["api", "npm", "shape"]
            .into_iter()
            .map(|field| RefusedFactScope::Fact(FactKey::wire(field)))
            .collect()
    );
    assert!(facts.keys().all(|key| !key.starts_with("wire.")));
    assert_eq!(
        outcome.findings[0].key,
        FindingKey::UnknownApiProtocol {
            value: "future-protocol".to_owned()
        }
    );
}

#[test]
fn v2_unknown_cost_unit_leaves_only_that_price_unserved() {
    let mut input = document();
    input["provider"]["models"]["model"]["cost"]["audio"]["input"]["unit"] = json!("hour");
    let outcome = normalize(&input);
    let facts = facts(&outcome);
    assert_unaffected(&facts);
    assert_eq!(facts["wire.api"]["value"], "https://example.com");
    assert!(facts.keys().all(|key| !key.starts_with("rate.input_audio")));
    assert_eq!(
        outcome.findings[0].key,
        FindingKey::UnknownCostUnit {
            role: "audio.input".to_owned(),
            value: "hour".to_owned()
        }
    );
}

#[test]
fn v2_only_admitted_units_are_served_and_amounts_are_not_converted() {
    for (unit, expected) in [
        ("minute", Some(60)),
        ("second", Some(1)),
        ("image", None),
        ("megapixel", None),
    ] {
        let mut input = document();
        let cost = &mut input["provider"]["models"]["model"]["cost"];
        cost["audio"]["input"]["unit"] = json!(unit);
        cost["text"]["input"] = json!({"price": 0.75, "unit": "character"});
        let outcome = normalize(&input);
        let facts = facts(&outcome);
        assert_unaffected(&facts);
        assert_eq!(
            facts["rate.input.per_character"],
            json!({"state": "priced", "units": 750000000, "exponent": 9, "currency": "USD", "per": 1000000})
        );
        match expected {
            Some(per) => assert_eq!(
                facts["rate.input_audio.per_second"],
                json!({"state": "priced", "units": 625000000, "exponent": 9, "currency": "USD", "per": per})
            ),
            None => {
                assert!(facts.keys().all(|key| !key.starts_with("rate.input_audio")));
                assert_eq!(outcome.finding_counts()["unserved_cost"], 1);
            }
        }
    }
}

#[test]
fn v2_token_audio_and_cached_audio_need_evidence() {
    let mut input = document();
    input["provider"]["models"]["model"]["cost"]["audio"]["input"] =
        json!({"unit": "token", "price": 5, "cache": {"read": 0.5}});
    let outcome = normalize(&input);
    assert_eq!(facts(&outcome)["rate.input_audio"]["state"], "unpriced");
    assert!(!facts(&outcome).contains_key("rate.cached_input_audio"));
    let outcome = normalize_models_dev_v2_with(
        &serde_json::to_vec(&input).unwrap(),
        &ChargeUnitEvidence::input_audio_per_token(["provider"]),
    )
    .unwrap();
    let facts = facts(&outcome);
    assert_eq!(facts["rate.input_audio"]["units"], 5000000000i64);
    assert!(facts["rate.input_audio"].get("per").is_none());
    assert_eq!(facts["rate.cached_input_audio"]["units"], 500000000);
    assert!(!facts.contains_key("rate.cache_read"));
}

#[test]
fn v2_mode_audio_needs_directional_evidence_and_does_not_reprice_unit_audio() {
    let mut input = document();
    let row = model(&mut input);
    row["cost"]["audio"]["output"] = json!({"unit": "token", "price": 2});
    row["experimental"]["modes"]["fast"] = json!({"cost": {"audio": {
        "input": {"unit": "token", "price": 1, "cache": {"read": 0.25}},
        "output": {"unit": "token", "price": 3}
    }}});
    add_refused_mode(&mut input);
    let outcome = normalize(&input);
    assert_mode_refused(&outcome, "ultrafast", "region");
    let actual = facts(&outcome);
    assert_unaffected(&actual);
    assert_eq!(actual["rate.input_audio.per_second"]["per"], 60);
    assert_eq!(actual["rate.output_audio"]["state"], "unpriced");
    assert!(actual.keys().all(|key| !key.ends_with(".mode.fast")));

    let evidence = ChargeUnitEvidence::input_audio_per_token(["provider"])
        .with_output_audio_per_token(["provider"]);
    let outcome =
        normalize_models_dev_v2_with(&serde_json::to_vec(&input).unwrap(), &evidence).unwrap();
    let actual = facts(&outcome);
    assert_eq!(actual["rate.input_audio.per_second"]["per"], 60);
    assert_eq!(actual["rate.input_audio.mode.fast"]["units"], 1_000_000_000);
    assert_eq!(
        actual["rate.cached_input_audio.mode.fast"]["units"],
        250_000_000
    );
    assert_eq!(
        actual["rate.output_audio.mode.fast"]["units"],
        3_000_000_000u64
    );
}

#[test]
fn v2_unconditional_price_uses_base_key_beside_a_refused_mode() {
    let mut input = document();
    add_refused_mode(&mut input);
    let outcome = normalize(&input);
    assert_mode_refused(&outcome, "ultrafast", "region");
    assert_unaffected(&facts(&outcome));
}

#[test]
fn v2_context_gt_and_gte_use_exclusive_selector_keys() {
    let mut input = document();
    model(&mut input)["cost"]["text"]["output"]["overrides"] = json!([
        {"when": {"context": {"gte": 32}}, "price": 1},
        {"when": {"context": {"gt": 32}}, "price": 2}
    ]);
    add_refused_mode(&mut input);
    let outcome = normalize(&input);
    assert_mode_refused(&outcome, "ultrafast", "region");
    let actual = facts(&outcome);
    assert_unaffected(&actual);
    assert_eq!(
        actual["rate.output.above_context.31"]["units"],
        1_000_000_000
    );
    assert_eq!(
        actual["rate.output.above_context.32"]["units"],
        2_000_000_000u64
    );
    assert!(!actual.contains_key("rate.output.above_context.33"));
}

#[test]
fn v2_service_tier_override_uses_the_tier_unless_it_has_a_declared_mode() {
    let mut input = document();
    model(&mut input)["cost"]["text"]["output"]["overrides"] = json!([
        {"when": {"service_tier": {"equals": "priority"}}, "price": 2}
    ]);
    add_refused_mode(&mut input);
    let outcome = normalize(&input);
    assert_mode_refused(&outcome, "ultrafast", "region");
    assert_eq!(
        facts(&outcome)["rate.output.mode.priority"]["units"],
        2_000_000_000
    );

    model(&mut input)["experimental"]["modes"]["fast"] = json!({"service_tier": "priority"});
    let outcome = normalize(&input);
    let actual = facts(&outcome);
    assert_unaffected(&actual);
    assert_eq!(actual["rate.output.mode.fast"]["units"], 2_000_000_000);
    assert!(!actual.contains_key("rate.output.mode.priority"));
}

#[test]
fn v2_service_tier_multiplier_is_exact_and_prices_only_named_roles() {
    let mut input = document();
    let cost = &mut model(&mut input)["cost"];
    cost["text"]["input"] = json!({"price": 0.0000000015, "cache": {"read": 1}});
    cost["multipliers"] = json!([
        {"when": {"service_tier": {"equals": "priority"}}, "factor": {"text": {"input": {"price": 2}}}}
    ]);
    add_refused_mode(&mut input);
    let outcome = normalize(&input);
    assert_mode_refused(&outcome, "ultrafast", "region");
    let actual = facts(&outcome);
    assert_unaffected(&actual);
    // Rounding the base to two nanodollars before multiplying would give four.
    assert_eq!(actual["rate.input"]["units"], 2);
    assert_eq!(actual["rate.input.mode.priority"]["units"], 3);
    assert!(!actual.contains_key("rate.output.mode.priority"));
    assert!(!actual.contains_key("rate.cache_read.mode.priority"));
}

#[test]
fn v2_declared_multiplier_mode_keeps_v1_base_scaling_without_tier_keys() {
    let mut input = document();
    let row = model(&mut input);
    row["experimental"]["modes"]["fast"] = json!({"service_tier": "priority"});
    row["cost"]["text"]["output"]["overrides"] = json!([
        {"when": {"context": {"gt": 32}}, "price": 1}
    ]);
    row["cost"]["multipliers"] = json!([
        {"when": {"service_tier": {"equals": "priority"}}, "factor": {"text": 2}},
        {"when": {"service_tier": {"equals": "flex"}}, "factor": {"text": 2}}
    ]);
    let outcome = normalize(&input);
    assert_mode_refused(&outcome, "flex", "tiered price");
    let actual = facts(&outcome);
    assert_unaffected(&actual);
    assert_eq!(actual["rate.output.mode.fast"]["units"], 250_000_000);
    assert_eq!(
        actual["rate.output.above_context.32"]["units"],
        1_000_000_000
    );
    assert!(!actual.contains_key("rate.output.mode.priority"));
    assert!(actual.keys().all(|key| !key.contains(".mode.fast.")));
}

#[test]
fn v2_empty_multiplier_factor_refuses_its_mode() {
    let mut input = document();
    model(&mut input)["cost"]["multipliers"] = json!([
        {"when": {"service_tier": {"equals": "priority"}}, "factor": {}}
    ]);
    let outcome = normalize(&input);
    assert_mode_refused(&outcome, "priority", "names no priced base roles");
    assert_unaffected(&facts(&outcome));
}

#[test]
fn v2_same_spelling_different_tiers_do_not_chain_multipliers() {
    let mut input = document();
    let row = model(&mut input);
    row["experimental"]["modes"]["fast"] = json!({"service_tier": "priority"});
    row["cost"]["multipliers"] = json!([
        {"when": {"service_tier": {"equals": "priority"}}, "factor": {"text": 2}},
        {"when": {"service_tier": {"equals": "fast"}}, "factor": {"text": 2}}
    ]);
    add_refused_mode(&mut input);
    let outcome = normalize(&input);
    assert_mode_refused(&outcome, "ultrafast", "region");
    let actual = facts(&outcome);
    assert_unaffected(&actual);
    assert_eq!(actual["rate.output.mode.fast"]["units"], 250_000_000);
    assert!(!actual.contains_key("rate.output.mode.priority"));

    model(&mut input)["cost"]["multipliers"][1]["factor"]["text"] = json!(3);
    let outcome = normalize(&input);
    assert_mode_refused(&outcome, "fast", "conflicting prices");
    assert_unaffected(&facts(&outcome));
}

#[test]
fn v2_conflicting_named_and_direct_mode_rates_refuse_the_mode() {
    let mut input = document();
    let row = model(&mut input);
    row["experimental"]["modes"]["fast"] = json!({
        "service_tier": "priority", "cost": {"text": {"output": {"price": 1}}}
    });
    row["cost"]["text"]["output"]["overrides"] = json!([
        {"when": {"service_tier": {"equals": "priority"}}, "price": 2}
    ]);
    let outcome = normalize(&input);
    assert_mode_refused(&outcome, "fast", "conflicting prices");
    let actual = facts(&outcome);
    assert_unaffected(&actual);
    assert!(!actual.contains_key("rate.output.mode.priority"));
}

#[test]
fn v2_context_and_service_tier_cannot_be_combined() {
    let mut input = document();
    model(&mut input)["cost"]["text"]["output"]["overrides"] = json!([
        {"when": {"service_tier": {"equals": "priority"}, "context": {"gt": 32}}, "price": 2}
    ]);
    let outcome = normalize(&input);
    assert_mode_refused(&outcome, "priority", "combined conditions");
    let actual = facts(&outcome);
    assert_unaffected(&actual);
    assert!(actual.keys().all(|key| !key.contains(".above_context.")));
}

fn model(document: &mut Value) -> &mut Value {
    &mut document["provider"]["models"]["model"]
}

fn add_refused_mode(document: &mut Value) {
    model(document)["experimental"]["modes"]["ultrafast"] = json!({"cost": {"text": {"output": {
        "price": 8, "overrides": [{"when": {"region": {"equals": "us"}}, "price": 9}]
    }}}});
}

fn assert_mode_refused(outcome: &NormalizeV2Outcome, mode: &str, reason: &str) {
    let model = outcome.catalog.models().next().unwrap();
    let scope = RefusedFactScope::ModeRates(Some(mode.to_owned()));
    assert!(model.refused.contains(&scope), "{:?}", outcome.findings);
    let old_key = FactKey::from_stored(format!("rate.output.mode.{mode}"));
    assert!(scope.covers(&old_key), "held price must not be withdrawn");
    assert!(!facts(outcome).contains_key(old_key.as_str()));
    assert_refused_finding(outcome, reason);
}

fn assert_refused_finding(outcome: &NormalizeV2Outcome, reason: &str) {
    assert!(
        outcome.findings.iter().any(|finding| {
            matches!(&finding.key, FindingKey::RefusedPrice {detail, ..} if detail.contains(reason))
                && finding.first_model_id.as_deref() == Some("model")
        }),
        "{reason} missing from {:?}",
        outcome.findings
    );
}

fn assert_refused_base_condition(when: Value, reason: &str) {
    let mut input = document();
    let row = model(&mut input);
    row["cost"]["text"]["output"]["overrides"] = json!([{"when": when, "price": 2}]);
    row["experimental"]["modes"]["fast"] = json!({"cost": {"text": {"output": {"price": 3}}}});
    let outcome = normalize(&input);
    let model = outcome.catalog.models().next().unwrap();
    assert_eq!(model.refused, [RefusedFactScope::BaseAndTierRates].into());
    assert!(
        RefusedFactScope::BaseAndTierRates.covers(&FactKey::from_stored("rate.output".to_owned()))
    );
    let actual = facts(&outcome);
    assert_eq!(actual["rate.output.mode.fast"]["units"], 3_000_000_000u64);
    assert_eq!(actual["limit.context"], 2048);
    assert!(actual
        .keys()
        .filter(|key| key.starts_with("rate."))
        .eq(["rate.output.mode.fast"]));
    assert_refused_finding(&outcome, reason);
}

macro_rules! refused_condition {
    ($test:ident, $reason:literal, $when:tt) => {
        #[test]
        fn $test() {
            assert_refused_base_condition(json!($when), $reason);
        }
    };
}

refused_condition!(v2_context_lt_is_refused, "lt", {"context": {"lt": 32}});
refused_condition!(v2_context_lte_is_refused, "lte", {"context": {"lte": 32}});
refused_condition!(v2_context_eq_is_refused, "eq", {"context": {"eq": 32}});
refused_condition!(v2_context_equals_is_refused, "equals", {"context": {"equals": 32}});
refused_condition!(v2_context_band_is_refused, "lte", {"context": {"gt": 32, "lte": 64}});
refused_condition!(v2_unknown_operator_is_named_in_the_refusal, "future_operator", {"context": {"future_operator": 32}});
refused_condition!(v2_unknown_key_is_named_in_the_refusal, "future_key", {"future_key": {"equals": true}});
refused_condition!(v2_region_is_refused, "region", {"region": {"equals": "us"}});
refused_condition!(v2_utc_hours_is_refused, "utc_hours", {"utc_hours": {"gt": "02:00"}});
refused_condition!(v2_utc_days_is_refused, "utc_days", {"utc_days": {"in": ["sun"]}});
refused_condition!(v2_date_is_refused, "date", {"date": {"gte": "2027-01-01"}});
refused_condition!(v2_holiday_is_refused, "holiday", {"holiday": {"equals": "US"}});
refused_condition!(v2_resolution_is_refused, "resolution", {"resolution": {"equals": "720p"}});
refused_condition!(v2_quality_is_refused, "quality", {"quality": {"equals": "high"}});
refused_condition!(v2_duration_is_refused, "duration", {"duration": {"gt": 32}});

#[test]
fn v2_malformed_non_object_and_v1_documents_return_no_catalog() {
    let valid = document().to_string();
    let late_error = format!("{},\"zz-broken\":", valid.strip_suffix('}').unwrap());
    for input in [late_error.as_bytes(), b"{", b"{} []", br#"{"p":{"models":{"m":{"type":"chat","tool_call":true,"provider":{"api":"https://example.com"}}}}}"#] {
        assert!(matches!(normalize_models_dev_v2_with(input, &ChargeUnitEvidence::none()), Err(NormalizeError::DocumentParse { .. })));
    }
    assert_eq!(
        normalize_models_dev_v2_with(b"[]", &ChargeUnitEvidence::none()),
        Err(NormalizeError::NotAProviderMap)
    );
}
