//! The embedded temperature ranges document and its local adapter.

pub use fusiform_curate::temperature_ranges::*;
#[cfg(test)]
use {serde_json::Value, std::collections::BTreeMap};

use std::sync::OnceLock;
const DOCUMENT: &str = include_str!("../data/temperature-ranges.json");
pub fn ranges() -> &'static Ranges {
    static RANGES: OnceLock<Ranges> = OnceLock::new();
    RANGES.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
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
            apply(super::ranges(), provider, &mut facts);
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
