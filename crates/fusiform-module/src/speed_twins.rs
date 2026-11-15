//! Sourced speed variants that expose the same model under a separate wire id.
//! Current reads join these claims; neither identity nor derived prices is stored.

use std::collections::{BTreeMap, BTreeSet};

const DOCUMENT: &str = include_str!("../data/speed-twins.json");
const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;
pub const KEYS: &[&str] = &["model.mode_of", "route.mode_wire_ids", "route.mode_labels"];

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeedTwin {
    pub provider_id: String,
    pub base_model_id: String,
    pub twin_model_id: String,
    pub mode: String,
    pub label: String,
    pub api_card: String,
    pub source_ref: String,
    pub quote: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

/// `(provider, base model, mode)` identifies one separately served speed tier.
pub type SpeedTwins = BTreeMap<(String, String, String), SpeedTwin>;

pub fn load_reporting_rejects(doc: &str) -> (SpeedTwins, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Doc {
        speed_twins: Vec<serde_json::Value>,
    }
    let doc: Doc = match serde_json::from_str(doc) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                BTreeMap::new(),
                vec![format!("speed twins unreadable: {error}")],
            )
        }
    };
    let mut rows = SpeedTwins::new();
    let mut rejects = Vec::new();
    let mut twins = BTreeSet::new();
    for raw in doc.speed_twins {
        let row: SpeedTwin = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!("speed twin did not parse: {error}"));
                continue;
            }
        };
        let key = (
            row.provider_id.clone(),
            row.base_model_id.clone(),
            row.mode.clone(),
        );
        let invalid = if [
            &row.provider_id,
            &row.base_model_id,
            &row.twin_model_id,
            &row.label,
            &row.api_card,
            &row.source_ref,
            &row.quote,
        ]
        .iter()
        .any(|field| field.trim().is_empty())
        {
            Some("empty identity, label, API card, source or quote")
        } else if !matches!(row.mode.as_str(), "fast" | "ultrafast") {
            Some("mode must be fast or ultrafast")
        } else if !row
            .api_card
            .split_once('/')
            .is_some_and(|(provider, model)| {
                !provider.is_empty()
                    && !model.is_empty()
                    && !model.contains('/')
                    && !row.api_card.chars().any(char::is_whitespace)
            })
        {
            Some("api_card must be provider/model")
        } else if row.established_at_ms <= 0
            || row.review_by_ms.checked_sub(row.established_at_ms) != Some(REVIEW_INTERVAL_MS)
        {
            Some("review_by_ms must be 60 days after established_at_ms")
        } else if rows.contains_key(&key)
            || twins.contains(&(row.provider_id.clone(), row.twin_model_id.clone()))
        {
            Some("duplicate base mode or twin identity")
        } else {
            None
        };
        if let Some(reason) = invalid {
            rejects.push(format!("speed twin {}/{}: {reason}", key.0, key.1));
        } else {
            twins.insert((row.provider_id.clone(), row.twin_model_id.clone()));
            rows.insert(key, row);
        }
    }
    let bases: BTreeSet<_> = rows
        .values()
        .map(|row| (row.provider_id.clone(), row.base_model_id.clone()))
        .collect();
    rows.retain(|_, row| {
        if bases.contains(&(row.provider_id.clone(), row.twin_model_id.clone())) {
            rejects.push(format!(
                "speed twin {}/{}: a twin is also a base",
                row.provider_id, row.twin_model_id
            ));
            false
        } else {
            true
        }
    });
    (rows, rejects)
}

/// Embedded curation is immutable for the process, so parse and validate it once.
pub fn rows() -> &'static SpeedTwins {
    static ROWS: std::sync::OnceLock<SpeedTwins> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

pub fn twin(provider: &str, model: &str) -> Option<&'static SpeedTwin> {
    rows()
        .values()
        .find(|row| row.provider_id == provider && row.twin_model_id == model)
}

pub fn missing_seed_rows(rows: &SpeedTwins, seed: &serde_json::Value) -> Vec<String> {
    rows.values()
        .flat_map(|row| {
            [&row.base_model_id, &row.twin_model_id]
                .into_iter()
                .filter(|model| seed[&row.provider_id]["models"].get(*model).is_none())
                .map(|model| {
                    format!(
                        "{}/{model}: speed twin identity is missing from the seed",
                        row.provider_id
                    )
                })
        })
        .collect()
}

pub fn native_mode_collisions(
    rows: &SpeedTwins,
    catalog: &fusiform_core::normalize::NormalizedCatalog,
) -> Vec<String> {
    rows.values().filter(|row| catalog.models().any(|model| {
        model.key.provider_id == row.provider_id && model.key.model_id == row.base_model_id
            && model.rates.iter().any(|rate| matches!(&rate.condition, fusiform_core::RateCondition::Mode { name } if name == &row.mode))
    })).map(|row| format!("{}/{}: native .mode.{} collides with a speed twin; remove or correct the curated pair", row.provider_id, row.base_model_id, row.mode)).collect()
}

