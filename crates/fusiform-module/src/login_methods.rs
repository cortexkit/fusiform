//! The embedded login methods document and its local adapter.

pub use fusiform_curate::login_methods::*;

const DOCUMENT: &str = include_str!("../data/login-methods.json");
/// Count malformed, duplicate or already-curated claims and unknown identities.
pub fn load_reporting_rejects(doc: &str) -> (Rows, Vec<String>) {
    let seed: serde_json::Value =
        serde_json::from_str(include_str!("../data/models-dev-seed.json"))
            .expect("the embedded seed parses");
    let aliases = crate::aliases::aliases();
    load_where(doc, crate::curated_models::models(), |provider, model| {
        seed[provider]["models"][model].is_object()
            || aliases.contains_key(&(provider.to_owned(), model.to_owned()))
    })
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
        let antigravity_aliases =
            crate::aliases::aliases()
                .iter()
                .filter(|((provider, model), _)| {
                    provider == "google" && model.starts_with("antigravity-")
                });
        assert_eq!(rows.len(), 12 + antigravity_aliases.clone().count());
        for (id, alias) in antigravity_aliases {
            assert_eq!(rows[id].auth_methods, ["antigravity"]);
            assert_eq!(rows[id].source_ref, alias.source_ref);
        }
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
        assert_eq!(overdue_at(&rows, first_deadline + 1).len(), 10);
        let last_deadline = rows.values().map(|row| row.review_by_ms).max().unwrap();
        let overdue = overdue_at(&rows, last_deadline + 1);
        assert_eq!(overdue.len(), rows.len());
        assert!(overdue
            .iter()
            .all(|message| message.contains("re-verify https://")
                && message.contains("remove the login allowlist")));
    }

    #[test]
    fn planted_unknown_model_and_retired_alias_are_rejected() {
        let mut doc = document();
        for (provider, model) in [
            ("openai", "not-in-the-seed"),
            ("google", "antigravity-gemini-3.7-flash"),
        ] {
            doc["models"][0]["provider_id"] = json!(provider);
            doc["models"][0]["model_id"] = json!(model);
            let (rows, rejects) = load_reporting_rejects(&doc.to_string());
            assert_eq!(rows.len() + 1, doc["models"].as_array().unwrap().len());
            assert_eq!(rejects.len(), 1);
            assert!(rejects[0].contains(&format!("{provider}/{model}")));
            assert!(rejects[0].contains("neither an upstream model nor a current alias"));
        }
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
        let count = document()["models"].as_array().unwrap().len();
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
            assert_eq!(rows.len(), count - 1, "{field}: {rejects:?}");
            assert_eq!(rejects.len(), 1, "{field}: {rejects:?}");
        }
        let mut doc = document();
        let duplicate = doc["models"][0].clone();
        doc["models"].as_array_mut().unwrap().push(duplicate);
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rows.len(), count);
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("duplicate login methods identity"));
    }
}
