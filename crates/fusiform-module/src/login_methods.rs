//! The embedded login methods document and its local adapter.

pub use fusiform_curate::login_methods::*;

const DOCUMENT: &str = include_str!("../data/login-methods.json");
/// Count every malformed, duplicate, non-upstream or already-curated claim.
pub fn load_reporting_rejects(doc: &str) -> (Rows, Vec<String>) {
    let seed = serde_json::from_str(include_str!("../data/models-dev-seed.json"))
        .expect("the embedded seed parses");
    load_with(doc, &seed, crate::curated_models::models())
}

pub fn rows() -> &'static Rows {
    static ROWS: std::sync::OnceLock<Rows> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> serde_json::Value {
        serde_json::from_str(DOCUMENT).unwrap()
    }

    #[test]
    fn shipped_login_methods_load_without_rejects_and_cover_the_documented_models() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.len(), 13);
        for model in [
            "gpt-6.2-astra",
            "gpt-6.2-sol",
            "gpt-6.1-sol",
            "gpt-6-luna",
            "gpt-6-astra",
            "gpt-6.1-terra",
            "gpt-6-sol",
        ] {
            assert_eq!(
                rows[&("openai".into(), model.into())].auth_methods,
                ["apikey", "chatgpt"]
            );
        }
        for model in [
            "gpt-5.5",
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.2",
            "gpt-5.3-codex",
            "gpt-5.3-codex-spark",
        ] {
            assert_eq!(
                rows[&("openai".into(), model.into())].auth_methods,
                ["apikey"]
            );
        }
    }

    #[test]
    fn login_methods_are_within_their_review_dates() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let overdue = overdue_at(rows(), now);
        assert!(overdue.is_empty(), "{}", overdue.join("\n"));
    }

    #[test]
    fn planted_overdue_login_methods_require_reverification_or_removal() {
        let rows = load_reporting_rejects(DOCUMENT).0;
        let first_deadline = rows.values().map(|row| row.review_by_ms).min().unwrap();
        assert!(overdue_at(&rows, first_deadline).is_empty());
        assert_eq!(overdue_at(&rows, first_deadline + 1).len(), 11);
        let last_deadline = rows.values().map(|row| row.review_by_ms).max().unwrap();
        let overdue = overdue_at(&rows, last_deadline + 1);
        assert_eq!(overdue.len(), 13);
        assert!(overdue
            .iter()
            .all(|message| message.contains("re-verify https://")
                && message.contains("remove the login allowlist")));
    }

    #[test]
    fn planted_missing_seed_model_is_rejected() {
        let mut doc = document();
        doc["models"][0]["model_id"] = json!("not-in-the-seed");
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rows.len(), 12);
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("openai/not-in-the-seed"));
        assert!(rejects[0].contains("missing from the embedded seed"));
    }

    #[test]
    fn planted_curated_duplicate_is_rejected() {
        let mut doc = document();
        let curated = crate::curated_models::models().values().next().unwrap();
        doc["models"][0]["provider_id"] = json!(curated.provider_id);
        doc["models"][0]["model_id"] = json!(curated.model_id);
        let (_, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("already curated"));
    }

    #[test]
    fn malformed_duplicate_and_unknown_method_rows_are_counted() {
        assert_eq!(load_reporting_rejects("not json").1.len(), 1);
        for (field, value) in [
            ("auth_methods", json!([])),
            ("auth_methods", json!(["unknown"])),
            ("auth_methods", json!(["apikey", "apikey"])),
            ("quote", json!("")),
            ("source_ref", json!("")),
            ("review_by_ms", json!(1)),
            ("established_at_ms", json!(0)),
            ("unexpected", json!(true)),
        ] {
            let mut doc = document();
            doc["models"][0][field] = value;
            let (rows, rejects) = load_reporting_rejects(&doc.to_string());
            assert_eq!(rows.len(), 12, "{field}: {rejects:?}");
            assert_eq!(rejects.len(), 1, "{field}: {rejects:?}");
        }
        let mut doc = document();
        let duplicate = doc["models"][0].clone();
        doc["models"].as_array_mut().unwrap().push(duplicate);
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rows.len(), 13);
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("duplicate login methods identity"));
    }
}