pub fn overdue_at(rows: &SpeedTwins, now_ms: i64) -> Vec<String> {
    rows.values().filter(|row| row.review_by_ms < now_ms).map(|row| {
        format!("{}/{} -> {}: re-verify {} and re-date established_at_ms and review_by_ms (60 days later), or remove the speed twin", row.provider_id, row.base_model_id, row.twin_model_id, row.source_ref)
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> serde_json::Value {
        serde_json::from_str(DOCUMENT).unwrap()
    }

    #[test]
    fn shipped_speed_twins_load_and_match_the_seed() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.len(), 1);
        let seed = serde_json::from_str(include_str!("../data/models-dev-seed.json")).unwrap();
        assert!(missing_seed_rows(&rows, &seed).is_empty());
        assert!(native_mode_collisions(&rows, &crate::seed::catalog().unwrap()).is_empty());
        assert!(twin("kimi-code-plan-global", "kimi-for-coding-highspeed").is_none());
    }

    #[test]
    fn planted_missing_base_and_twin_fire_the_seed_fence() {
        let mut seed: serde_json::Value =
            serde_json::from_str(include_str!("../data/models-dev-seed.json")).unwrap();
        for model in ["k3.1-pro", "k3.1-pro-highspeed"] {
            let saved = seed["kimi-code-plan-global"]["models"]
                .as_object_mut()
                .unwrap()
                .remove(model)
                .unwrap();
            let missing = missing_seed_rows(rows(), &seed);
            assert_eq!(missing.len(), 1);
            assert!(missing[0].contains(model));
            seed["kimi-code-plan-global"]["models"][model] = saved;
        }
    }

    #[test]
    fn planted_native_mode_fires_the_collision_fence() {
        let mut catalog = crate::seed::catalog().unwrap();
        let base = catalog
            .providers
            .iter_mut()
            .flat_map(|provider| &mut provider.models)
            .find(|model| {
                model.key.provider_id == "kimi-code-plan-global" && model.key.model_id == "k3.1-pro"
            })
            .unwrap();
        let mut rate = base.rates[0].clone();
        rate.condition = fusiform_core::RateCondition::Mode {
            name: "fast".into(),
        };
        base.rates.push(rate);
        let collisions = native_mode_collisions(rows(), &catalog);
        assert_eq!(collisions.len(), 1);
        assert!(collisions[0].contains(".mode.fast"));
    }

    #[test]
    fn planted_twin_as_base_is_rejected() {
        let mut doc = document();
        let mut chained = doc["speed_twins"][0].clone();
        chained["base_model_id"] = json!("k3.1-pro-highspeed");
        chained["twin_model_id"] = json!("another-model");
        doc["speed_twins"].as_array_mut().unwrap().push(chained);
        let (_, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("also a base"));
    }

    #[test]
    fn planted_unknown_mode_is_rejected() {
        let mut doc = document();
        doc["speed_twins"][0]["mode"] = json!("highspeed");
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert!(rows.is_empty());
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("fast or ultrafast"));
        doc["speed_twins"][0]["mode"] = json!("ultrafast");
        assert!(load_reporting_rejects(&doc.to_string()).1.is_empty());
    }

    #[test]
    fn malformed_duplicate_and_self_referential_claims_are_rejected() {
        for (key, value) in [
            ("quote", json!(" ")),
            ("api_card", json!("missing-provider")),
            ("review_by_ms", json!(1)),
            ("unknown", json!(true)),
            ("twin_model_id", json!("k3.1-pro")),
        ] {
            let mut doc = document();
            doc["speed_twins"][0][key] = value;
            assert_eq!(load_reporting_rejects(&doc.to_string()).1.len(), 1, "{key}");
        }
        let mut doc = document();
        let duplicate = doc["speed_twins"][0].clone();
        doc["speed_twins"].as_array_mut().unwrap().push(duplicate);
        assert_eq!(load_reporting_rejects(&doc.to_string()).1.len(), 1);
    }

    #[test]
    fn speed_twins_are_within_their_review_dates() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let overdue = overdue_at(rows(), now);
        assert!(
            overdue.is_empty(),
            "NOT A BUILD FAILURE. A speed twin review date passed.\n{}",
            overdue.join("\n")
        );
    }

    #[test]
    fn planted_overdue_claim_names_the_source_and_remedy() {
        let row = rows().values().next().unwrap();
        assert!(overdue_at(rows(), row.review_by_ms).is_empty());
        let overdue = overdue_at(rows(), row.review_by_ms + 1);
        assert_eq!(overdue.len(), 1);
        assert!(overdue[0].contains(&row.source_ref));
        assert!(overdue[0].contains("60 days later"));
    }
}
