//! Sourced values served for one upstream fact, with dated claim history.
//!
//! Two kinds of row live here.
//!
//! A REPLACEMENT corrects a value models.dev publishes that is true for one way
//! of calling a model and wrong for the way consumers call it. It replaces that
//! one value on catalog reads, and only while the stored upstream value
//! is exactly the value the row was sourced against (`replaces`). Once
//! models.dev publishes anything else, the row stops applying and the upstream
//! value is served again, so an override can never hide a later upstream
//! change. `replaces` may be `null` when the fact is one ingest always stores,
//! as `null` when models.dev publishes nothing (`model.status`): the row then
//! applies only while that stored `null` is there.
//!
//! An ADDITION supplies a fact models.dev publishes nothing for and ingest does
//! not store at all (see `ADDED_FACTS`), sourced from the model vendor's own
//! documentation. Its `replaces` is `null`. It applies only
//! while the stored upstream row carries no such fact at all. If upstream ever
//! records the key, upstream wins and the row stops applying, for the same
//! reason as a replacement.
//!
//! Current reads use the compiled file. Every boot records claim changes in
//! `fact_override_era`; point-in-time reads apply the claim in force, with its
//! guard evaluated against upstream at the requested instant. Provenance-only
//! edits do not open an era or rewrite its original provenance.

use std::collections::BTreeMap;

const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;

/// The facts a row may serve.
///
/// Limits are plain integers that nothing else derives at serve time. Other
/// facts already have their own curated mechanism or serve-time derivation
/// (most rates: inheritance and billing planes; `wire.*`: wire corrections;
/// `model.canonical_id`: canonical links), and a second mechanism writing the
/// same fact would make the served value depend on which one ran last.
///
/// `capability.server_side_fallback` and `capability.mid_conversation_system`
/// are vendor-documented request features that models.dev has no fields for,
/// so they can only arrive as additions.
///
/// `capability.allowed_fallback_models` is the vendor's list of models a
/// refused request may be retried on, in the vendor's order, and
/// `route.refusal_fallback.<category>` is a retry target the vendor has
/// announced for one refusal category. models.dev publishes neither, so both
/// are additions. An empty list is a value: the vendor permits no target.
/// Only categories with an announced target have a key here; a category
/// without one has no fact, which a consumer reads as unknown.
///
/// `capability.reasoning_default` is the reasoning effort a vendor documents
/// its own API applying when a request names none. models.dev publishes no
/// default, so it too can only arrive as an addition, and it is served only
/// while the row's own `capability.reasoning_options` lists that effort: a
/// runner that sends the default explicitly must never send an effort the
/// row says the model refuses.
///
/// `model.status` can only replace the `null` ingest stores when models.dev
/// publishes no status, and only with `"deprecated"`, for a model whose
/// vendor marks it deprecated before models.dev does. Consumers stop routing
/// to deprecated rows, so leaving the mark off serves a dead model as live
/// until models.dev catches up.
/// The image model's mixed output price is split with a guarded text-rate
/// replacement and an image-rate addition, before alias and plane projection.
/// `capability.temperature` replaces only a published boolean when the vendor
/// documents different acceptance; it never fills an unknown upstream claim.
pub const OVERRIDABLE_FACTS: [&str; 13] = [
    "limit.context",
    "limit.output",
    "capability.server_side_fallback",
    "capability.mid_conversation_system",
    "capability.allowed_fallback_models",
    "route.refusal_fallback.bio",
    "route.refusal_fallback.cyber",
    "capability.reasoning_default",
    "capability.temperature",
    "model.status",
    "rate.output",
    "rate.output_image",
    "rate.cached_input_audio",
];

/// The facts only an addition row can serve, because models.dev publishes no
/// value for them. A read filtered to one of these must still reach the rows
/// an addition applies to, even though the store holds no matching fact.
pub const ADDED_FACTS: [&str; 8] = [
    "capability.server_side_fallback",
    "capability.mid_conversation_system",
    "capability.allowed_fallback_models",
    "route.refusal_fallback.bio",
    "route.refusal_fallback.cyber",
    "capability.reasoning_default",
    "rate.output_image",
    "rate.cached_input_audio",
];

/// Whether a row supplies a fact the stored row lacks (an addition), rather
/// than replacing a stored value. The fact key decides, not `replaces`: a
/// `model.status` row replaces a stored `null`, so its `replaces` is `null`
/// too, yet it must apply only while that `null` is stored.
fn is_addition(row: &FactOverride) -> bool {
    ADDED_FACTS.contains(&row.fact_key.as_str())
}

