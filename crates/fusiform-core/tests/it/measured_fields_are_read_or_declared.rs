//! Inventory the embedded api.json bytes, not a hand-maintained description of
//! them. Serde ignores unknown fields, so a new upstream key otherwise looks
//! exactly like a field upstream never published.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

const SEED: &[u8] = include_bytes!("../../../fusiform-module/data/models-dev-seed.json");
#[test]
fn every_seed_key_is_read_or_declared_unread() {
    let outcome = fusiform_core::normalize_models_dev(SEED).expect("the embedded seed uses v2");
    assert!(outcome.catalog.model_count() > 1_000);
    let violations = v2_unaccounted(&outcome);
    assert!(
        violations.is_empty(),
        "v2 seed keys neither read nor declared unread: {violations:?}"
    );
}

fn saved_catalog(version: &str) -> Vec<u8> {
    saved_gzip(&format!("api-{version}-20261125.json.gz"))
}

fn saved_gzip(file: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/data/models-dev-v2/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    let output = std::process::Command::new("gzip")
        .args(["-dc", &path])
        .output()
        .expect("gzip must be available to read the saved catalog");
    assert!(output.status.success(), "gzip failed for {path}");
    output.stdout
}

// The v2 inventory classifies both read and declared-unread keys in the
// normalizer. This test reads that inventory instead of keeping its own
// allowlist, which could fall out of sync with the keys accepted at runtime.
fn v2_unaccounted(outcome: &fusiform_core::NormalizeV2Outcome) -> Vec<&str> {
    use fusiform_core::normalize::v2::FindingKey;
    outcome
        .findings
        .iter()
        .filter_map(|finding| match &finding.key {
            FindingKey::UnknownKey { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn every_saved_v2_key_is_read_or_declared_and_v2_only_facts_are_admitted() {
    use fusiform_core::normalize::is_servable_mode_name;
    use fusiform_core::upstream_facts::{facts_of, rate_key};
    use fusiform_core::{normalize_models_dev_v2_with, ChargeUnitEvidence};
    use fusiform_protocol::{served_fact_key_matches, SERVED_FACTS};

    let saved = saved_catalog("v2");
    let outcome = normalize_models_dev_v2_with(&saved, &ChargeUnitEvidence::none()).unwrap();
    let violations = v2_unaccounted(&outcome);
    assert!(
        violations.is_empty(),
        "v2 keys neither read nor declared unread: {violations:?}"
    );
    assert_eq!(outcome.catalog.model_count(), 10_913);

    let old_bytes = saved_catalog("v1");
    assert!(normalize_models_dev_v2_with(&old_bytes, &ChargeUnitEvidence::none()).is_err());
    let old: Value = serde_json::from_slice(&saved_gzip("v1-fixture.json.gz")).unwrap();
    let mut v2_only = 0;
    let mut unit_examples = BTreeSet::new();
    let mut chat_differences = Vec::new();
    let mut legacy_mode_rates = 0;
    let mut priority_models = 0;
    let mut priority_rates = 0;
    for provider in &outcome.catalog.providers {
        let prior_models = old["providers"][&provider.provider_id]["models"].as_object();
        for model in &provider.models {
            let prior_model = prior_models.and_then(|rows| rows.get(&model.key.model_id));
            let only_in_v2 = prior_model.is_none();
            v2_only += usize::from(only_in_v2);
            let projected = facts_of(model).values;
            if let Some(old) = prior_model.and_then(Value::as_object) {
                let actual: BTreeMap<_, _> = projected
                    .iter()
                    .filter(|(key, _)| key.as_str().contains(rate_key::MODE))
                    .map(|(key, value)| (key.as_str(), value.as_str()))
                    .collect();
                let mut priority = 0;
                for (key, value) in old.iter().filter(|(key, _)| key.contains(rate_key::MODE)) {
                    assert_eq!(
                        actual.get(key.as_str()).copied(),
                        value.as_str(),
                        "{key} on {:?}",
                        model.key
                    );
                    legacy_mode_rates += 1;
                    priority += usize::from(key.ends_with(".mode.priority"));
                }
                if priority != 0 {
                    assert_eq!(
                        actual
                            .keys()
                            .filter(|key| key.ends_with(".mode.priority"))
                            .count(),
                        priority
                    );
                    priority_models += 1;
                    priority_rates += priority;
                }
            }
            for (key, value) in projected {
                let key = key.as_str();
                let requires_unit = matches!(
                    key,
                    "rate.input.per_character" | "rate.input_audio.per_second"
                );
                if only_in_v2 {
                    let base =
                        if let Some((base, threshold)) = key.split_once(rate_key::ABOVE_CONTEXT) {
                            assert!(base.starts_with("rate."));
                            assert!(threshold
                                .parse::<u64>()
                                .is_ok_and(|n| n > 0 && n.to_string() == threshold));
                            base
                        } else if let Some((base, mode)) = key.split_once(rate_key::MODE) {
                            assert!(base.starts_with("rate."));
                            assert!(is_servable_mode_name(mode));
                            base
                        } else {
                            key
                        };
                    assert!(
                        requires_unit
                            || SERVED_FACTS
                                .iter()
                                .any(|fact| served_fact_key_matches(fact.key, base)),
                        "unadmitted fact {key} on {}/{}",
                        provider.provider_id,
                        model.key.model_id
                    );
                    assert_ne!(key, "model.kind");
                }
                if requires_unit || key == "rate.output" {
                    let json: Value = serde_json::from_str(&value).unwrap();
                    if json["state"] == "priced" {
                        unit_examples.insert((key.to_owned(), json["per"].as_u64()));
                    }
                }
                if key == "model.type"
                    && value == r#""chat""#
                    && prior_model.is_some_and(|row| row.get(key).is_none())
                {
                    chat_differences.push(serde_json::json!([
                        provider.provider_id,
                        model.key.model_id,
                        key,
                        null,
                        value,
                        "default_chat",
                    ]));
                }
            }
        }
    }
    assert_eq!(v2_only, 861);
    assert_eq!(legacy_mode_rates, 548);
    assert_eq!(priority_models, 38);
    assert_eq!(priority_rates, 114);
    let explained: Value =
        serde_json::from_slice(&saved_gzip("explained-differences.json.gz")).unwrap();
    let expected: Vec<_> = explained["normalized"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| {
            row.as_array()
                .and_then(|fields| fields.last())
                .and_then(Value::as_str)
                == Some("default_chat")
        })
        .collect();
    assert_eq!(chat_differences.len(), 9_611);
    assert_eq!(chat_differences.len(), expected.len());
    for (actual, expected) in chat_differences.iter().zip(expected) {
        assert_eq!(actual, expected, "chat type difference");
    }
    assert_eq!(
        unit_examples,
        BTreeSet::from([
            ("rate.output".to_owned(), None),
            ("rate.input.per_character".to_owned(), Some(1_000_000)),
            ("rate.input_audio.per_second".to_owned(), Some(1)),
            ("rate.input_audio.per_second".to_owned(), Some(60)),
        ])
    );
}

#[test]
fn v2_inventory_fence_detects_a_planted_key_in_an_unread_object() {
    use fusiform_core::{normalize_models_dev_v2_with, ChargeUnitEvidence};

    let bytes = br#"{"p":{"models":{"m":{"type":"chat","capabilities":{"cache":{"implicit":{"ttl":["1h"],"planted":1}}},"api":{}}}}}"#;
    let outcome = normalize_models_dev_v2_with(bytes, &ChargeUnitEvidence::none()).unwrap();
    assert_eq!(
        v2_unaccounted(&outcome),
        ["model.capabilities.cache.implicit.planted"]
    );
}
