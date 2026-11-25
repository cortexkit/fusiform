//! Sourced endpoint corrections, applied only to current catalog reads.
//!
//! The store retains upstream claims verbatim; only an explicit per-model
//! correction can withhold an endpoint.

use std::collections::BTreeMap;

const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireCorrection {
    pub provider: String,
    pub model: String,
    pub field: String,
    pub value: Option<String>,
    pub replaces: Option<String>,
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
    load(doc, false)
}

/// Accept the retired completeness field in signed documents without applying it.
pub fn load_stored_reporting_rejects(doc: &str) -> (WireCorrections, Vec<String>) {
    load(doc, true)
}

fn load(doc: &str, stored: bool) -> (WireCorrections, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Doc {
        corrections: Vec<serde_json::Value>,
        #[serde(flatten)]
        metadata: BTreeMap<String, serde::de::IgnoredAny>,
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
    if !stored && doc.metadata.contains_key("complete_providers") {
        return (
            WireCorrections::new(),
            vec!["unknown field `complete_providers`".into()],
        );
    }
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
        } else if row.replaces.as_ref().is_some_and(|base| {
            row.model != "*" || base.trim().is_empty() || Some(base) == row.value.as_ref()
        }) {
            Some("replaces must name a different, nonempty observed base on a provider correction")
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

pub fn overdue_at(rows: &WireCorrections, now_ms: i64) -> Vec<String> {
    rows.values().filter(|row| row.review_by_ms < now_ms)
        .map(|row| format!("{}/{}: re-verify {}@{} {}, then re-date the review (60 days), or remove the correction", row.provider, row.model, row.source_ref.repo, row.source_ref.commit, row.source_ref.file)).collect()
}

/// Apply an explicit per-model correction, including a disclosure when withheld.
pub fn apply(
    corrections: &WireCorrections,
    provider: &str,
    model: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
) -> Option<fusiform_protocol::WireCorrectionWire> {
    let row = corrections.get(&(provider.to_string(), model.to_string()))?;
    apply_row(row, model, facts, prefixes)
}

/// Fill an absent endpoint or correct an exact, sourced upstream base.
pub fn apply_provider_api(
    corrections: &WireCorrections,
    provider: &str,
    model: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
) -> Option<fusiform_protocol::WireCorrectionWire> {
    if corrections.contains_key(&(provider.to_string(), model.to_string())) {
        return None;
    }
    let row = corrections.get(&(provider.to_string(), "*".to_string()))?;
    let upstream = facts.get("wire.api");
    if match row.replaces.as_deref() {
        Some(base) => upstream.and_then(|claim| claim["value"].as_str()) != Some(base),
        None => upstream.is_some(),
    } {
        return None;
    }
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
        // Historical per-model notes explain the curation to maintainers;
        // provider corrections can document why a base is replaced for callers.
        notes: if row.model == "*" {
            row.notes.clone()
        } else {
            None
        },
    })
}