/// What `value` and `replaces` must be for a fact key, or why the row is
/// invalid.
fn invalid_values(row: &FactOverride) -> Option<&'static str> {
    match row.fact_key.as_str() {
        "rate.output" => {
            if crate::curated_models::invalid_rate(&row.value, false).is_some()
                || !plain_token_rate(&row.replaces)
            {
                Some("rate.output needs a sourced token rate and a plain upstream token rate to replace")
            } else if matches_replaced(row, &row.value) {
                Some("value must differ from the upstream value it replaces")
            } else {
                None
            }
        }
        "rate.output_image" | "rate.cached_input_audio" => {
            if crate::curated_models::invalid_rate(&row.value, false).is_some()
                || !row.replaces.is_null()
            {
                Some("rate addition must add a sourced token rate with replaces null")
            } else {
                None
            }
        }
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
        "capability.temperature" => {
            if !row.value.is_boolean() || !row.replaces.is_boolean() {
                Some("temperature value and replaces must be booleans")
            } else if row.value == row.replaces {
                Some("value must differ from the upstream value it replaces")
            } else {
                None
            }
        }
        "capability.server_side_fallback" | "capability.mid_conversation_system" => {
            if !row.value.is_boolean() {
                Some("capability addition value must be a boolean")
            } else if !row.replaces.is_null() {
                Some("capability rows must be additions: models.dev publishes no such field, so replaces must be null")
            } else {
                None
            }
        }
        "capability.allowed_fallback_models" => {
            if !model_id_list(&row.value) {
                Some("allowed fallback models must be a list of distinct nonempty model ids, possibly empty")
            } else if !row.replaces.is_null() {
                Some("capability rows must be additions: models.dev publishes no such field, so replaces must be null")
            } else {
                None
            }
        }
        "route.refusal_fallback.bio" | "route.refusal_fallback.cyber" => {
            if row.value.as_str().is_none_or(|id| id.trim().is_empty()) {
                Some("a refusal fallback route must name one nonempty model id")
            } else if row.value.as_str() == Some(row.model_id.as_str()) {
                Some("a refusal fallback route cannot name the refused model itself")
            } else if !row.replaces.is_null() {
                Some("route rows must be additions: models.dev publishes no such field, so replaces must be null")
            } else {
                None
            }
        }
        "capability.reasoning_default" => {
            if default_effort(&row.value).is_none()
                || row.value.as_object().is_none_or(|object| object.len() != 2)
            {
                Some("reasoning default value must be exactly {\"type\":\"effort\",\"value\":<nonempty effort>}")
            } else if !row.replaces.is_null() {
                Some("capability rows must be additions: models.dev publishes no such field, so replaces must be null")
            } else {
                None
            }
        }
        // Only the vendor's deprecation is served. Other statuses (beta,
        // alpha) change nothing a consumer does, and a status models.dev
        // publishes is never replaced: the row applies only over the `null`
        // ingest stores when models.dev publishes none.
        "model.status" => {
            if row.value != serde_json::json!("deprecated") {
                Some("model.status value must be \"deprecated\"")
            } else if !row.replaces.is_null() {
                Some("model.status rows replace only the null stored when models.dev publishes no status, so replaces must be null")
            } else {
                None
            }
        }
        _ => Some("only limit.context, limit.output, capability.server_side_fallback, capability.mid_conversation_system, capability.allowed_fallback_models, route.refusal_fallback.bio, route.refusal_fallback.cyber, capability.reasoning_default, capability.temperature, model.status, rate.output, rate.output_image and rate.cached_input_audio may be overridden"),
    }
}

/// Whether a value is a list of distinct nonempty model ids. The vendor's
/// order is kept as given, and an empty list is allowed.
fn model_id_list(value: &serde_json::Value) -> bool {
    value.as_array().is_some_and(|ids| {
        let mut seen = std::collections::BTreeSet::new();
        ids.iter().all(|id| {
            id.as_str()
                .is_some_and(|id| !id.trim().is_empty() && seen.insert(id))
        })
    })
}

/// The effort a `capability.reasoning_default` value names, when it has the
/// one shape the fact allows: `{"type":"effort","value":"<effort>"}`.
fn default_effort(value: &serde_json::Value) -> Option<&str> {
    (value["type"] == "effort")
        .then(|| value["value"].as_str())
        .flatten()
        .filter(|effort| !effort.trim().is_empty())
}

