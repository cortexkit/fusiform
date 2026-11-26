//! Sourced login allowlists for upstream and alias rows, applied only on current reads.
//! These restrictions are not observations and do not enter model fact history.

use std::collections::BTreeMap;

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

pub fn load_with(
    doc: &str,
    seed: &serde_json::Value,
    curated: &crate::curated_models::CuratedModels,
) -> (Rows, Vec<String>) {
    load_where(doc, curated, |provider, model| {
        seed[provider]["models"][model].is_object()
    })
}

/// Validate rows using the caller's catalog identity check.
pub fn load_where(
    doc: &str,
    curated: &crate::curated_models::CuratedModels,
    known_model: impl Fn(&str, &str) -> bool,
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
        } else if !known_model(&row.provider_id, &row.model_id) {
            Some("row is neither an upstream model nor a current alias; correct or remove the login allowlist")
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

pub fn overdue_at(rows: &Rows, now_ms: i64) -> Vec<String> {
    rows.values().filter(|row| row.review_by_ms < now_ms)
        .map(|row| format!("{}/{}: re-verify {} and re-date established_at_ms and review_by_ms (60 days later), or remove the login allowlist", row.provider_id, row.model_id, row.source_ref))
        .collect()
}
