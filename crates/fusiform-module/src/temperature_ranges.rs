//! Current-only sampling bounds documented by each serving provider.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{Number, Value};

const DOCUMENT: &str = include_str!("../data/temperature-ranges.json");
const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;
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

pub fn ranges() -> &'static Ranges {
    static RANGES: OnceLock<Ranges> = OnceLock::new();
    RANGES.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
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
pub fn apply(provider: &str, facts: &mut BTreeMap<String, Value>) {
    if facts.get("capability.temperature") == Some(&Value::Bool(false)) {
        return;
    }
    if let Some(row) = ranges().get(provider) {
        // Keep the vendor's numeric representation, including a zero minimum.
        facts.insert(KEYS[0].into(), Value::Number(row.minimum.clone()));
        facts.insert(KEYS[1].into(), Value::Number(row.maximum.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn review_gate(ranges: &Ranges, now: i64) -> Result<(), Vec<String>> {
        let overdue = overdue_at(ranges, now);
        if overdue.is_empty() {
            Ok(())
        } else {
            Err(overdue)
        }
    }

    #[test]
    fn temperature_ranges_shipped_rows_are_valid_and_reviewed() {
        let (ranges, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(ranges.len(), 5);
        let seed: Value =
            serde_json::from_str(include_str!("../data/models-dev-seed.json")).unwrap();
        for provider in ranges.keys() {
            assert!(seed.get(provider).is_some(), "{provider}");
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        review_gate(&ranges, now).unwrap();
        for (provider, min, max) in [
            ("openai", "0", "2"),
            ("google", "0.0", "2.0"),
            ("deepseek", "0", "2"),
            ("xai", "0", "2"),
            ("anthropic", "0.0", "1.0"),
        ] {
            let mut facts = BTreeMap::new();
            apply(provider, &mut facts);
            assert_eq!(facts[KEYS[0]].to_string(), min);
            assert_eq!(facts[KEYS[1]].to_string(), max);
        }
    }

    #[test]
    fn temperature_ranges_malformed_rows_are_counted_without_losing_valid_rows() {
        let original: Value = serde_json::from_str(DOCUMENT).unwrap();
        for (field, value) in [
            ("minimum", json!("0")),
            ("maximum", Value::Null),
            ("minimum", json!(3)),
            ("quote", json!("")),
            ("source_ref", json!("http://example.com")),
            ("provider_id", json!("openai/model")),
            ("review_by_ms", json!(1)),
        ] {
            let mut bad = original["ranges"][0].clone();
            bad[field] = value;
            let mut doc = original.clone();
            doc["ranges"].as_array_mut().unwrap().push(bad);
            let (ranges, rejects) = load_reporting_rejects(&doc.to_string());
            assert_eq!(ranges.len(), 5, "{field}");
            assert_eq!(rejects.len(), 1, "{field}: {rejects:?}");
        }
        let mut duplicate = original.clone();
        duplicate["ranges"]
            .as_array_mut()
            .unwrap()
            .push(original["ranges"][0].clone());
        assert_eq!(load_reporting_rejects(&duplicate.to_string()).1.len(), 1);
        assert_eq!(load_reporting_rejects("not json").1.len(), 1);
    }

    #[test]
    fn temperature_ranges_past_review_fails_the_shipped_review_gate() {
        let mut ranges = ranges().clone();
        let deadline = ranges["openai"].review_by_ms;
        review_gate(&ranges, deadline).unwrap();
        ranges.get_mut("openai").unwrap().review_by_ms = deadline - 1;
        let errors = review_gate(&ranges, deadline).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("openai"));
    }
}
