//! Sourced values served in place of one current upstream fact.
//!
//! models.dev sometimes publishes a value that is true for one way of calling
//! a model and wrong for the way consumers call it. A row here replaces that
//! one value on current catalog reads, and only while the stored upstream value
//! is exactly the value the row was sourced against (`replaces`). Once
//! models.dev publishes anything else, the row stops applying and the upstream
//! value is served again, so an override can never hide a later upstream
//! change. Nothing is written to the store, and point-in-time reads serve the
//! recorded history unchanged.

use std::collections::BTreeMap;

const DOCUMENT: &str = include_str!("../data/fact-overrides.json");
const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;

/// The facts a row may override.
///
/// Limits are plain integers that nothing else derives at serve time. Other
/// facts already have their own curated mechanism or serve-time derivation
/// (rates: inheritance and billing planes; `wire.*`: wire corrections;
/// `model.canonical_id`: canonical links), and a second mechanism writing the
/// same fact would make the served value depend on which one ran last.
pub const OVERRIDABLE_FACTS: [&str; 2] = ["limit.context", "limit.output"];

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactOverride {
    pub provider_id: String,
    pub model_id: String,
    pub fact_key: String,
    pub value: serde_json::Value,
    /// The exact upstream value this row corrects.
    pub replaces: serde_json::Value,
    pub reason: String,
    pub source_ref: String,
    pub quote: String,
    #[serde(default)]
    pub notes: Option<String>,
    pub established_by: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

/// Keyed by `(provider_id, model_id, fact_key)`.
pub type FactOverrides = BTreeMap<(String, String, String), FactOverride>;

fn positive_integer(value: &serde_json::Value) -> bool {
    value.as_u64().is_some_and(|n| n > 0)
}

/// Parse each row independently, reporting malformed and duplicate rows
/// rather than silently choosing one.
pub fn load_reporting_rejects(doc: &str) -> (FactOverrides, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Doc {
        overrides: Vec<serde_json::Value>,
    }
    let doc: Doc = match serde_json::from_str(doc) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                BTreeMap::new(),
                vec![format!("fact overrides unreadable: {error}")],
            )
        }
    };
    let mut rows = FactOverrides::new();
    let mut rejects = Vec::new();
    for raw in doc.overrides {
        let row: FactOverride = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!("fact override did not parse: {error}"));
                continue;
            }
        };
        let key = (
            row.provider_id.clone(),
            row.model_id.clone(),
            row.fact_key.clone(),
        );
        let invalid = if [
            &row.provider_id,
            &row.model_id,
            &row.reason,
            &row.source_ref,
            &row.quote,
        ]
        .iter()
        .any(|field| field.trim().is_empty())
        {
            Some("empty identity, reason, source or quote")
        } else if row.notes.as_ref().is_some_and(|n| n.trim().is_empty()) {
            Some("notes, when present, must be nonempty")
        } else if !OVERRIDABLE_FACTS.contains(&row.fact_key.as_str()) {
            Some("only limit.context and limit.output may be overridden")
        } else if !positive_integer(&row.value) || !positive_integer(&row.replaces) {
            Some("value and replaces must be positive integers")
        } else if row.value == row.replaces {
            Some("value must differ from the upstream value it replaces")
        } else if row.established_by != "fusiform" {
            Some("established_by must be fusiform")
        } else if row.established_at_ms <= 0
            || row.review_by_ms.checked_sub(row.established_at_ms) != Some(REVIEW_INTERVAL_MS)
        {
            Some("review_by_ms must be 60 days after established_at_ms")
        } else if rows.contains_key(&key) {
            Some("duplicate fact override identity")
        } else {
            None
        };
        if let Some(reason) = invalid {
            rejects.push(format!(
                "fact override {}/{} {}: {reason}",
                key.0, key.1, key.2
            ));
        } else {
            rows.insert(key, row);
        }
    }
    (rows, rejects)
}

pub fn overrides() -> &'static FactOverrides {
    static ROWS: std::sync::OnceLock<FactOverrides> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

pub fn overdue_at(rows: &FactOverrides, now_ms: i64) -> Vec<String> {
    rows.values()
        .filter(|row| row.review_by_ms < now_ms)
        .map(|row| {
            format!(
                "{}/{} {}: re-verify {} and whether models.dev still publishes {}, then re-date established_at_ms and review_by_ms (60 days later), or remove the override",
                row.provider_id, row.model_id, row.fact_key, row.source_ref, row.replaces
            )
        })
        .collect()
}

