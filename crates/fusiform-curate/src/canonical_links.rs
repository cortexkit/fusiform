//! Sourced cross-provider identities for upstream rows without a lab identity.
//!
//! These claims are applied only while serving the current catalog. They never
//! replace an upstream identity or become observations in the store.

use std::collections::BTreeMap;

const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalLink {
    pub provider_id: String,
    pub model_id: String,
    pub canonical_id: String,
    pub source_ref: String,
    pub quote: String,
    pub lab_source_ref: String,
    pub lab_quote: String,
    pub established_by: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

pub type CanonicalLinks = BTreeMap<(String, String), CanonicalLink>;

fn valid_canonical_id(id: &str) -> bool {
    let Some((lab, model)) = id.split_once('/') else {
        return false;
    };
    !lab.is_empty()
        && !model.is_empty()
        && !model.contains('/')
        && !id.chars().any(char::is_whitespace)
}

/// Reject malformed claims and duplicate keys rather than silently choosing one.
pub fn load_reporting_rejects(doc: &str) -> (CanonicalLinks, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Doc {
        links: Vec<serde_json::Value>,
    }
    let doc: Doc = match serde_json::from_str(doc) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                BTreeMap::new(),
                vec![format!("canonical links unreadable: {error}")],
            )
        }
    };
    let mut rows = CanonicalLinks::new();
    let mut rejects = Vec::new();
    for raw in doc.links {
        let row: CanonicalLink = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!("canonical link did not parse: {error}"));
                continue;
            }
        };
        let key = (row.provider_id.clone(), row.model_id.clone());
        let invalid = if [
            &row.provider_id,
            &row.model_id,
            &row.source_ref,
            &row.quote,
            &row.lab_source_ref,
            &row.lab_quote,
        ]
        .iter()
        .any(|field| field.trim().is_empty())
        {
            Some("empty identity, source or quote")
        } else if !valid_canonical_id(&row.canonical_id) {
            Some("canonical_id must be <lab>/<id> without whitespace")
        } else if row.established_by != "fusiform" {
            Some("established_by must be fusiform")
        } else if row.established_at_ms <= 0
            || row.review_by_ms.checked_sub(row.established_at_ms) != Some(REVIEW_INTERVAL_MS)
        {
            Some("review_by_ms must be 60 days after established_at_ms")
        } else if rows.contains_key(&key) {
            Some("duplicate canonical link identity")
        } else {
            None
        };
        if let Some(reason) = invalid {
            rejects.push(format!("canonical link {}/{}: {reason}", key.0, key.1));
        } else {
            rows.insert(key, row);
        }
    }
    (rows, rejects)
}

pub fn upstream_collisions(rows: &CanonicalLinks, seed: &serde_json::Value) -> Vec<String> {
    rows.keys()
        .filter(|(provider, model)| {
            seed[provider]["models"][model].get("canonical_model_id").is_some_and(|id| !id.is_null())
        })
        .map(|(provider, model)| format!("{provider}/{model}: upstream now publishes canonical_model_id; remove the canonical link"))
        .collect()
}

pub fn missing_seed_rows(rows: &CanonicalLinks, seed: &serde_json::Value) -> Vec<String> {
    rows.keys()
        .filter(|(provider, model)| seed[provider]["models"].get(model).is_none())
        .map(|(provider, model)| format!("{provider}/{model}: linked row is missing from the embedded seed; correct or remove the canonical link"))
        .collect()
}

pub fn overdue_at(rows: &CanonicalLinks, now_ms: i64) -> Vec<String> {
    rows.values().filter(|row| row.review_by_ms < now_ms)
        .map(|row| format!("{}/{}: re-verify {} and re-date established_at_ms and review_by_ms (60 days later), or remove the canonical link", row.provider_id, row.model_id, row.source_ref))
        .collect()
}
