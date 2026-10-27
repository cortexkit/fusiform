//! Sourced values served for one current upstream fact.
//!
//! Two kinds of row live here.
//!
//! A REPLACEMENT corrects a value models.dev publishes that is true for one way
//! of calling a model and wrong for the way consumers call it. It replaces that
//! one value on current catalog reads, and only while the stored upstream value
//! is exactly the value the row was sourced against (`replaces`). Once
//! models.dev publishes anything else, the row stops applying and the upstream
//! value is served again, so an override can never hide a later upstream
//! change.
//!
//! An ADDITION (`replaces: null`) supplies a fact models.dev publishes nothing
//! for, sourced from the model vendor's own documentation. It applies only
//! while the stored upstream row carries no such fact at all. If upstream ever
//! records the key, upstream wins and the row stops applying, for the same
//! reason as a replacement.
//!
//! Nothing is written to the store, and point-in-time reads serve the recorded
//! history unchanged.

use std::collections::BTreeMap;

const DOCUMENT: &str = include_str!("../data/fact-overrides.json");
const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;

/// The facts a row may serve.
///
/// Limits are plain integers that nothing else derives at serve time. Other
/// facts already have their own curated mechanism or serve-time derivation
/// (rates: inheritance and billing planes; `wire.*`: wire corrections;
/// `model.canonical_id`: canonical links), and a second mechanism writing the
/// same fact would make the served value depend on which one ran last.
///
/// `capability.server_side_fallback` is a vendor-documented request feature
/// that models.dev has no field for, so it can only arrive as an addition.
pub const OVERRIDABLE_FACTS: [&str; 3] = [
    "limit.context",
    "limit.output",
    "capability.server_side_fallback",
];

/// The facts only an addition row can serve, because models.dev publishes no
/// value for them. A read filtered to one of these must still reach the rows
/// an addition applies to, even though the store holds no matching fact.
pub const ADDED_FACTS: [&str; 1] = ["capability.server_side_fallback"];

/// What `value` and `replaces` must be for a fact key, or why the row is
/// invalid.
fn invalid_values(row: &FactOverride) -> Option<&'static str> {
    match row.fact_key.as_str() {
        // Limits are always stored, as null when models.dev omits them, so an
        // absent limit only means a correction withheld it. An addition could
        // therefore never apply except over a withholding, which it must not
        // undo; only replacements are allowed.
        "limit.context" | "limit.output" => {
            if !positive_integer(&row.value) || !positive_integer(&row.replaces) {
                Some("value and replaces must be positive integers")
            } else if row.value == row.replaces {
                Some("value must differ from the upstream value it replaces")
            } else {
                None
            }
        }
        "capability.server_side_fallback" => {
            if !row.value.is_boolean() {
                Some("capability.server_side_fallback value must be a boolean")
            } else if !row.replaces.is_null() {
                Some("capability.server_side_fallback rows must be additions: models.dev publishes no such field, so replaces must be null")
            } else {
                None
            }
        }
        _ => Some("only limit.context, limit.output and capability.server_side_fallback may be overridden"),
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactOverride {
    pub provider_id: String,
    pub model_id: String,
    pub fact_key: String,
    pub value: serde_json::Value,
    /// The exact upstream value this row corrects, or null for an addition:
    /// a fact upstream publishes no value for. Required either way, so an
    /// addition is always written out as one rather than by omission.
    #[serde(deserialize_with = "required_value")]
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

/// Deserialize a field that must be present even when its value is null.
///
/// serde treats a field with its own deserializer as mandatory, so a
/// replacement row that forgot `replaces` is rejected rather than read as an
/// addition whose `replaces` is null.
fn required_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<serde_json::Value, D::Error> {
    serde::Deserialize::deserialize(deserializer)
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
        } else if let Some(reason) = invalid_values(&row) {
            Some(reason)
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
                row.provider_id, row.model_id, row.fact_key, row.source_ref, upstream_expectation(row)
            )
        })
        .collect()
}

/// What a row expects upstream to publish, as a reviewer reads it.
fn upstream_expectation(row: &FactOverride) -> String {
    if row.replaces.is_null() {
        "no value for this fact".to_string()
    } else {
        row.replaces.to_string()
    }
}

