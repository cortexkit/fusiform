//! The embedded wire corrections document and its local adapter.

pub use fusiform_curate::wire_corrections::*;
const DOCUMENT: &str = include_str!("../data/wire-corrections.json");
pub fn corrections() -> &'static WireCorrections {
    static ROWS: std::sync::OnceLock<WireCorrections> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shipped_corrections_load_without_rejects() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.len(), 6);
        assert!(rows.keys().all(|(provider, _)| provider != "opencode-go"));
        assert_eq!(rows.values().filter(|row| row.withheld).count(), 0);
    }

    #[test]
    fn authored_wire_corrections_reject_the_retired_field_even_when_empty() {
        let doc = json!({"corrections": [], "complete_providers": null});
        let (_, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rejects, ["unknown field `complete_providers`"]);
    }

    #[test]
    fn malformed_and_duplicate_corrections_are_reported() {
        let mut doc: serde_json::Value = serde_json::from_str(DOCUMENT).unwrap();
        let row = doc["corrections"][0].clone();
        doc["corrections"]
            .as_array_mut()
            .unwrap()
            .extend([row, json!({})]);
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rows.len(), corrections().len());
        assert_eq!(rejects.len(), 2);
        assert!(rejects[0].contains("duplicate"));
        assert!(rejects[1].contains("did not parse"));
        assert_eq!(load_reporting_rejects("not json").1.len(), 1);
    }

    #[test]
    fn invalid_correction_fields_are_rejected() {
        let doc: serde_json::Value = serde_json::from_str(DOCUMENT).unwrap();
        for (field, value) in [
            ("provider", json!("")),
            ("model", json!(" ")),
            ("source_ref", json!({"repo":"", "commit":"x", "file":"x"})),
            ("field", json!("wire.npm")),
            ("value", json!("")),
            ("withheld", json!(true)),
            ("established_by", json!("pi")),
            ("established_at_ms", json!(0)),
            ("review_by_ms", json!(1)),
            ("unknown", json!(true)),
        ] {
            let mut row = doc["corrections"][0].clone();
            row[field] = value;
            let (rows, rejects) = load_reporting_rejects(&json!({"corrections":[row]}).to_string());
            assert!(rows.is_empty(), "{field}");
            assert_eq!(rejects.len(), 1, "{field}");
        }
    }

    #[test]
    fn corrections_are_within_review_dates() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert!(overdue_at(corrections(), now).is_empty());
    }

    #[test]
    fn planted_provider_default_without_value_or_quote_is_rejected() {
        let doc: serde_json::Value = serde_json::from_str(DOCUMENT).unwrap();
        let provider = doc["corrections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["model"] == "*")
            .unwrap();
        for quote in [json!(null), json!(" ")] {
            let mut row = provider.clone();
            row["quote"] = quote;
            let (rows, rejects) = load_reporting_rejects(&json!({"corrections":[row]}).to_string());
            assert!(rows.is_empty());
            assert_eq!(rejects.len(), 1);
            assert!(rejects[0].contains("provider defaults require"));
        }
        let mut row = provider.clone();
        row["value"] = json!(null);
        row["withheld"] = json!(true);
        let (rows, rejects) = load_reporting_rejects(&json!({"corrections":[row]}).to_string());
        assert!(rows.is_empty());
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("provider defaults require"));
    }

    #[test]
    fn explicit_withholding_removes_only_the_named_model_endpoint() {
        let row = json!({
            "provider": "example", "model": "unrouted", "field": "wire.api",
            "withheld": true,
            "source_ref": {"repo": "fixture", "commit": "rev", "file": "routes"},
            "established_by": "fusiform", "established_at_ms": 1,
            "review_by_ms": 1 + 60 * 86_400_000_i64
        });
        let (corrections, rejects) =
            load_reporting_rejects(&json!({"corrections":[row]}).to_string());
        assert!(rejects.is_empty(), "{rejects:?}");
        let shape = json!({"value":"completions","from":"model"});
        let mut facts = std::collections::BTreeMap::from([
            (
                "wire.api".into(),
                json!({"value":"https://example.test/v1","from":"model"}),
            ),
            ("wire.shape".into(), shape.clone()),
        ]);
        assert!(apply(&corrections, "example", "other", &mut facts, None).is_none());
        assert!(facts.contains_key("wire.api"));
        let result = apply(&corrections, "example", "unrouted", &mut facts, None).unwrap();
        assert!(result.withheld);
        assert!(!facts.contains_key("wire.api"));
        assert_eq!(facts["wire.shape"], shape);
    }

    #[test]
    fn planted_overdue_correction_requires_review() {
        let row = corrections().values().next().unwrap().clone();
        let review = row.review_by_ms;
        let rows = WireCorrections::from([((row.provider.clone(), row.model.clone()), row)]);
        assert!(overdue_at(&rows, review).is_empty());
        let overdue = overdue_at(&rows, review + 1);
        assert_eq!(overdue.len(), 1);
        assert!(overdue[0].contains("re-verify"));
        assert!(overdue[0].contains("or remove"));
    }
}
