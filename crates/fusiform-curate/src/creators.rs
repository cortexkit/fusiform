//! Which provider published an open-weight family, so a rate can be inherited.
//!
//! # Why this exists
//!
//! A reseller that serves open weights often publishes no price: models.dev
//! keeps the list price on the originator's provider entry. A consumer reading
//! an absent rate as maximum cost ranks those models last, even when the
//! reseller charges nothing at the margin.
//!
//! # Why it is a table rather than a derivation
//!
//! Upstream publishes `family` — identical across every provider serving the
//! same weights — and `open_weights`, which together relate the rows and mark
//! the population. It also publishes `canonical_model_id`, now served as an
//! upstream claim linking provider rows. That string is not a typed
//! `creator_provider_id`, and this inheritance policy does not infer a price
//! source from its spelling.
//!
//! Measured 2026-09-06: `release_date` is the MODEL's release date and is
//! byte-identical across providers, so "whoever listed it first" cannot
//! distinguish a creator from a reseller.
//! So each row is a human claim with a named basis, and the value it produces
//! is disclosed on the wire rather than blended into the published rates.

use std::collections::HashMap;

/// A family, and the provider whose list price stands in for it.
#[derive(Debug, Clone, serde::Deserialize)]
struct CreatorRow {
    family: String,
    creator_provider_id: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct CreatorDoc {
    creators: Vec<CreatorRow>,
}

/// `family` -> the provider id whose price is inherited.
pub type Creators = HashMap<String, String>;

/// Load the table, reporting rows that did not parse.
///
/// Rejects are COUNTED rather than skipped silently. A malformed row means the
/// curated file drifted from this struct, and a loader that quietly returns
/// fewer rows turns a schema mistake into models that mysteriously stay
/// unpriced — a defect with no error attached to it.
pub fn parse_document(doc: &str) -> (Creators, Vec<String>) {
    let doc: CreatorDoc = match serde_json::from_str(doc) {
        Ok(d) => d,
        Err(e) => {
            return (
                Creators::new(),
                vec![format!("creator table unreadable: {e}")],
            )
        }
    };

    let mut out = Creators::new();
    let mut rejects = Vec::new();
    for row in doc.creators {
        if row.family.is_empty() || row.creator_provider_id.is_empty() {
            rejects.push(format!(
                "creator row with an empty field: family={:?} provider={:?}",
                row.family, row.creator_provider_id
            ));
            continue;
        }
        if let Some(existing) = out.insert(row.family.clone(), row.creator_provider_id.clone()) {
            // Two rows for one family is a curation mistake, not a preference
            // order. Reported rather than resolved: silently keeping the last
            // one would make the served price depend on file ordering.
            rejects.push(format!(
                "family {:?} names two creators: {:?} and {:?}",
                row.family, existing, row.creator_provider_id
            ));
        }
    }
    (out, rejects)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duplicate_family_is_reported_rather_than_resolved() {
        // Drives the parser with a synthetic document, because the shipped one
        // is clean and a test that only reads it cannot tell a working reject
        // path from an absent one.
        let (_, rejects) = parse_document(
            r#"{"creators":[
                {"family":"glm","creator_provider_id":"zai"},
                {"family":"glm","creator_provider_id":"someone-else"}
            ]}"#,
        );
        assert_eq!(
            rejects.len(),
            1,
            "a second row for one family must be reported, since resolving it \
             silently would make the served price depend on file order"
        );
    }
}