/// Whether a row's `capability.reasoning_options` lists `effort` among the
/// values of an entry of type `effort`. Absent or unreadable options list
/// nothing, so a default is never served beside options that do not name it.
pub fn options_list_effort(options: Option<&serde_json::Value>, effort: &str) -> bool {
    options
        .and_then(serde_json::Value::as_array)
        .is_some_and(|options| {
            options.iter().any(|option| {
                option["type"] == "effort"
                    && option["values"]
                        .as_array()
                        .is_some_and(|values| values.iter().any(|v| v.as_str() == Some(effort)))
            })
        })
}

/// Whether an addition's value agrees with the other facts of the row it is
/// added to. Only a reasoning default depends on another fact: the effort it
/// names must be one the row's own options list, so a consumer that sends the
/// default never sends an effort the row says the model refuses.
fn consistent_with_row(row: &FactOverride, facts: &BTreeMap<String, serde_json::Value>) -> bool {
    if row.fact_key != "capability.reasoning_default" {
        return true;
    }
    default_effort(&row.value).is_some_and(|effort| {
        options_list_effort(facts.get("capability.reasoning_options"), effort)
    })
}

fn plain_token_rate(value: &serde_json::Value) -> bool {
    let mut annotated = value.clone();
    let Some(object) = annotated.as_object_mut() else {
        return false;
    };
    if object.contains_key("unit_provenance") {
        return false;
    }
    object.insert(
        "unit_provenance".into(),
        serde_json::json!({"kind":"stated"}),
    );
    crate::curated_models::invalid_rate(&annotated, false).is_none()
}

/// Storage omits currency provenance; rendering adds that annotation before
/// overrides run. Compare the upstream amount, not the renderer's annotation,
/// so the same guard works against both seed facts and current served facts.
fn matches_replaced(row: &FactOverride, value: &serde_json::Value) -> bool {
    if row.fact_key == "rate.output" {
        let mut plain = value.clone();
        if let Some(object) = plain.as_object_mut() {
            object.remove("unit_provenance");
        }
        plain == row.replaces
    } else {
        value == &row.replaces
    }
}

fn disclosed_replacement(row: &FactOverride) -> serde_json::Value {
    let mut value = row.replaces.clone();
    if row.fact_key == "rate.output" {
        // A stored models.dev rate has no provenance field. Its wire disclosure
        // needs the same named currency policy as an ordinary served rate so
        // consumers can decode every monetary value with the published type.
        value["unit_provenance"] = serde_json::json!({
            "kind":"assumed_by_policy",
            "policy":fusiform_core::PolicyId::models_dev_usd_v1().0,
        });
    }
    value
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
    if is_addition(row) {
        "no value for this fact".to_string()
    } else {
        row.replaces.to_string()
    }
}

/// Why a row would not apply over this stored model row, or `None` when it
/// would. `stored` is `None` when the seed does not carry the model.
pub fn stale_row(
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
    if published.is_none() && is_addition(row) && !consistent_with_row(row, facts) {
        return Some(format!(
            "{}/{} {}: the embedded seed's capability.reasoning_options {} do not list {}, so the default would never be served; re-verify the vendor's default against the model's listed efforts, or remove the override",
            row.provider_id,
            row.model_id,
            row.fact_key,
            facts
                .get("capability.reasoning_options")
                .map_or("(absent)".to_string(), |v| v.to_string()),
            row.value
        ));
    }
    let applies = if is_addition(row) {
        published.is_none()
    } else {
        published.is_some_and(|value| matches_replaced(row, value))
    };
    (!applies).then(|| {
        let published = published.map_or("no value".to_string(), |v| v.to_string());
        format!(
            "{}/{} {}: the embedded seed publishes {published}, not {}; upstream wins, so re-verify and re-source the override, or remove it",
            row.provider_id, row.model_id, row.fact_key, upstream_expectation(row)
        )
    })
}

pub fn apply_rows(
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
        if is_addition(row) {
            // An addition applies only while the stored row has no such fact.
            // Any stored value, null included, means upstream now records the
            // key, and upstream wins. A withheld fact is stored too, just not
            // served.
            if facts.contains_key(fact_key) || withheld.contains(&fact_key.as_str()) {
                continue;
            }
            // A value that disagrees with the row's other facts, such as a
            // reasoning default its options no longer list, is not served:
            // upstream has moved and the row needs review.
            if !consistent_with_row(row, facts) {
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
            if !matches_replaced(row, served) {
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
            replaces: disclosed_replacement(row),
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
