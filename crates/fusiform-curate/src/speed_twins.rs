//! Sourced speed variants that expose the same model under a separate wire id.
//! Current reads join these claims; neither identity nor derived prices is stored.

use std::collections::{BTreeMap, BTreeSet};

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
