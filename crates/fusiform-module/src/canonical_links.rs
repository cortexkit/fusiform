//! Sourced cross-provider identities for upstream rows without a lab identity.
//!
//! These claims are applied only while serving the current catalog. They never
//! replace an upstream identity or become observations in the store.

use std::collections::BTreeMap;

const DOCUMENT: &str = include_str!("../data/canonical-links.json");
const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalLink {
    pub provider_id: String,
    pub model_id: String,
    pub canonical_id: String,
    pub source_ref: String,
    pub quote: String,
    pub lab_source_ref: String,
    pub lab_quote: String,
    pub established_by: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

pub type CanonicalLinks = BTreeMap<(String, String), CanonicalLink>;

fn valid_canonical_id(id: &str) -> bool {
    let Some((lab, model)) = id.split_once('/') else {
        return false;
    };
    !lab.is_empty()
        && !model.is_empty()
        && !model.contains('/')
        && !id.chars().any(char::is_whitespace)
}

/// Reject malformed claims and duplicate keys rather than silently choosing one.
pub fn load_reporting_rejects(doc: &str) -> (CanonicalLinks, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Doc {
        links: Vec<serde_json::Value>,
    }
    let doc: Doc = match serde_json::from_str(doc) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                BTreeMap::new(),
                vec![format!("canonical links unreadable: {error}")],
            )
        }
    };
    let mut rows = CanonicalLinks::new();
    let mut rejects = Vec::new();
    for raw in doc.links {
        let row: CanonicalLink = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!("canonical link did not parse: {error}"));
                continue;
            }
        };
        let key = (row.provider_id.clone(), row.model_id.clone());
        let invalid = if [
            &row.provider_id,
            &row.model_id,
            &row.source_ref,
            &row.quote,
            &row.lab_source_ref,
            &row.lab_quote,
        ]
        .iter()
        .any(|field| field.trim().is_empty())
        {
            Some("empty identity, source or quote")
        } else if !valid_canonical_id(&row.canonical_id) {
            Some("canonical_id must be <lab>/<id> without whitespace")
        } else if row.established_by != "fusiform" {
            Some("established_by must be fusiform")
        } else if row.established_at_ms <= 0
            || row.review_by_ms.checked_sub(row.established_at_ms) != Some(REVIEW_INTERVAL_MS)
        {
            Some("review_by_ms must be 60 days after established_at_ms")
        } else if rows.contains_key(&key) {
            Some("duplicate canonical link identity")
        } else {
            None
        };
        if let Some(reason) = invalid {
            rejects.push(format!("canonical link {}/{}: {reason}", key.0, key.1));
        } else {
            rows.insert(key, row);
        }
    }
    (rows, rejects)
}

pub fn links() -> &'static CanonicalLinks {
    if let Some(rows) = PLANTED.with(|planted| planted.get()) {
        return rows;
    }
    static ROWS: std::sync::OnceLock<CanonicalLinks> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

thread_local! {
    static PLANTED: std::cell::Cell<Option<&'static CanonicalLinks>> = const { std::cell::Cell::new(None) };
}

#[doc(hidden)]
#[must_use]
pub struct Planted {
    previous: Option<&'static CanonicalLinks>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

/// Use fixture links on the calling thread until the returned guard is dropped.
/// Normal serving uses the embedded file; only tests call this fixture seam.
#[doc(hidden)]
pub fn plant(doc: &str) -> Planted {
    let (rows, rejects) = load_reporting_rejects(doc);
    assert!(rejects.is_empty(), "{rejects:?}");
    let rows = Box::leak(Box::new(rows));
    Planted {
        previous: PLANTED.with(|planted| planted.replace(Some(rows))),
        _thread_bound: std::marker::PhantomData,
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        PLANTED.with(|planted| planted.set(self.previous));
    }
}

pub fn upstream_collisions(rows: &CanonicalLinks, seed: &serde_json::Value) -> Vec<String> {
    rows.keys()
        .filter(|(provider, model)| {
            seed[provider]["models"][model].get("canonical_model_id").is_some_and(|id| !id.is_null())
        })
        .map(|(provider, model)| format!("{provider}/{model}: upstream now publishes canonical_model_id; remove the canonical link"))
        .collect()
}

pub fn missing_seed_rows(rows: &CanonicalLinks, seed: &serde_json::Value) -> Vec<String> {
    rows.keys()
        .filter(|(provider, model)| seed[provider]["models"].get(model).is_none())
        .map(|(provider, model)| format!("{provider}/{model}: linked row is missing from the embedded seed; correct or remove the canonical link"))
        .collect()
}

pub fn overdue_at(rows: &CanonicalLinks, now_ms: i64) -> Vec<String> {
    rows.values().filter(|row| row.review_by_ms < now_ms)
        .map(|row| format!("{}/{}: re-verify {} and re-date established_at_ms and review_by_ms (60 days later), or remove the canonical link", row.provider_id, row.model_id, row.source_ref))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture_document() -> serde_json::Value {
        json!({"links": [{
            "provider_id": "provider",
            "model_id": "model",
            "canonical_id": "lab/model",
            "source_ref": "https://example.org/provider/model",
            "quote": "The provider serves Lab Model.",
            "lab_source_ref": "https://example.org/lab/model",
            "lab_quote": "Lab Model is our model.",
            "established_by": "fusiform",
            "established_at_ms": 1,
            "review_by_ms": 1 + REVIEW_INTERVAL_MS
        }]})
    }

    fn fixture_links() -> CanonicalLinks {
        let (rows, rejects) = load_reporting_rejects(&fixture_document().to_string());
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.len(), 1);
        rows
    }

