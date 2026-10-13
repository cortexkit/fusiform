//! Current-only model facts backed by explicit probes and lab records.
//!
//! Provider rows missing from models.dev's API catalog cannot be inferred from
//! lab ids: the provider may require a different model id and login method.
//! These rows carry those restrictions and stand down as soon as upstream lands.

use std::collections::BTreeMap;

const DOCUMENT: &str = include_str!("../data/curated-models.json");

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CuratedModel {
    pub provider_id: String,
    pub model_id: String,
    pub auth_methods: Vec<String>,
    pub facts: BTreeMap<String, serde_json::Value>,
    pub source_ref: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

pub type CuratedModels = BTreeMap<(String, String), CuratedModel>;

/// Reject unsupported facts rather than silently exposing a new wire vocabulary.
/// A login-restricted subscription row must never claim an API list price.
fn invalid_facts(row: &CuratedModel) -> Option<String> {
    for key in row.facts.keys() {
        if !row.auth_methods.is_empty() && key.starts_with("rate.") {
            return Some(format!(
                "subscription-only row must not carry a list price: {key}"
            ));
        }
        if !fusiform_protocol::SERVED_FACTS
            .iter()
            .any(|fact| fact.key == key)
        {
            return Some(format!("fact key is not in SERVED_FACTS: {key}"));
        }
    }
    None
}

/// Malformed rows and duplicate identities are counted, not silently skipped.
pub fn load_reporting_rejects(doc: &str) -> (CuratedModels, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Doc {
        models: Vec<serde_json::Value>,
    }
    let doc: Doc = match serde_json::from_str(doc) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                BTreeMap::new(),
                vec![format!("curated table unreadable: {error}")],
            )
        }
    };
    let mut rows = CuratedModels::new();
    let mut rejects = Vec::new();
    for raw in doc.models {
        let row: CuratedModel = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!("curated row did not parse: {error}"));
                continue;
            }
        };
        let key = (row.provider_id.clone(), row.model_id.clone());
        let invalid = if [&row.provider_id, &row.model_id, &row.source_ref]
            .iter()
            .any(|field| field.trim().is_empty())
            || row.facts.is_empty()
        {
            Some("empty identity, source or facts".to_string())
        } else if row.auth_methods.is_empty()
            || row
                .auth_methods
                .iter()
                .any(|method| !crate::billing_planes::AUTH_METHODS.contains(&method.as_str()))
            || row
                .auth_methods
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != row.auth_methods.len()
        {
            Some(
                "auth_methods must be a nonempty allowlist of distinct known login methods"
                    .to_string(),
            )
        } else if row.established_at_ms <= 0
            || row.review_by_ms.checked_sub(row.established_at_ms) != Some(60 * 86_400_000)
        {
            Some("review_by_ms must be 60 days after established_at_ms".to_string())
        } else if rows.contains_key(&key) {
            Some("duplicate curated identity".to_string())
        } else {
            invalid_facts(&row)
        };
        if let Some(reason) = invalid {
            rejects.push(format!("curated {}/{}: {reason}", key.0, key.1));
        } else {
            rows.insert(key, row);
        }
    }
    (rows, rejects)
}

/// Embedded rows are parsed once per process, never written to the store.
pub fn models() -> &'static CuratedModels {
    static PARSED: std::sync::OnceLock<CuratedModels> = std::sync::OnceLock::new();
    PARSED.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

pub fn upstream_collisions(rows: &CuratedModels, seed: &serde_json::Value) -> Vec<String> {
    rows.keys()
        .filter(|(provider, model)| seed[provider]["models"].get(model).is_some())
        .map(|(provider, model)| {
            format!("{provider}/{model}: upstream now publishes this row; remove the curated row")
        })
        .collect()
}

pub fn overdue_at(rows: &CuratedModels, now_ms: i64) -> Vec<String> {
    rows.values().filter(|row| row.review_by_ms < now_ms)
        .map(|row| format!("{}/{}: re-verify {} and re-date established_at_ms and review_by_ms (60 days later), or remove the curated row", row.provider_id, row.model_id, row.source_ref))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planted(edit: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut doc: serde_json::Value = serde_json::from_str(DOCUMENT).unwrap();
        edit(&mut doc["models"][0]);
        serde_json::to_string(&doc).unwrap()
    }

    #[test]
    fn shipped_curated_rows_load_without_rejects() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn upstream_now_publishes_this_row_remove_the_curated_row() {
        let seed = serde_json::from_str(include_str!("../data/models-dev-seed.json")).unwrap();
        let collisions = upstream_collisions(models(), &seed);
        assert!(collisions.is_empty(), "{}", collisions.join("\n"));
    }

    #[test]
    fn planted_upstream_row_triggers_the_removal_fence() {
        let seed = serde_json::json!({"meta":{"models":{"muse-wonder-full-1.0":{}}}});
        let collisions = upstream_collisions(models(), &seed);
        assert_eq!(collisions.len(), 1);
        assert!(collisions[0].contains("upstream now publishes this row; remove the curated row"));
    }

    #[test]
    fn curated_rows_are_within_their_review_dates() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let overdue = overdue_at(models(), now);
        assert!(overdue.is_empty(), "{}", overdue.join("\n"));
    }

    #[test]
    fn planted_overdue_row_requires_reverification_or_removal() {
        let row = models().values().next().unwrap();
        assert!(overdue_at(models(), row.review_by_ms).is_empty());
        let overdue = overdue_at(models(), row.review_by_ms + 1);
        assert_eq!(overdue.len(), 1);
        assert!(overdue[0].contains("re-verify"));
        assert!(overdue[0].contains("or remove"));
    }

    #[test]
    fn planted_unknown_fact_is_rejected() {
        let doc = planted(|row| row["facts"]["model.name"] = "Muse Wonder Full".into());
        let (rows, rejects) = load_reporting_rejects(&doc);
        assert!(rows.is_empty());
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("not in SERVED_FACTS"));
    }

    #[test]
    fn planted_subscription_price_is_rejected() {
        let doc = planted(|row| {
            row["facts"]["rate.input"] = serde_json::json!({"state":"priced","units":1})
        });
        let (rows, rejects) = load_reporting_rejects(&doc);
        assert!(rows.is_empty());
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("must not carry a list price"));
    }

    #[test]
    fn duplicate_and_malformed_rows_are_counted_as_rejects() {
        let mut doc: serde_json::Value = serde_json::from_str(DOCUMENT).unwrap();
        let duplicate = doc["models"][0].clone();
        doc["models"]
            .as_array_mut()
            .unwrap()
            .extend([duplicate, serde_json::json!({})]);
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rows.len(), 1);
        assert_eq!(rejects.len(), 2);
        assert!(rejects[0].contains("duplicate"));
        assert!(rejects[1].contains("did not parse"));
    }
}
