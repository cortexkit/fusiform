//! The embedded wire corrections document and its local adapter.

pub use fusiform_curate::wire_corrections::*;
const DOCUMENT: &str = include_str!("../data/wire-corrections.json");
pub fn corrections() -> &'static WireCorrections {
    static ROWS: std::sync::OnceLock<WireCorrections> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

pub fn complete_providers() -> &'static CompleteProviders {
    static ROWS: std::sync::OnceLock<CompleteProviders> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_complete_providers_reporting_rejects(DOCUMENT, corrections()).0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shipped_corrections_load_without_rejects() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.len(), 35);
        // OpenCode Go's unrouted models are withheld by completeness rather
        // than listed one by one, so no per-model withholding ships.
        assert_eq!(rows.values().filter(|row| row.withheld).count(), 0);
        let (complete, rejects) = load_complete_providers_reporting_rejects(DOCUMENT, &rows);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(complete.keys().collect::<Vec<_>>(), ["opencode-go"]);
        assert_eq!(
            complete["opencode-go"].notes,
            "not routed by pi at 74f46864"
        );
    }

    fn complete_doc(entry: serde_json::Value) -> String {
        json!({"corrections":[], "complete_providers":[entry]}).to_string()
    }

    #[test]
    fn invalid_complete_providers_are_rejected() {
        let doc: serde_json::Value = serde_json::from_str(DOCUMENT).unwrap();
        let shipped = doc["complete_providers"][0].clone();
        for (field, value, reason) in [
            ("provider", json!(" "), "empty identity"),
            ("notes", json!(""), "empty identity"),
            (
                "source_ref",
                json!({"repo":"x", "commit":"", "file":"x"}),
                "empty identity",
            ),
            ("field", json!("wire.npm"), "only wire.api"),
            ("notes", json!("not routed by pi"), "name the source commit"),
            ("established_by", json!("pi"), "established_by"),
            ("established_at_ms", json!(0), "review_by_ms"),
            ("review_by_ms", json!(1), "review_by_ms"),
            ("unknown", json!(true), "did not parse"),
        ] {
            let mut entry = shipped.clone();
            entry[field] = value;
            let (rows, rejects) =
                load_complete_providers_reporting_rejects(&complete_doc(entry), corrections());
            assert!(rows.is_empty(), "{field}");
            assert_eq!(rejects.len(), 1, "{field}");
            assert!(rejects[0].contains(reason), "{field}: {rejects:?}");
        }
        let duplicated = json!({"corrections":[], "complete_providers":[shipped, shipped]});
        let (rows, rejects) =
            load_complete_providers_reporting_rejects(&duplicated.to_string(), corrections());
        assert_eq!(rows.len(), 1);
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("duplicate"));
    }

    #[test]
    fn complete_providers_must_agree_with_their_per_model_rows() {
        let doc: serde_json::Value = serde_json::from_str(DOCUMENT).unwrap();
        let shipped = doc["complete_providers"][0].clone();
        let reject = |rows: &WireCorrections| {
            let (complete, rejects) =
                load_complete_providers_reporting_rejects(&complete_doc(shipped.clone()), rows);
            assert!(complete.is_empty());
            assert_eq!(rejects.len(), 1);
            rejects[0].clone()
        };
        // Without routed rows, completeness would withhold every model.
        let unrouted: WireCorrections = corrections()
            .iter()
            .filter(|((provider, _), _)| provider != "opencode-go")
            .map(|(key, row)| (key.clone(), row.clone()))
            .collect();
        assert!(reject(&unrouted).contains("at least one routed model"));
        // A provider default would fill what completeness withholds.
        let mut defaulted = corrections().clone();
        let mut default = corrections()[&("openai".into(), "*".into())].clone();
        default.provider = "opencode-go".into();
        default.source_ref = shipped_source(&shipped);
        defaulted.insert(("opencode-go".into(), "*".into()), default);
        assert!(reject(&defaulted).contains("provider default"));
        // A routed row read from another source is not part of this list.
        let mut elsewhere = corrections().clone();
        elsewhere
            .get_mut(&("opencode-go".into(), "kimi-k3".into()))
            .unwrap()
            .source_ref
            .commit = "0000000".into();
        assert!(reject(&elsewhere).contains("completeness source"));
    }

    fn shipped_source(entry: &serde_json::Value) -> fusiform_protocol::WireSourceRef {
        serde_json::from_value(entry["source_ref"].clone()).unwrap()
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
        assert!(complete_overdue_at(complete_providers(), now).is_empty());
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

    fn opencode_anthropic_models() -> Vec<String> {
        let catalog = crate::seed::catalog().unwrap();
        let provider = catalog
            .providers
            .iter()
            .find(|provider| provider.provider_id == "opencode-go")
            .unwrap();
        let mut models: Vec<_> = provider
            .models
            .iter()
            .filter(|model| {
                model
                    .wire
                    .npm
                    .as_ref()
                    .is_some_and(|npm| npm.value == "@ai-sdk/anthropic")
            })
            .map(|model| model.key.model_id.clone())
            .collect();
        models.sort();
        models
    }

    fn unversioned_anthropic_bases(rows: &WireCorrections) -> Vec<String> {
        opencode_anthropic_models()
            .into_iter()
            .filter(|model| {
                rows.get(&("opencode-go".into(), model.clone()))
                    .and_then(|row| row.value.as_ref())
                    .is_some_and(|api| !api.ends_with("/v1"))
            })
            .collect()
    }

    #[test]
    fn opencode_anthropic_bases_include_the_version_path() {
        assert_eq!(
            opencode_anthropic_models(),
            [
                "claude-haiku-5-5",
                "minimax-m2.7",
                "minimax-m3",
                "qwen3.7-plus",
                "qwen3.8-flash",
                "qwen3.8-max",
                "qwen4-max"
            ]
        );
        assert!(unversioned_anthropic_bases(corrections()).is_empty());
    }

    #[test]
    fn planted_unversioned_anthropic_base_is_reported() {
        let mut rows = corrections().clone();
        rows.get_mut(&("opencode-go".into(), "claude-haiku-5-5".into()))
            .unwrap()
            .value = Some("https://opencode.ai/inference/go".into());
        assert_eq!(unversioned_anthropic_bases(&rows), ["claude-haiku-5-5"]);
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

        let complete = complete_providers()["opencode-go"].clone();
        let review = complete.review_by_ms;
        let rows = CompleteProviders::from([(complete.provider.clone(), complete)]);
        assert!(complete_overdue_at(&rows, review).is_empty());
        let overdue = complete_overdue_at(&rows, review + 1);
        assert_eq!(overdue.len(), 1);
        assert!(overdue[0].contains("re-verify"));
        assert!(overdue[0].contains("or remove"));
    }
}