    fn fixture_seed() -> serde_json::Value {
        json!({"provider": {"models": {"model": {}}}})
    }

    fn seed() -> serde_json::Value {
        serde_json::from_str(include_str!("../data/models-dev-seed.json")).unwrap()
    }

    #[test]
    fn shipped_links_load_without_rejects() {
        let (_, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
    }

    #[test]
    fn empty_document_loads_without_rejects() {
        let (rows, rejects) = load_reporting_rejects(r#"{"links": []}"#);
        assert!(rows.is_empty());
        assert!(rejects.is_empty(), "{rejects:?}");
    }

    #[test]
    fn planted_links_restore_the_previous_table_on_drop() {
        let shipped = links();
        {
            let _links = plant(&fixture_document().to_string());
            assert_eq!(links().len(), 1);
            {
                let _empty = plant(r#"{"links": []}"#);
                assert!(links().is_empty());
            }
            assert_eq!(links().len(), 1);
        }
        assert!(std::ptr::eq(links(), shipped));
    }

    #[test]
    fn upstream_canonical_ids_require_removing_the_link() {
        let collisions = upstream_collisions(links(), &seed());
        assert!(collisions.is_empty(), "{}", collisions.join("\n"));
    }

    #[test]
    fn planted_upstream_identity_triggers_the_removal_fence() {
        let rows = fixture_links();
        let mut seed = fixture_seed();
        let row = rows.values().next().unwrap();
        assert!(upstream_collisions(&rows, &seed).is_empty());
        seed[&row.provider_id]["models"][&row.model_id]["canonical_model_id"] =
            json!("lab/published");
        let collisions = upstream_collisions(&rows, &seed);
        assert_eq!(collisions.len(), 1);
        assert!(collisions[0].contains(&format!("{}/{}", row.provider_id, row.model_id)));
        assert!(collisions[0].contains("remove the canonical link"));
    }

    #[test]
    fn linked_rows_exist_in_the_embedded_seed() {
        let missing = missing_seed_rows(links(), &seed());
        assert!(missing.is_empty(), "{}", missing.join("\n"));
    }

    #[test]
    fn planted_missing_seed_row_names_the_bad_link() {
        let rows = fixture_links();
        let mut seed = fixture_seed();
        let row = rows.values().next().unwrap();
        assert!(missing_seed_rows(&rows, &seed).is_empty());
        seed[&row.provider_id]["models"]
            .as_object_mut()
            .unwrap()
            .remove(&row.model_id);
        let missing = missing_seed_rows(&rows, &seed);
        assert_eq!(missing.len(), 1);
        assert!(missing[0].contains(&format!("{}/{}", row.provider_id, row.model_id)));
        assert!(missing[0].contains("correct or remove"));
    }

    #[test]
    fn canonical_links_are_within_their_review_dates() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let overdue = overdue_at(links(), now);
        assert!(overdue.is_empty(), "{}", overdue.join("\n"));
    }

    #[test]
    fn planted_overdue_link_requires_reverification_or_removal() {
        let rows = fixture_links();
        let row = rows.values().next().unwrap();
        assert!(overdue_at(&rows, row.review_by_ms).is_empty());
        let overdue = overdue_at(&rows, row.review_by_ms + 1);
        assert_eq!(overdue.len(), 1);
        assert!(overdue[0].contains("re-verify"));
        assert!(overdue[0].contains("or remove"));
    }

    #[test]
    fn malformed_and_duplicate_rows_are_counted_as_rejects() {
        let mut doc = fixture_document();
        let row = doc["links"][0].clone();
        doc["links"]
            .as_array_mut()
            .unwrap()
            .extend([row, json!({})]);
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rows.len(), 1);
        assert_eq!(rejects.len(), 2);
        assert!(rejects[0].contains("duplicate"));
        assert!(rejects[1].contains("did not parse"));
        assert_eq!(load_reporting_rejects("not json").1.len(), 1);
    }

    #[test]
    fn malformed_fields_are_rejected() {
        for (field, value) in [
            ("canonical_id", json!("unqualified")),
            ("canonical_id", json!("/model")),
            ("canonical_id", json!("lab/")),
            ("canonical_id", json!("lab/model/extra")),
            ("canonical_id", json!("lab/model name")),
            ("provider_id", json!(" ")),
            ("model_id", json!("")),
            ("source_ref", json!("")),
            ("quote", json!("")),
            ("lab_source_ref", json!("")),
            ("lab_quote", json!("")),
            ("established_by", json!("upstream")),
            ("established_at_ms", json!(0)),
            ("review_by_ms", json!(1)),
            ("unknown", json!(true)),
        ] {
            let mut row = fixture_document()["links"][0].clone();
            row[field] = value;
            let (rows, rejects) = load_reporting_rejects(&json!({"links": [row]}).to_string());
            assert!(rows.is_empty(), "{field}");
            assert_eq!(rejects.len(), 1, "{field}");
        }
    }
}
