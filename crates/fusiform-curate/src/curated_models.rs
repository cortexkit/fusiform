//! Model rows models.dev does not carry, backed by explicit probes and lab
//! records.
//!
//! Provider rows missing from models.dev's API catalog cannot be inferred from
//! lab ids: the provider may require a different model id and login method.
//! These rows carry those restrictions and stand down wherever upstream
//! publishes the model.
//!
//! A current read serves the rows straight from the compiled file. Every boot
//! also records the file in the store ([`apply_to_store`]), so a point-in-time
//! read can serve the claim that was in force at its instant.

use std::collections::BTreeMap;

// Each unit-billed key must first be recognised by the consumer that prices
// spend: an unknown rate key makes that consumer refuse a whole pricing day.
// Grow this list one deliberate key at a time, never by a key pattern.
const UNIT_BILLED_RATE_KEYS: &[&str] = &[
    "rate.input_audio.per_second",
    "rate.input.per_character",
    "rate.request.per_request",
];

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CuratedModel {
    pub provider_id: String,
    pub model_id: String,
    pub auth_methods: Vec<String>,
    pub facts: BTreeMap<String, serde_json::Value>,
    pub source_ref: String,
    /// Who read the sources, when the file records it. Not served.
    #[serde(default)]
    pub established_by: Option<String>,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

pub type CuratedModels = BTreeMap<(String, String), CuratedModel>;

/// A row in `models` that fusiform has stopped serving for now.
///
/// The row's data stays in `models` untouched, so serving it again is a
/// one-line removal from `held`. While held, the row is never served as
/// curated. If the store has never recorded the id, a current read reports it
/// the way it reports a retired alias: absent on a retired-inclusive read and
/// listed in `retired` on a named read. If the store has recorded it, the
/// store's own row (present or withdrawn) answers instead. Deleting the row instead would make
/// the id vanish, and a consumer that tracks identities reads a vanished id as
/// a truncated catalog and refuses the refresh.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldModel {
    pub provider_id: String,
    pub model_id: String,
    /// When fusiform put the row on hold; served as `retired_at_ms` when the
    /// store has never recorded the id.
    pub held_since_ms: i64,
    /// Why the row is held and what lets it be served again. Not served.
    pub reason: String,
}

pub type HeldModels = BTreeMap<(String, String), HeldModel>;

/// Everything the curated file carries: its rows, and which of them are held.
#[derive(Debug, Default)]
pub struct Table {
    pub models: CuratedModels,
    pub held: HeldModels,
}

/// Reject unsupported facts rather than silently exposing a new wire vocabulary.
/// A row that admits a subscription login must never claim an API list price,
/// so only an `apikey`-only row may carry `rate.*` facts.
fn invalid_facts(row: &CuratedModel) -> Option<String> {
    for (key, value) in &row.facts {
        if key.starts_with("rate.") && row.auth_methods != ["apikey"] {
            return Some(format!(
                "a row admitting a subscription login must not carry a list price: {key}"
            ));
        }
        if !fusiform_protocol::SERVED_FACTS
            .iter()
            .any(|fact| fact.key == key)
            && !UNIT_BILLED_RATE_KEYS.contains(&key.as_str())
        {
            return Some(format!("fact key is not in SERVED_FACTS: {key}"));
        }
        if key == "model.kind"
            && !value
                .as_str()
                .is_some_and(|kind| fusiform_protocol::MODEL_KINDS.contains(&kind))
        {
            return Some(format!(
                "model.kind must be one of {:?}: {value}",
                fusiform_protocol::MODEL_KINDS
            ));
        }
        if key == "wire.api" {
            let valid = value.as_object().is_some_and(|object| {
                object.len() == 2
                    && object.get("from").is_some_and(|from| from == "curated")
                    && object
                        .get("value")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|api| url::Url::parse(api).ok())
                        .is_some_and(|api| api.scheme() == "https" && api.host_str().is_some())
            });
            if !valid {
                return Some("wire.api must be a sourced HTTPS endpoint with from: curated".into());
            }
        } else if key == "wire.family" {
            if !value.as_str().is_some_and(crate::wire_families::is_known) {
                return Some("wire.family must name a verified protocol token".into());
            }
        } else if key.starts_with("wire.") {
            return Some(format!(
                "curated rows may declare only wire.api and wire.family, not {key}"
            ));
        }
        if key.starts_with("rate.") {
            if let Some(reason) = invalid_rate(value, UNIT_BILLED_RATE_KEYS.contains(&key.as_str()))
            {
                return Some(format!("{key}: {reason}"));
            }
        }
    }
    None
}

