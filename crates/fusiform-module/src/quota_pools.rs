//! Resolve quota pool memberships from the native document or signed history.

use std::sync::OnceLock;

use fusiform_protocol::QuotaPoolWire;
use fusiform_store::{CatalogError, CatalogStore};

const DOCUMENT: &str = include_str!("../data/quota-pools.json");

fn load_native(doc: &str) -> (Vec<QuotaPoolWire>, Vec<String>) {
    static SEED: OnceLock<serde_json::Value> = OnceLock::new();
    fusiform_curate::quota_pools::load_where(doc, |provider, model| {
        let seed = SEED.get_or_init(|| {
            serde_json::from_str(include_str!("../data/models-dev-seed.json"))
                .expect("the embedded seed parses")
        });
        seed[provider]["models"][model].is_object() || {
            let id = (provider.to_owned(), model.to_owned());
            crate::aliases::aliases().contains_key(&id)
                && !crate::aliases::retired_aliases().contains_key(&id)
        }
    })
}

pub(crate) fn at(
    store: &CatalogStore,
    at: i64,
    provider: Option<&str>,
) -> Result<Vec<QuotaPoolWire>, CatalogError> {
    let matches = |row: &QuotaPoolWire| provider.is_none_or(|id| row.provider_id == id);
    if store.is_store_mode() {
        let Some(doc) = store.store_document_at("quota-pools", at)? else {
            return Ok(Vec::new());
        };
        let (rows, rejects) = fusiform_curate::quota_pools::load_stored_reporting_rejects(&doc);
        report(rejects);
        return Ok(rows.into_iter().filter(matches).collect());
    }

    if let Some(rows) = PLANTED.with_borrow(|rows| {
        rows.as_ref()
            .map(|rows| rows.iter().filter(|row| matches(row)).cloned().collect())
    }) {
        return Ok(rows);
    }
    static ROWS: OnceLock<Vec<QuotaPoolWire>> = OnceLock::new();
    let rows = ROWS.get_or_init(|| {
        let (rows, rejects) = load_native(DOCUMENT);
        report(rejects);
        rows
    });
    Ok(rows.iter().filter(|row| matches(row)).cloned().collect())
}

fn report(rejects: Vec<String>) {
    for reject in rejects {
        eprintln!("fusiform: quota-pools: {reject}");
    }
}

thread_local! {
    static PLANTED: std::cell::RefCell<Option<Vec<QuotaPoolWire>>> = const { std::cell::RefCell::new(None) };
}

#[doc(hidden)]
#[must_use]
pub struct Planted {
    previous: Option<Vec<QuotaPoolWire>>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

/// Use an embedded-document fixture on the calling thread until the guard drops.
#[doc(hidden)]
pub fn plant(doc: &str) -> Planted {
    let (rows, rejects) = load_native(doc);
    assert!(rejects.is_empty(), "{rejects:?}");
    Planted {
        previous: PLANTED.replace(Some(rows)),
        _thread_bound: std::marker::PhantomData,
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        PLANTED.replace(self.previous.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_quota_pools_report_rejects_without_losing_valid_entries() {
        let retired = crate::aliases::retired_aliases()
            .keys()
            .next()
            .map(|(p, m)| format!("{p}/{m}"))
            .unwrap();
        let pool = |id, models: serde_json::Value| {
            json!({"provider_id":"google", "pool_id":id, "models":models,
                "source_ref":"https://example.test/quota", "quote":"Synthetic pool membership",
                "established_at_ms":1000, "review_by_ms":5_184_001_000_i64})
        };
        let valid = json!(["openai/gpt-4o", "google/antigravity-gemini-4-hydrogen"]);
        let mut bad_date = pool("date", valid.clone());
        bad_date["review_by_ms"] = json!(5_184_001_001_i64);
        let mut missing_quote = pool("quote", valid.clone());
        missing_quote["quote"] = json!(" ");
        let doc = json!({"schema":"quota-pools/v1", "entries":[
            pool("Weekly", json!([])),
            pool("unknown", json!(["google/nonexistent-model"])),
            pool("retired", json!([retired])),
            pool("repeat", json!(["openai/gpt-4o", "openai/gpt-4o"])),
            pool("Weekly", valid.clone()),
            pool("weekly", valid.clone()),
            pool("Weekly", valid.clone()),
            bad_date,
            missing_quote,
        ]});
        let (rows, rejects) = load_native(&doc.to_string());
        assert_eq!(
            rows.iter().map(|r| r.pool_id.as_str()).collect::<Vec<_>>(),
            ["Weekly", "weekly"]
        );
        assert_eq!(serde_json::to_value(&rows[0].models).unwrap(), valid);
        assert_eq!(rejects.len(), 7, "{rejects:?}");
        for (reject, expected) in rejects.iter().zip([
            "Weekly: models must be nonempty",
            "unknown: model \"google/nonexistent-model\" is neither",
            "retired: model",
            "repeat: duplicate model",
            "Weekly: duplicate quota pool identity",
            "date: review_by_ms must be 60 days",
            "quote: empty provider, pool id, source or quote",
        ]) {
            assert!(reject.contains(expected), "{reject}");
        }
    }

    #[test]
    fn shipped_quota_pools_load_without_rejects() {
        let (_, rejects) = load_native(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
    }
}
