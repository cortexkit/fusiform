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
fn v2_deferred_overrides_are_not_silently_discarded() {
    let mut input = document();
    input["provider"]["models"]["model"]["cost"]["audio"]["input"]["overrides"] = json!([
        {"when": {"context": {"gt": 32}}, "price": 1},
        {"when": {"context": {"gte": 128}}, "price": 2}
    ]);
    let outcome = normalize(&input);
    assert_unaffected(&facts(&outcome));
    assert_eq!(outcome.finding_counts()["condition_deferred"], 2);
    assert_eq!(
        outcome.findings[0].key,
        FindingKey::ConditionDeferred {
            role: "audio.input".to_owned(),
            condition: "context".to_owned()
        }
    );
    assert!(!facts(&outcome)
        .keys()
        .any(|key| key.contains("above_context")));
}

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