/// A curated rate is served byte for byte as written, so it must already be in
/// the served shape: a priced amount whose currency the source stated, or a
/// stated zero. Curated rows never borrow a price, never refuse one, and never
/// carry fields the published `RateValue` type would silently drop.
pub(crate) fn invalid_rate(value: &serde_json::Value, unit_billed: bool) -> Option<String> {
    let keys: std::collections::BTreeSet<&str> = value
        .as_object()
        .map(|obj| obj.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let mut priced_keys = std::collections::BTreeSet::from([
        "state",
        "units",
        "exponent",
        "currency",
        "unit_provenance",
    ]);
    if unit_billed {
        priced_keys.extend(["per", "increment", "minimum_quantity"]);
    }
    match serde_json::from_value::<fusiform_protocol::RateValue>(value.clone()) {
        Ok(fusiform_protocol::RateValue::Priced {
            amount,
            per,
            inherited_from: None,
            ..
        }) if amount.units > 0
            && amount.unit_provenance == fusiform_protocol::money::UnitProvenance::Stated
            && keys == priced_keys
            && if unit_billed {
                per.is_some_and(|quantity| quantity > 0)
            } else {
                per.is_none()
            } =>
        {
            None
        }
        Ok(fusiform_protocol::RateValue::StatedZero {
            inherited_from: None,
        }) if !unit_billed && keys == std::collections::BTreeSet::from(["state"]) => None,
        _ => Some(format!(
            "must be a priced rate with a stated currency, or a stated zero: {value}"
        )),
    }
}

/// Load the whole file: its rows, then its held entries checked against them.
pub fn load_table_reporting_rejects(doc: &str) -> (Table, Vec<String>) {
    let (models, mut rejects) = load_reporting_rejects(doc);
    let (held, held_rejects) = load_held_reporting_rejects(doc, &models);
    rejects.extend(held_rejects);
    (Table { models, held }, rejects)
}

/// The `held` entries, each naming a row that loaded.
///
/// A held entry naming no row would hold nothing and report an id nobody
/// serves, so it is a reject, as is an id held twice.
pub fn load_held_reporting_rejects(doc: &str, rows: &CuratedModels) -> (HeldModels, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Doc {
        #[serde(default)]
        held: Vec<serde_json::Value>,
    }
    let doc: Doc = match serde_json::from_str(doc) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                HeldModels::new(),
                vec![format!("curated held list unreadable: {error}")],
            )
        }
    };
    let mut held = HeldModels::new();
    let mut rejects = Vec::new();
    for raw in doc.held {
        let entry: HeldModel = match serde_json::from_value(raw) {
            Ok(entry) => entry,
            Err(error) => {
                rejects.push(format!("curated held entry did not parse: {error}"));
                continue;
            }
        };
        let key = (entry.provider_id.clone(), entry.model_id.clone());
        let invalid = if entry.reason.trim().is_empty() || entry.held_since_ms <= 0 {
            Some("a held entry needs a reason and a positive held_since_ms")
        } else if !rows.contains_key(&key) {
            Some("a held entry must name a row in models")
        } else if held.contains_key(&key) {
            Some("duplicate held identity")
        } else {
            None
        };
        if let Some(reason) = invalid {
            rejects.push(format!("curated held {}/{}: {reason}", key.0, key.1));
        } else {
            held.insert(key, entry);
        }
    }
    (held, rejects)
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
            || row
                .established_by
                .as_deref()
                .is_some_and(|by| by.trim().is_empty())
        {
            Some("empty identity, source, established_by or facts".to_string())
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