/// Rows the embedded models.dev seed would not let apply.
///
/// The seed is normalized exactly as ingest stores it, so this asks the same
/// question `apply` asks of a stored row. A replacement is stale when the
/// seed's stored value is not its `replaces`; an addition is stale when the
/// seed's row carries the fact at all, which means models.dev now publishes
/// it and upstream wins. Either way, and when the model is missing from the
/// seed, a store seeded from that copy would never apply the row: upstream has
/// moved (re-source the override or remove it) or the row was written against
/// the wrong value.
pub fn stale_against_seed(rows: &FactOverrides, seed: &serde_json::Value) -> Vec<String> {
    let bytes = serde_json::to_vec(seed).expect("a JSON value serializes");
    let catalog = match fusiform_core::normalize::normalize_models_dev(&bytes) {
        Ok(outcome) => outcome.catalog,
        Err(error) => return vec![format!("the embedded seed does not normalize: {error:?}")],
    };
    let mut stored: BTreeMap<(&str, &str), BTreeMap<String, serde_json::Value>> = BTreeMap::new();
    for model in catalog.models() {
        let facts = fusiform_store::ingest::facts_with_values(model)
            .into_iter()
            .map(|(key, raw)| {
                let value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw));
                (key.as_str().to_string(), value)
            })
            .collect();
        stored.insert((&model.key.provider_id, &model.key.model_id), facts);
    }
    rows.values()
        .filter_map(|row| {
            stale_row(
                row,
                stored.get(&(row.provider_id.as_str(), row.model_id.as_str())),
            )
        })
        .collect()
}

/// Why a row would not apply over this stored model row, or `None` when it
/// would. `stored` is `None` when the seed does not carry the model.
fn stale_row(
    row: &FactOverride,
    stored: Option<&BTreeMap<String, serde_json::Value>>,
) -> Option<String> {
    let Some(facts) = stored else {
        return Some(format!(
            "{}/{} {}: the embedded seed does not carry this model; re-verify the model id, or remove the override",
            row.provider_id, row.model_id, row.fact_key
        ));
    };
    let published = facts.get(&row.fact_key);
    let applies = if row.replaces.is_null() {
        published.is_none()
    } else {
        published == Some(&row.replaces)
    };
    (!applies).then(|| {
        let published = published.map_or("no value".to_string(), |v| v.to_string());
        format!(
            "{}/{} {}: the embedded seed publishes {published}, not {}; upstream wins, so re-verify and re-source the override, or remove it",
            row.provider_id, row.model_id, row.fact_key, upstream_expectation(row)
        )
    })
}

/// Apply every shipped row for this model that upstream still lets apply,
/// returning one disclosure per applied row.
///
/// `withheld` names this model's facts that a correction keeps out of the
/// read. They are absent from `facts`, and an addition must not mistake that
/// absence for "upstream publishes nothing" and serve a value over the
/// withholding.
pub fn apply(
    provider_id: &str,
    model_id: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
    withheld: &[&str],
) -> Vec<fusiform_protocol::FactOverrideWire> {
    apply_rows(
        overrides(),
        provider_id,
        model_id,
        facts,
        prefixes,
        withheld,
    )
}

