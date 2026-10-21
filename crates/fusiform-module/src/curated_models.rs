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
    /// Who read the sources, when the file records it. Not served.
    #[serde(default)]
    pub established_by: Option<String>,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

pub type CuratedModels = BTreeMap<(String, String), CuratedModel>;

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
        if key.starts_with("rate.") {
            if let Some(reason) = invalid_rate(value) {
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
fn invalid_rate(value: &serde_json::Value) -> Option<String> {
    let keys: std::collections::BTreeSet<&str> = value
        .as_object()
        .map(|obj| obj.keys().map(String::as_str).collect())
        .unwrap_or_default();
    match serde_json::from_value::<fusiform_protocol::RateValue>(value.clone()) {
        Ok(fusiform_protocol::RateValue::Priced {
            amount,
            inherited_from: None,
            ..
        }) if amount.units > 0
            && amount.unit_provenance == fusiform_protocol::money::UnitProvenance::Stated
            && keys
                == std::collections::BTreeSet::from([
                    "state",
                    "units",
                    "exponent",
                    "currency",
                    "unit_provenance",
                ]) =>
        {
            None
        }
        Ok(fusiform_protocol::RateValue::StatedZero {
            inherited_from: None,
        }) if keys == std::collections::BTreeSet::from(["state"]) => None,
        _ => Some(format!(
            "must be a priced rate with a stated currency, or a stated zero: {value}"
        )),
    }
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

    /// The rows the shipped file carries, named so a row that silently fails to
    /// load (or a new one) changes this test rather than passing a count.
    const SHIPPED: &[(&str, &str)] = &[
        ("cloudflare-workers-ai", "@cf/cloudflare/clairic"),
        ("cloudflare-workers-ai", "@cf/cloudflare/clef"),
        ("cloudflare-workers-ai", "@cf/cloudflare/clef-flash"),
        ("meta", "muse-wonder-full-1.0"),
        ("typesafe", "jev-1.13.0"),
    ];

    /// The shipped document with one edit applied to the row of that identity.
    fn planted_in(
        provider: &str,
        model: &str,
        edit: impl FnOnce(&mut serde_json::Value),
    ) -> String {
        let mut doc: serde_json::Value = serde_json::from_str(DOCUMENT).unwrap();
        let row = doc["models"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|row| row["provider_id"] == provider && row["model_id"] == model)
            .expect("the planted row is shipped");
        edit(row);
        serde_json::to_string(&doc).unwrap()
    }

    fn planted(edit: impl FnOnce(&mut serde_json::Value)) -> String {
        planted_in("meta", "muse-wonder-full-1.0", edit)
    }

    /// Load a planted document and return its single reject, after checking
    /// that exactly the planted row was dropped and every other row loaded.
    fn single_reject(doc: &str, provider: &str, model: &str) -> String {
        let (rows, rejects) = load_reporting_rejects(doc);
        assert_eq!(rejects.len(), 1, "{rejects:?}");
        assert_eq!(rows.len(), SHIPPED.len() - 1);
        assert!(!rows.contains_key(&(provider.to_string(), model.to_string())));
        assert!(rejects[0].contains(&format!("{provider}/{model}")));
        rejects[0].clone()
    }

    #[test]
    fn shipped_curated_rows_load_without_rejects() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        let ids: Vec<(&str, &str)> = rows
            .keys()
            .map(|(provider, model)| (provider.as_str(), model.as_str()))
            .collect();
        assert_eq!(ids, SHIPPED);
    }

    #[test]
    fn upstream_now_publishes_this_row_remove_the_curated_row() {
        let seed = serde_json::from_str(include_str!("../data/models-dev-seed.json")).unwrap();
        let collisions = upstream_collisions(models(), &seed);
        assert!(collisions.is_empty(), "{}", collisions.join("\n"));
    }

    #[test]
    fn planted_upstream_row_triggers_the_removal_fence() {
        // Every shipped identity, one at a time, so the fence is shown to see
        // each row rather than only the first one in the file.
        for (provider, model) in SHIPPED {
            let mut seed = serde_json::json!({});
            seed[*provider]["models"][*model] = serde_json::json!({});
            let collisions = upstream_collisions(models(), &seed);
            assert_eq!(collisions.len(), 1, "{provider}/{model}: {collisions:?}");
            assert!(collisions[0].starts_with(&format!("{provider}/{model}:")));
            assert!(
                collisions[0].contains("upstream now publishes this row; remove the curated row")
            );
        }
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
        // Checked per row: rows carry different review dates, so an instant
        // that makes one row overdue may make others overdue too.
        assert_eq!(models().len(), SHIPPED.len());
        for row in models().values() {
            let id = format!("{}/{}:", row.provider_id, row.model_id);
            assert!(!overdue_at(models(), row.review_by_ms)
                .iter()
                .any(|line| line.starts_with(&id)));
            let overdue = overdue_at(models(), row.review_by_ms + 1);
            let line = overdue
                .iter()
                .find(|line| line.starts_with(&id))
                .unwrap_or_else(|| panic!("{id} must be overdue after its review date"));
            assert!(line.contains("re-verify"));
            assert!(line.contains("or remove"));
        }
    }

    #[test]
    fn planted_unknown_fact_is_rejected() {
        let doc = planted(|row| row["facts"]["model.name"] = "Muse Wonder Full".into());
        let reject = single_reject(&doc, "meta", "muse-wonder-full-1.0");
        assert!(reject.contains("not in SERVED_FACTS"));
    }

    #[test]
    fn planted_subscription_price_is_rejected() {
        let doc = planted(|row| {
            row["facts"]["rate.input"] = serde_json::json!({"state":"priced","units":1})
        });
        let reject = single_reject(&doc, "meta", "muse-wonder-full-1.0");
        assert!(reject.contains("must not carry a list price"));
        // An apikey row that also admits a subscription login is refused too:
        // the price would be served to the subscription read.
        let doc = planted_in("typesafe", "jev-1.13.0", |row| {
            row["auth_methods"] = serde_json::json!(["apikey", "oauth"])
        });
        let reject = single_reject(&doc, "typesafe", "jev-1.13.0");
        assert!(reject.contains("must not carry a list price"));
    }

    #[test]
    fn planted_model_kind_outside_the_vocabulary_is_rejected() {
        for bad in [
            serde_json::json!("chat"),
            serde_json::json!("Decision"),
            serde_json::json!(""),
            serde_json::json!(null),
            serde_json::json!(["decision"]),
        ] {
            let doc = planted_in("cloudflare-workers-ai", "@cf/cloudflare/clef", |row| {
                row["facts"]["model.kind"] = bad.clone()
            });
            let reject = single_reject(&doc, "cloudflare-workers-ai", "@cf/cloudflare/clef");
            assert!(
                reject.contains("model.kind must be one of"),
                "{bad}: {reject}"
            );
        }
        // Control: every value in the vocabulary loads.
        for kind in fusiform_protocol::MODEL_KINDS {
            let doc = planted_in("cloudflare-workers-ai", "@cf/cloudflare/clef", |row| {
                row["facts"]["model.kind"] = (*kind).into()
            });
            let (rows, rejects) = load_reporting_rejects(&doc);
            assert!(rejects.is_empty(), "{kind}: {rejects:?}");
            assert_eq!(rows.len(), SHIPPED.len());
        }
    }

    #[test]
    fn planted_rate_outside_the_served_shape_is_rejected() {
        let priced = serde_json::json!({"state":"priced", "units":42000000, "exponent":9,
            "currency":"USD", "unit_provenance":{"kind":"stated"}});
        let mut assumed = priced.clone();
        assumed["unit_provenance"] = serde_json::json!({"kind":"assumed_by_policy",
            "policy":"models-dev-usd-v1"});
        let mut no_provenance = priced.clone();
        no_provenance
            .as_object_mut()
            .unwrap()
            .remove("unit_provenance");
        let mut extra = priced.clone();
        extra["floor"] = serde_json::json!({"kind":"unknown"});
        let mut zero_units = priced.clone();
        zero_units["units"] = 0.into();
        let borrowed_zero = serde_json::json!({"state":"stated_zero", "inherited_from":
            {"provider_id":"x", "family":"y", "basis":"alias"}});
        for bad in [
            assumed,
            no_provenance,
            extra,
            zero_units,
            borrowed_zero,
            serde_json::json!({"state":"unpriced", "reason":"missing_rate"}),
            serde_json::json!(0.042),
        ] {
            let doc = planted_in("typesafe", "jev-1.13.0", |row| {
                row["facts"]["rate.input"] = bad.clone()
            });
            let reject = single_reject(&doc, "typesafe", "jev-1.13.0");
            assert!(
                reject.contains("rate.input: must be a priced rate"),
                "{bad}: {reject}"
            );
        }
        // Control: the shipped shape itself loads.
        let doc = planted_in("typesafe", "jev-1.13.0", |row| {
            row["facts"]["rate.input"] = priced
        });
        assert!(load_reporting_rejects(&doc).1.is_empty());
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
        assert_eq!(rows.len(), SHIPPED.len());
        assert_eq!(rejects.len(), 2);
        assert!(rejects[0].contains("duplicate"));
        assert!(rejects[1].contains("did not parse"));
    }
}
