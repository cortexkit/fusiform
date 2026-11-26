//! Load sourced model membership lists for subscription quota pools.

use std::collections::BTreeSet;

use fusiform_protocol::QuotaPoolWire;
use serde_json::Value;

const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;

/// Validate signed entries without asking today's catalog to confirm an older
/// identity. The publisher checked model membership against its own catalog.
pub fn load_stored_reporting_rejects(doc: &str) -> (Vec<QuotaPoolWire>, Vec<String>) {
    load_where(doc, |_, _| true)
}

/// Validate rows with the caller's upstream-or-current-alias identity check.
/// Keep the first valid claim for each exact provider and pool id.
pub fn load_where(
    doc: &str,
    known_model: impl Fn(&str, &str) -> bool,
) -> (Vec<QuotaPoolWire>, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Document {
        schema: String,
        entries: Vec<Value>,
    }
    let doc = match serde_json::from_str::<Document>(doc) {
        Ok(doc) if doc.schema == "quota-pools/v1" => doc,
        Ok(_) => return (Vec::new(), vec!["expected schema quota-pools/v1".into()]),
        Err(error) => return (Vec::new(), vec![format!("unreadable document: {error}")]),
    };

    let mut rows = Vec::new();
    let mut rejects = Vec::new();
    let mut seen = BTreeSet::new();
    for (i, raw) in doc.entries.into_iter().enumerate() {
        let row: QuotaPoolWire = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!("entry {i} did not parse: {error}"));
                continue;
            }
        };
        let invalid = validate(&row, &known_model).err().or_else(|| {
            (!seen.insert((row.provider_id.clone(), row.pool_id.clone())))
                .then(|| "duplicate quota pool identity".into())
        });
        if let Some(reason) = invalid {
            rejects.push(format!(
                "entry {i} {}/{}: {reason}",
                row.provider_id, row.pool_id
            ));
        } else {
            rows.push(row);
        }
    }
    (rows, rejects)
}

fn validate(row: &QuotaPoolWire, known_model: impl Fn(&str, &str) -> bool) -> Result<(), String> {
    if [&row.provider_id, &row.pool_id, &row.source_ref, &row.quote]
        .iter()
        .any(|field| field.trim().is_empty())
    {
        return Err("empty provider, pool id, source or quote".into());
    }
    if row.established_at_ms <= 0
        || row.review_by_ms.checked_sub(row.established_at_ms) != Some(REVIEW_INTERVAL_MS)
    {
        return Err("review_by_ms must be 60 days after established_at_ms".into());
    }
    if row.models.is_empty() {
        return Err("models must be nonempty".into());
    }
    let mut seen = BTreeSet::new();
    for id in &row.models {
        if !seen.insert(id) {
            return Err(format!("duplicate model {id:?}"));
        }
        let Some((provider, model)) = id
            .split_once('/')
            .filter(|(provider, model)| !provider.trim().is_empty() && !model.trim().is_empty())
        else {
            return Err(format!(
                "model {id:?} must be a provider-qualified catalog id"
            ));
        };
        if !known_model(provider, model) {
            return Err(format!(
                "model {id:?} is neither an upstream model nor a current alias"
            ));
        }
    }
    Ok(())
}
