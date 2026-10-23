//! Sourced endpoint corrections, applied only to current catalog reads.
//!
//! The store retains upstream claims verbatim. A missing route in the pinned
//! client catalog withholds the endpoint rather than guessing a destination.
//! A provider named in the document's `complete_providers` has its routed
//! models listed exhaustively, so any of its models without a per-model row is
//! withheld too, including models upstream adds after the list was read.

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

pub fn corrections() -> &'static WireCorrections {
    static ROWS: std::sync::OnceLock<WireCorrections> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

pub fn complete_providers() -> &'static CompleteProviders {
    static ROWS: std::sync::OnceLock<CompleteProviders> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_complete_providers_reporting_rejects(DOCUMENT, corrections()).0)
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
    provider: &str,
    model: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
) -> Option<fusiform_protocol::WireCorrectionWire> {
    if let Some(row) = corrections().get(&(provider.to_string(), model.to_string())) {
        return apply_row(row, model, facts, prefixes, None);
    }
    let complete = complete_providers().get(provider)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shipped_corrections_load_without_rejects() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.len(), 33);
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
