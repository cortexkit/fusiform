//! Current-only sampling bounds documented by each serving provider.

use std::collections::BTreeMap;

const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;

use serde::Deserialize;
use serde_json::{Number, Value};

pub const KEYS: [&str; 2] = ["limit.temperature_min", "limit.temperature_max"];

#[derive(Clone, Debug, Deserialize)]
pub struct Range {
    pub provider_id: String,
    pub minimum: Number,
    pub maximum: Number,
    pub source_ref: String,
    pub quote: String,
    pub notes: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

pub type Ranges = BTreeMap<String, Range>;

pub fn load_reporting_rejects(document: &str) -> (Ranges, Vec<String>) {
    #[derive(Deserialize)]
    struct Document {
        schema: String,
        ranges: Vec<Value>,
    }
    let mut ranges = Ranges::new();
    let mut rejects = Vec::new();
    let doc: Document = match serde_json::from_str(document) {
        Ok(doc) => doc,
        Err(error) => return (ranges, vec![error.to_string()]),
    };
    if doc.schema != "temperature-ranges/v1" {
        return (ranges, vec!["unknown temperature-ranges schema".into()]);
    }
    for (index, raw) in doc.ranges.into_iter().enumerate() {
        let row: Range = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!("range {index}: {error}"));
                continue;
            }
        };
        if row.provider_id.trim().is_empty()
            || row.provider_id.contains('/')
            || row
                .minimum
                .as_f64()
                .zip(row.maximum.as_f64())
                .is_none_or(|(min, max)| min > max)
            || !row.source_ref.starts_with("https://")
            || row.quote.trim().is_empty()
            || row.notes.trim().is_empty()
            || row.established_at_ms <= 0
            || row.review_by_ms.checked_sub(row.established_at_ms) != Some(REVIEW_INTERVAL_MS)
        {
            rejects.push(format!(
                "range {index}: invalid bounds, provider or provenance"
            ));
            continue;
        }
        match ranges.entry(row.provider_id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(row);
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                rejects.push(format!(
                    "range {index}: duplicate provider {}",
                    row.provider_id
                ));
            }
        }
    }
    (ranges, rejects)
}

pub fn overdue_at(ranges: &Ranges, now_ms: i64) -> Vec<String> {
    ranges
        .values()
        .filter(|row| row.review_by_ms < now_ms)
        .map(|row| {
            format!(
                "{}: re-verify {} and re-date the 60-day review",
                row.provider_id, row.source_ref
            )
        })
        .collect()
}

/// Called only on current, non-alias rows after guarded replacements apply.
pub fn apply(rows: &Ranges, provider: &str, facts: &mut BTreeMap<String, Value>) {
    if facts.get("capability.temperature") == Some(&Value::Bool(false)) {
        return;
    }
    if let Some(row) = rows.get(provider) {
        // Keep the vendor's numeric representation, including a zero minimum.
        facts.insert(KEYS[0].into(), Value::Number(row.minimum.clone()));
        facts.insert(KEYS[1].into(), Value::Number(row.maximum.clone()));
    }
}
