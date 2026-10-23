//! Sourced endpoint corrections, applied only to current catalog reads.
//!
//! The store retains upstream claims verbatim. A missing route in the pinned
//! client catalog withholds the endpoint rather than guessing a destination.

use std::collections::BTreeMap;

const DOCUMENT: &str = include_str!("../data/wire-corrections.json");
const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireCorrection {
    pub provider: String,
    pub model: String,
    pub field: String,
    pub value: Option<String>,
    pub withheld: bool,
    pub source_ref: fusiform_protocol::WireSourceRef,
    pub quote: Option<String>,
    pub notes: Option<String>,
    pub established_by: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

pub type WireCorrections = BTreeMap<(String, String), WireCorrection>;

/// Parse each claim independently, reporting malformed and duplicate rows.
pub fn load_reporting_rejects(doc: &str) -> (WireCorrections, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Doc {
        corrections: Vec<serde_json::Value>,
    }
    let doc: Doc = match serde_json::from_str(doc) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                BTreeMap::new(),
                vec![format!("wire corrections unreadable: {error}")],
            )
        }
    };
    let mut rows = WireCorrections::new();
    let mut rejects = Vec::new();
    for raw in doc.corrections {
        let row: WireCorrection = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!("wire correction did not parse: {error}"));
                continue;
            }
        };
        let key = (row.provider.clone(), row.model.clone());
        let invalid = if [
            &row.provider,
            &row.model,
            &row.source_ref.repo,
            &row.source_ref.commit,
            &row.source_ref.file,
        ]
        .iter()
        .any(|field| field.trim().is_empty())
        {
            Some("empty identity or source")
        } else if row.field != "wire.api" {
            Some("only wire.api corrections are supported")
        } else if row.withheld != row.value.is_none()
            || row.value.as_ref().is_some_and(|v| v.trim().is_empty())
        {
            Some("withheld requires no value; a correction requires a nonempty value")
        } else if row.model == "*"
            && (row.withheld
                || row
                    .quote
                    .as_ref()
                    .is_none_or(|quote| quote.trim().is_empty()))
        {
            Some("provider defaults require a value and a source quote")
        } else if row.established_by != "fusiform" {
            Some("established_by must be fusiform")
        } else if row.established_at_ms <= 0
            || row.review_by_ms.checked_sub(row.established_at_ms) != Some(REVIEW_INTERVAL_MS)
        {
            Some("review_by_ms must be 60 days after established_at_ms")
        } else if rows.contains_key(&key) {
            Some("duplicate wire correction identity")
        } else {
            None
        };
        if let Some(reason) = invalid {
            rejects.push(format!("wire correction {}/{}: {reason}", key.0, key.1));
        } else {
            rows.insert(key, row);
        }
    }
    (rows, rejects)
}

pub fn corrections() -> &'static WireCorrections {
    static ROWS: std::sync::OnceLock<WireCorrections> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

pub fn overdue_at(rows: &WireCorrections, now_ms: i64) -> Vec<String> {
    rows.values().filter(|row| row.review_by_ms < now_ms)
        .map(|row| format!("{}/{}: re-verify {}@{} {}, then re-date the review (60 days), or remove the correction", row.provider, row.model, row.source_ref.repo, row.source_ref.commit, row.source_ref.file)).collect()
}

/// Apply only the requested field, including a disclosure when it is withheld.
pub fn apply(
    provider: &str,
    model: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
) -> Option<fusiform_protocol::WireCorrectionWire> {
    let row = corrections().get(&(provider.to_string(), model.to_string()))?;
    apply_row(row, model, facts, prefixes)
}

/// Fill a missing endpoint only; upstream claims and per-model corrections win.
pub fn apply_provider_api(
    provider: &str,
    model: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
) -> Option<fusiform_protocol::WireCorrectionWire> {
    if facts.contains_key("wire.api")
        || corrections().contains_key(&(provider.to_string(), model.to_string()))
    {
        return None;
    }
    let row = corrections().get(&(provider.to_string(), "*".to_string()))?;
    apply_row(row, model, facts, prefixes)
}

fn apply_row(
    row: &WireCorrection,
    model: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
) -> Option<fusiform_protocol::WireCorrectionWire> {
    if prefixes.is_some_and(|p| !p.iter().any(|p| row.field.starts_with(p))) {
        return None;
    }
    if row.withheld {
        facts.remove("wire.api");
    } else {
        facts.insert(
            row.field.clone(),
            serde_json::json!({"value": row.value, "from": "curated"}),
        );
    }
    Some(fusiform_protocol::WireCorrectionWire {
        provider: row.provider.clone(),
        model: model.to_string(),
        field: row.field.clone(),
        withheld: row.withheld,
        source_ref: row.source_ref.clone(),
        established_by: row.established_by.clone(),
        established_at_ms: row.established_at_ms,
        review_by_ms: row.review_by_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shipped_corrections_load_without_rejects() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.len(), 38);
        assert_eq!(rows.values().filter(|row| row.withheld).count(), 5);
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
                "qwen3.8-max"
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
    }
}
