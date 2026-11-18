//! Sourced endpoint corrections, applied only to current catalog reads.
//!
//! The store retains upstream claims verbatim. A missing route in the pinned
//! client catalog withholds the endpoint rather than guessing a destination.
//! A provider named in the document's `complete_providers` has its routed
//! models listed exhaustively, so any of its models without a per-model row is
//! withheld too, including models upstream adds after the list was read.

use std::collections::BTreeMap;

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

/// A provider whose per-model rows list every model its pinned source routes.
///
/// Any other model of that provider is not routed by the source, so its
/// endpoint is withheld and disclosed with this entry's source and `notes`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteProvider {
    pub provider: String,
    pub field: String,
    pub source_ref: fusiform_protocol::WireSourceRef,
    pub notes: String,
    pub established_by: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

pub type CompleteProviders = BTreeMap<String, CompleteProvider>;

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

/// Parse each completeness entry independently, checked against the parsed
/// per-model rows it vouches for. Malformed, duplicate and inconsistent
/// entries are reported and left out, so the provider falls back to its
/// per-model rows alone.
pub fn load_complete_providers_reporting_rejects(
    doc: &str,
    corrections: &WireCorrections,
) -> (CompleteProviders, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Doc {
        #[serde(default)]
        complete_providers: Vec<serde_json::Value>,
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
    let mut rows = CompleteProviders::new();
    let mut rejects = Vec::new();
    for raw in doc.complete_providers {
        let row: CompleteProvider = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!("complete provider did not parse: {error}"));
                continue;
            }
        };
        let provider_rows: Vec<&WireCorrection> = corrections
            .values()
            .filter(|correction| correction.provider == row.provider)
            .collect();
        let invalid = if [
            &row.provider,
            &row.notes,
            &row.source_ref.repo,
            &row.source_ref.commit,
            &row.source_ref.file,
        ]
        .iter()
        .any(|field| field.trim().is_empty())
        {
            Some("empty identity, source or notes")
        } else if row.field != "wire.api" {
            Some("only wire.api completeness is supported")
        } else if !row.notes.contains(&row.source_ref.commit) {
            Some("notes must name the source commit the route list was read at")
        } else if row.established_by != "fusiform" {
            Some("established_by must be fusiform")
        } else if row.established_at_ms <= 0
            || row.review_by_ms.checked_sub(row.established_at_ms) != Some(REVIEW_INTERVAL_MS)
        {
            Some("review_by_ms must be 60 days after established_at_ms")
        } else if provider_rows
            .iter()
            .any(|correction| correction.model == "*")
        {
            // A provider default fills endpoints for unlisted models, which a
            // complete list withholds: the two cannot both hold.
            Some("a complete provider cannot also have a provider default")
        } else if !provider_rows.iter().any(|correction| !correction.withheld) {
            Some("a complete provider needs at least one routed model")
        } else if provider_rows
            .iter()
            .any(|correction| correction.source_ref != row.source_ref)
        {
            // Completeness is a claim about one source's route list; a row
            // read from elsewhere is not part of that list.
            Some("every per-model row must cite the completeness source")
        } else if rows.contains_key(&row.provider) {
            Some("duplicate complete provider")
        } else {
            None
        };
        if let Some(reason) = invalid {
            rejects.push(format!("complete provider {}: {reason}", row.provider));
        } else {
            rows.insert(row.provider.clone(), row);
        }
    }
    (rows, rejects)
}

pub fn overdue_at(rows: &WireCorrections, now_ms: i64) -> Vec<String> {
    rows.values().filter(|row| row.review_by_ms < now_ms)
        .map(|row| format!("{}/{}: re-verify {}@{} {}, then re-date the review (60 days), or remove the correction", row.provider, row.model, row.source_ref.repo, row.source_ref.commit, row.source_ref.file)).collect()
}

pub fn complete_overdue_at(rows: &CompleteProviders, now_ms: i64) -> Vec<String> {
    rows.values().filter(|row| row.review_by_ms < now_ms)
        .map(|row| format!("{}: re-verify that {}@{} {} routes no model beyond the listed corrections, then re-date the review (60 days), or remove the completeness entry", row.provider, row.source_ref.repo, row.source_ref.commit, row.source_ref.file)).collect()
}

/// Apply only the requested field, including a disclosure when it is withheld.
///
/// A per-model row wins. Without one, a model of a complete provider is not
/// routed by that provider's source, so its endpoint is withheld.
pub fn apply(
    corrections: &WireCorrections,
    complete_providers: &CompleteProviders,
    provider: &str,
    model: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
) -> Option<fusiform_protocol::WireCorrectionWire> {
    if let Some(row) = corrections.get(&(provider.to_string(), model.to_string())) {
        return apply_row(row, model, facts, prefixes, None);
    }
    let complete = complete_providers.get(provider)?;
    withhold_unrouted(complete, model, facts, prefixes)
}

fn withhold_unrouted(
    complete: &CompleteProvider,
    model: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
) -> Option<fusiform_protocol::WireCorrectionWire> {
    let row = WireCorrection {
        provider: complete.provider.clone(),
        model: model.to_string(),
        field: complete.field.clone(),
        value: None,
        withheld: true,
        source_ref: complete.source_ref.clone(),
        quote: None,
        notes: None,
        established_by: complete.established_by.clone(),
        established_at_ms: complete.established_at_ms,
        review_by_ms: complete.review_by_ms,
    };
    apply_row(&row, model, facts, prefixes, Some(&complete.notes))
}

/// Fill a missing endpoint only; upstream claims and per-model corrections win.
pub fn apply_provider_api(
    corrections: &WireCorrections,
    provider: &str,
    model: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
) -> Option<fusiform_protocol::WireCorrectionWire> {
    if facts.contains_key("wire.api")
        || corrections.contains_key(&(provider.to_string(), model.to_string()))
    {
        return None;
    }
    let row = corrections.get(&(provider.to_string(), "*".to_string()))?;
    apply_row(row, model, facts, prefixes, None)
}

/// `notes` is disclosed on the wire; per-model rows pass none, so their
/// explanatory notes stay in the data file as before.
fn apply_row(
    row: &WireCorrection,
    model: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
    notes: Option<&String>,
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
        notes: notes.cloned(),
    })
}
