//! Sourced login allowlists for upstream rows, applied only on current reads.
//! These restrictions are not upstream facts and never enter stored history.

use std::collections::BTreeMap;

const DOCUMENT: &str = include_str!("../data/login-methods.json");
const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginMethods {
    pub provider_id: String,
    pub model_id: String,
    pub auth_methods: Vec<String>,
    pub source_ref: String,
    pub quote: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

pub type Rows = BTreeMap<(String, String), LoginMethods>;

/// Count every malformed, duplicate, non-upstream or already-curated claim.
pub fn load_reporting_rejects(doc: &str) -> (Rows, Vec<String>) {
    let seed = serde_json::from_str(include_str!("../data/models-dev-seed.json"))
        .expect("the embedded seed parses");
    load_with(doc, &seed, crate::curated_models::models())
}

fn load_with(
    doc: &str,
    seed: &serde_json::Value,
    curated: &crate::curated_models::CuratedModels,
) -> (Rows, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Doc {
        models: Vec<serde_json::Value>,
    }
    let doc: Doc = match serde_json::from_str(doc) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                Rows::new(),
                vec![format!("login methods unreadable: {error}")],
            )
        }
    };
    let mut rows = Rows::new();
    let mut rejects = Vec::new();
    for raw in doc.models {
        let row: LoginMethods = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!("login methods row did not parse: {error}"));
                continue;
            }
        };
        let key = (row.provider_id.clone(), row.model_id.clone());
        let invalid = if [&row.provider_id, &row.model_id, &row.source_ref, &row.quote]
            .iter()
            .any(|field| field.trim().is_empty())
        {
            Some("empty identity, source or quote")
        } else if row.auth_methods.is_empty()
            || row.auth_methods.iter().enumerate().any(|(i, method)| {
                !crate::billing_planes::AUTH_METHODS.contains(&method.as_str())
                    || row.auth_methods[..i].contains(method)
            })
        {
            Some("auth_methods must be a nonempty list of distinct billing-plane methods")
        } else if row.established_at_ms <= 0
            || row.review_by_ms.checked_sub(row.established_at_ms) != Some(REVIEW_INTERVAL_MS)
        {
            Some("review_by_ms must be 60 days after established_at_ms")
        } else if curated.contains_key(&key) {
            Some("row is already curated; keep its login methods in curated-models.json")
        } else if !seed[&row.provider_id]["models"][&row.model_id].is_object() {
            Some("row is missing from the embedded seed; correct or remove the login allowlist")
        } else if rows.contains_key(&key) {
            Some("duplicate login methods identity")
        } else {
            None
        };
        if let Some(reason) = invalid {
            rejects.push(format!("login methods {}/{}: {reason}", key.0, key.1));
        } else {
            rows.insert(key, row);
        }
    }
    (rows, rejects)
}

pub fn rows() -> &'static Rows {
    static ROWS: std::sync::OnceLock<Rows> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

pub fn overdue_at(rows: &Rows, now_ms: i64) -> Vec<String> {
    rows.values().filter(|row| row.review_by_ms < now_ms)
        .map(|row| format!("{}/{}: re-verify {} and re-date established_at_ms and review_by_ms (60 days later), or remove the login allowlist", row.provider_id, row.model_id, row.source_ref))
        .collect()
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
        assert_eq!(rows.len(), 11);
        for model in [
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
        let deadline = rows.values().map(|row| row.review_by_ms).max().unwrap();
        assert!(overdue_at(&rows, deadline).is_empty());
        let overdue = overdue_at(&rows, deadline + 1);
        assert_eq!(overdue.len(), 11);
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
        assert_eq!(rows.len(), 10);
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
            assert_eq!(rows.len(), 10, "{field}: {rejects:?}");
            assert_eq!(rejects.len(), 1, "{field}: {rejects:?}");
        }
        let mut doc = document();
        let duplicate = doc["models"][0].clone();
        doc["models"].as_array_mut().unwrap().push(duplicate);
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rows.len(), 11);
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("duplicate login methods identity"));
    }
}