/// Rows whose `replaces` is not what the embedded models.dev seed publishes.
///
/// A store seeded from that copy would never apply such a row, so either
/// upstream has moved (re-source the override or remove it) or the row was
/// written against the wrong value.
pub fn stale_against_seed(rows: &FactOverrides, seed: &serde_json::Value) -> Vec<String> {
    rows.values()
        .filter_map(|row| {
            let limit = row.fact_key.strip_prefix("limit.")?;
            let published = &seed[&row.provider_id]["models"][&row.model_id]["limit"][limit];
            (published != &row.replaces).then(|| {
                format!(
                    "{}/{} {}: the embedded seed publishes {published}, not {}; re-verify and re-source the override, or remove it",
                    row.provider_id, row.model_id, row.fact_key, row.replaces
                )
            })
        })
        .collect()
}

/// Apply every shipped row for this model whose upstream value is still the
/// one it replaces, returning one disclosure per applied row.
pub fn apply(
    provider_id: &str,
    model_id: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
) -> Vec<fusiform_protocol::FactOverrideWire> {
    apply_rows(overrides(), provider_id, model_id, facts, prefixes)
}

fn apply_rows(
    rows: &FactOverrides,
    provider_id: &str,
    model_id: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
) -> Vec<fusiform_protocol::FactOverrideWire> {
    let start = (provider_id.to_string(), model_id.to_string(), String::new());
    let mut applied = Vec::new();
    for ((provider, model, fact_key), row) in rows.range(start..) {
        if provider != provider_id || model != model_id {
            break;
        }
        // A fact the caller filtered out is not served, so nothing is
        // overridden and nothing disclosed.
        if prefixes.is_some_and(|p| !p.iter().any(|p| fact_key.starts_with(p.as_str()))) {
            continue;
        }
        // Only while upstream still publishes exactly the value this row was
        // sourced against. An absent fact (never published, or withheld by a
        // correction) is not overridden either: the row corrects a value, it
        // does not supply a missing one.
        let Some(served) = facts.get_mut(fact_key) else {
            continue;
        };
        if *served != row.replaces {
            continue;
        }
        *served = row.value.clone();
        applied.push(fusiform_protocol::FactOverrideWire {
            model: format!("{provider_id}/{model_id}"),
            provider_id: row.provider_id.clone(),
            model_id: row.model_id.clone(),
            fact_key: row.fact_key.clone(),
            value: row.value.clone(),
            replaces: row.replaces.clone(),
            reason: row.reason.clone(),
            source_ref: row.source_ref.clone(),
            quote: row.quote.clone(),
            notes: row.notes.clone(),
            established_by: row.established_by.clone(),
            established_at_ms: row.established_at_ms,
            review_by_ms: row.review_by_ms,
        });
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> serde_json::Value {
        serde_json::from_str(DOCUMENT).unwrap()
    }

    fn seed() -> serde_json::Value {
        serde_json::from_str(include_str!("../data/models-dev-seed.json")).unwrap()
    }

    fn argon() -> &'static FactOverride {
        &overrides()[&(
            "google".to_string(),
            "gemini-4-argon".to_string(),
            "limit.output".to_string(),
        )]
    }

    #[test]
    fn shipped_overrides_load_without_rejects() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.len(), 1);
        let row = argon();
        assert_eq!(row.value, json!(65_536));
        assert_eq!(row.replaces, json!(32_768));
        // The alias inherits this value, so the row cites the route's own
        // declaration of it.
        assert!(row.notes.as_ref().unwrap().contains(
            "cortexkit/antigravity-auth/blob/48919aa7ae9378ea19af17e7d9da5b986395f00a/packages/core/src/model-registry.ts"
        ));
    }

    #[test]
    fn fact_overrides_are_within_their_review_dates() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let overdue = overdue_at(overrides(), now);
        assert!(overdue.is_empty(), "{}", overdue.join("\n"));
    }

    #[test]
    fn planted_overdue_override_fails_the_review_gate() {
        let row = argon().clone();
        let review = row.review_by_ms;
        let rows = FactOverrides::from([(
            (
                row.provider_id.clone(),
                row.model_id.clone(),
                row.fact_key.clone(),
            ),
            row,
        )]);
        assert!(overdue_at(&rows, review).is_empty());
        let overdue = overdue_at(&rows, review + 1);
        assert_eq!(overdue.len(), 1);
        assert!(overdue[0].contains("google/gemini-4-argon limit.output"));
        assert!(overdue[0].contains("re-verify"));
        assert!(overdue[0].contains("or remove"));
    }

    #[test]
    fn overrides_replace_what_the_embedded_seed_publishes() {
        let stale = stale_against_seed(overrides(), &seed());
        assert!(stale.is_empty(), "{}", stale.join("\n"));
    }

    #[test]
    fn planted_upstream_move_reports_the_stale_override() {
        let mut seed = seed();
        seed["google"]["models"]["gemini-4-argon"]["limit"]["output"] = json!(65_536);
        let stale = stale_against_seed(overrides(), &seed);
        assert_eq!(stale.len(), 1);
        assert!(stale[0].contains("google/gemini-4-argon limit.output"));
        assert!(stale[0].contains("publishes 65536"));
    }

    #[test]
    fn malformed_and_duplicate_rows_are_counted_as_rejects() {
        let mut doc = document();
        let row = doc["overrides"][0].clone();
        doc["overrides"]
            .as_array_mut()
            .unwrap()
            .extend([row, json!({})]);
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rows.len(), overrides().len());
        assert_eq!(rejects.len(), 2);
        assert!(rejects[0].contains("duplicate"));
        assert!(rejects[1].contains("did not parse"));
        assert_eq!(load_reporting_rejects("not json").1.len(), 1);
    }

    #[test]
    fn malformed_fields_are_rejected() {
        for (field, value, reason) in [
            ("provider_id", json!(" "), "empty identity"),
            ("model_id", json!(""), "empty identity"),
            ("reason", json!(""), "empty identity"),
            ("source_ref", json!(""), "empty identity"),
            ("quote", json!(" "), "empty identity"),
            ("notes", json!(" "), "notes"),
            ("fact_key", json!("rate.input"), "only limit"),
            ("fact_key", json!("wire.api"), "only limit"),
            ("value", json!(null), "positive integers"),
            ("value", json!("65536"), "positive integers"),
            ("replaces", json!(0), "positive integers"),
            ("value", json!(32_768), "must differ"),
            ("established_by", json!("upstream"), "established_by"),
            ("established_at_ms", json!(0), "review_by_ms"),
            ("review_by_ms", json!(1), "review_by_ms"),
            ("unknown", json!(true), "did not parse"),
        ] {
            let mut row = document()["overrides"][0].clone();
            row[field] = value;
            let (rows, rejects) = load_reporting_rejects(&json!({"overrides": [row]}).to_string());
            assert!(rows.is_empty(), "{field}");
            assert_eq!(rejects.len(), 1, "{field}");
            assert!(rejects[0].contains(reason), "{field}: {rejects:?}");
        }
    }

    fn facts(output: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
        BTreeMap::from([
            ("limit.context".to_string(), json!(1_048_576)),
            ("limit.output".to_string(), output),
        ])
    }

    #[test]
    fn applies_only_while_upstream_publishes_the_replaced_value() {
        let mut served = facts(json!(32_768));
        let applied = apply("google", "gemini-4-argon", &mut served, None);
        assert_eq!(served["limit.output"], json!(65_536));
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].model, "google/gemini-4-argon");
        assert_eq!(applied[0].replaces, json!(32_768));

        // Upstream moved to any other value, including the override's own:
        // upstream wins and nothing is disclosed.
        for moved in [json!(65_536), json!(49_152), json!(null)] {
            let mut served = facts(moved.clone());
            assert!(apply("google", "gemini-4-argon", &mut served, None).is_empty());
            assert_eq!(served["limit.output"], moved);
        }
        // An absent fact is not supplied.
        let mut served = BTreeMap::new();
        assert!(apply("google", "gemini-4-argon", &mut served, None).is_empty());
        assert!(served.is_empty());
        // Another model, or another provider serving the same id, is untouched.
        for (provider, model) in [
            ("google", "gemini-3.8-flash"),
            ("openrouter", "gemini-4-argon"),
        ] {
            let mut served = facts(json!(32_768));
            assert!(apply(provider, model, &mut served, None).is_empty());
            assert_eq!(served["limit.output"], json!(32_768));
        }
    }

    #[test]
    fn a_filter_excluding_the_fact_applies_nothing() {
        let mut served = facts(json!(32_768));
        let rates = vec!["rate.".to_string()];
        assert!(apply("google", "gemini-4-argon", &mut served, Some(&rates)).is_empty());
        assert_eq!(served["limit.output"], json!(32_768));
        let limits = vec!["limit.".to_string()];
        assert_eq!(
            apply("google", "gemini-4-argon", &mut served, Some(&limits)).len(),
            1
        );
    }
}