fn apply_rows(
    rows: &FactOverrides,
    provider_id: &str,
    model_id: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
    withheld: &[&str],
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
        if row.replaces.is_null() {
            // An addition applies only while the stored row has no such fact.
            // Any stored value, null included, means upstream now records the
            // key, and upstream wins. A withheld fact is stored too, just not
            // served.
            if facts.contains_key(fact_key) || withheld.contains(&fact_key.as_str()) {
                continue;
            }
            facts.insert(fact_key.clone(), row.value.clone());
        } else {
            // A replacement applies only while upstream still publishes
            // exactly the value it was sourced against. An absent fact (never
            // published, or withheld by a correction) is not overridden: the
            // row corrects a value, it does not supply a missing one.
            let Some(served) = facts.get_mut(fact_key) else {
                continue;
            };
            if *served != row.replaces {
                continue;
            }
            *served = row.value.clone();
        }
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

    fn fallback(model_id: &str) -> Option<&'static FactOverride> {
        overrides().get(&(
            "anthropic".to_string(),
            model_id.to_string(),
            "capability.server_side_fallback".to_string(),
        ))
    }

    /// The first addition row in the shipped document, for planted variants.
    fn addition_row() -> serde_json::Value {
        document()["overrides"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["replaces"].is_null())
            .expect("the document ships an addition row")
            .clone()
    }

    #[test]
    fn shipped_overrides_load_without_rejects() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.len(), 6);
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
    fn server_side_fallback_is_served_as_anthropic_documents_it() {
        const PAGE: &str =
            "https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback";
        const EXCLUSION: &str = "Claude Haiku 5.5 has no server-side fallback: with `fallbacks: \"default\"`, a declined request stays declined";
        for model in [
            "claude-fable-5",
            "claude-fable-5-1",
            "claude-opus-5",
            "claude-opus-5-5",
        ] {
            let row = fallback(model).unwrap_or_else(|| panic!("{model} has a row"));
            assert_eq!(row.value, json!(true), "{model}");
            assert!(row.replaces.is_null(), "{model} is an addition");
            assert!(row.source_ref.contains(PAGE), "{model}");
            assert!(
                row.quote.contains("server-side-fallback-2026-07-01"),
                "{model}"
            );
            // Haiku 5.5 is the page's only exclusion, so every true row quotes it.
            assert!(row.quote.contains(EXCLUSION), "{model}");
        }
        let haiku = fallback("claude-haiku-5-5").expect("Haiku 5.5 has a row");
        assert_eq!(haiku.value, json!(false));
        assert!(haiku.replaces.is_null());
        assert!(haiku.quote.contains(EXCLUSION));
        // Anthropic's page shows Sonnet 5.5 falling back only when a request
        // names its own fallback models, never with `fallbacks: "default"`,
        // and anthropic-auth marks it false, so no value is served for it.
        assert!(fallback("claude-sonnet-5-5").is_none());
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

    /// An addition is stale once the seed's stored row carries the fact.
    ///
    /// models.dev has no such field today, so no edit to the seed can make
    /// ingest store one; the stored facts are planted directly instead, which
    /// is what the gate checks.
    #[test]
    fn planted_upstream_publication_reports_the_stale_addition() {
        let row = fallback("claude-opus-5-5").unwrap();
        let control = BTreeMap::new();
        assert!(stale_row(row, Some(&control)).is_none());
        for published in [json!(false), json!(true), json!(null)] {
            let stored = BTreeMap::from([(row.fact_key.clone(), published.clone())]);
            let stale = stale_row(row, Some(&stored)).expect("upstream wins");
            assert!(stale.contains("anthropic/claude-opus-5-5 capability.server_side_fallback"));
            assert!(stale.contains(&format!("publishes {published}")), "{stale}");
            assert!(stale.contains("not no value for this fact"), "{stale}");
        }
        // A model the seed does not carry can never receive the addition.
        let missing = stale_row(row, None).expect("missing model");
        assert!(missing.contains("does not carry this model"));
        // And through the real gate, a model removed from the seed is named.
        let mut seed = seed();
        seed["anthropic"]["models"]
            .as_object_mut()
            .unwrap()
            .remove("claude-haiku-5-5");
        let stale = stale_against_seed(overrides(), &seed);
        assert_eq!(stale.len(), 1, "{stale:?}");
        assert!(stale[0].contains("anthropic/claude-haiku-5-5 capability.server_side_fallback"));
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
            // A limit is stored as null when unpublished, so a limit addition
            // could only ever fill a withheld fact.
            ("replaces", json!(null), "positive integers"),
        ] {
            let mut row = document()["overrides"][0].clone();
            row[field] = value;
            let (rows, rejects) = load_reporting_rejects(&json!({"overrides": [row]}).to_string());
            assert!(rows.is_empty(), "{field}");
            assert_eq!(rejects.len(), 1, "{field}");
            assert!(rejects[0].contains(reason), "{field}: {rejects:?}");
        }
    }

    #[test]
    fn malformed_addition_rows_are_rejected() {
        for (field, value, reason) in [
            ("value", json!(null), "must be a boolean"),
            ("value", json!("true"), "must be a boolean"),
            ("value", json!(1), "must be a boolean"),
            ("replaces", json!(false), "must be additions"),
            ("replaces", json!(true), "must be additions"),
            ("quote", json!(""), "empty identity"),
            ("source_ref", json!(" "), "empty identity"),
            ("review_by_ms", json!(1), "review_by_ms"),
        ] {
            let mut row = addition_row();
            row[field] = value;
            let (rows, rejects) = load_reporting_rejects(&json!({"overrides": [row]}).to_string());
            assert!(rows.is_empty(), "{field}");
            assert_eq!(rejects.len(), 1, "{field}");
            assert!(rejects[0].contains(reason), "{field}: {rejects:?}");
        }
        // An addition must say so: a row without `replaces` is not read as one.
        let mut row = addition_row();
        row.as_object_mut().unwrap().remove("replaces");
        let (rows, rejects) = load_reporting_rejects(&json!({"overrides": [row]}).to_string());
        assert!(rows.is_empty());
        assert!(rejects[0].contains("did not parse"), "{rejects:?}");
        // Two additions for one identity are a duplicate.
        let row = addition_row();
        let (rows, rejects) =
            load_reporting_rejects(&json!({"overrides": [row.clone(), row]}).to_string());
        assert_eq!(rows.len(), 1);
        assert!(rejects[0].contains("duplicate"), "{rejects:?}");
    }

    #[test]
    fn an_addition_applies_only_while_upstream_has_no_such_fact() {
        const KEY: &str = "capability.server_side_fallback";
        let mut served = facts(json!(1));
        let applied = apply("anthropic", "claude-opus-5-5", &mut served, None, &[]);
        assert_eq!(served[KEY], json!(true));
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].model, "anthropic/claude-opus-5-5");
        assert_eq!(applied[0].fact_key, KEY);
        assert!(applied[0].replaces.is_null());

        let mut served = facts(json!(1));
        apply("anthropic", "claude-haiku-5-5", &mut served, None, &[]);
        assert_eq!(served[KEY], json!(false));

        // Any stored value, null included, is upstream publishing the key.
        for published in [json!(false), json!(true), json!(null)] {
            let mut served = facts(json!(1));
            served.insert(KEY.to_string(), published.clone());
            assert!(apply("anthropic", "claude-opus-5-5", &mut served, None, &[]).is_empty());
            assert_eq!(served[KEY], published);
        }
        // A withheld fact is stored and hidden, not unpublished.
        let mut served = facts(json!(1));
        assert!(apply("anthropic", "claude-opus-5-5", &mut served, None, &[KEY]).is_empty());
        assert!(!served.contains_key(KEY));
        // A filter that excludes the key adds nothing; one naming it does.
        let limits = vec!["limit.".to_string()];
        let mut served = facts(json!(1));
        assert!(apply(
            "anthropic",
            "claude-opus-5-5",
            &mut served,
            Some(&limits),
            &[]
        )
        .is_empty());
        assert!(!served.contains_key(KEY));
        let capability = vec!["capability.".to_string()];
        assert_eq!(
            apply(
                "anthropic",
                "claude-opus-5-5",
                &mut served,
                Some(&capability),
                &[]
            )
            .len(),
            1
        );
        // The same model id under another provider is not the Claude API.
        let mut served = facts(json!(1));
        assert!(apply("opencode-go", "claude-haiku-5-5", &mut served, None, &[]).is_empty());
        assert!(!served.contains_key(KEY));
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
        let applied = apply("google", "gemini-4-argon", &mut served, None, &[]);
        assert_eq!(served["limit.output"], json!(65_536));
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].model, "google/gemini-4-argon");
        assert_eq!(applied[0].replaces, json!(32_768));

        // Upstream moved to any other value, including the override's own:
        // upstream wins and nothing is disclosed.
        for moved in [json!(65_536), json!(49_152), json!(null)] {
            let mut served = facts(moved.clone());
            assert!(apply("google", "gemini-4-argon", &mut served, None, &[]).is_empty());
            assert_eq!(served["limit.output"], moved);
        }
        // An absent fact is not supplied.
        let mut served = BTreeMap::new();
        assert!(apply("google", "gemini-4-argon", &mut served, None, &[]).is_empty());
        assert!(served.is_empty());
        // Another model, or another provider serving the same id, is untouched.
        for (provider, model) in [
            ("google", "gemini-3.8-flash"),
            ("openrouter", "gemini-4-argon"),
        ] {
            let mut served = facts(json!(32_768));
            assert!(apply(provider, model, &mut served, None, &[]).is_empty());
            assert_eq!(served["limit.output"], json!(32_768));
        }
    }

    #[test]
    fn a_filter_excluding_the_fact_applies_nothing() {
        let mut served = facts(json!(32_768));
        let rates = vec!["rate.".to_string()];
        assert!(apply("google", "gemini-4-argon", &mut served, Some(&rates), &[]).is_empty());
        assert_eq!(served["limit.output"], json!(32_768));
        let limits = vec!["limit.".to_string()];
        assert_eq!(
            apply("google", "gemini-4-argon", &mut served, Some(&limits), &[]).len(),
            1
        );
    }
}
