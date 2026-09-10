//! Which provider published an open-weight family, so a rate can be inherited.
//!
//! # Why this exists
//!
//! A reseller that serves open weights often publishes no price: models.dev
//! keeps the list price on the originator's provider entry. A consumer reading
//! an absent rate as maximum cost ranks those models last, which is how ALF's
//! router lost every ollama-cloud selection to a paid model.
//!
//! # Why it is a table rather than a derivation
//!
//! Upstream publishes `family` — identical across every provider serving the
//! same weights — and `open_weights`, which together relate the rows and mark
//! the population. It publishes nothing naming the ORIGINATOR.
//!
//! Measured 2026-09-06: `release_date` is the MODEL's release date and is
//! byte-identical across providers, so "whoever listed it first" cannot
//! distinguish a creator from a reseller. There is no other candidate signal.
//! So each row is a human claim with a named basis, and the value it produces
//! is disclosed on the wire rather than blended into the published rates.

use std::collections::HashMap;

const CREATORS: &str = include_str!("../data/model-creators.json");

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
pub fn load_reporting_rejects() -> (Creators, Vec<String>) {
    let doc: CreatorDoc = match serde_json::from_str(CREATORS) {
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

/// Load the table, discarding the reject report.
pub fn load() -> Creators {
    load_reporting_rejects().0
}

/// The table, parsed once.
///
/// Per process rather than per request: the file is embedded, so a re-parse on
/// every catalog read would cost the same answer repeatedly.
pub fn creators() -> &'static Creators {
    static PARSED: std::sync::OnceLock<Creators> = std::sync::OnceLock::new();
    PARSED.get_or_init(load)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_table_loads_without_rejects() {
        let (creators, rejects) = load_reporting_rejects();
        assert!(
            rejects.is_empty(),
            "the shipped creator table must parse cleanly: {rejects:?}"
        );
        assert!(
            !creators.is_empty(),
            "an empty table would make every assertion below vacuous"
        );
        assert_eq!(
            creators.get("glm").map(String::as_str),
            Some("zai"),
            "the family ALF's routing defect turns on must resolve"
        );
    }

    #[test]
    fn a_duplicate_family_is_reported_rather_than_resolved() {
        // Drives the parser with a synthetic document, because the shipped one
        // is clean and a test that only reads it cannot tell a working reject
        // path from an absent one.
        let doc: CreatorDoc = serde_json::from_str(
            r#"{"creators":[
                {"family":"glm","creator_provider_id":"zai"},
                {"family":"glm","creator_provider_id":"someone-else"}
            ]}"#,
        )
        .expect("the fixture parses");
        let mut out = Creators::new();
        let mut rejects = Vec::new();
        for row in doc.creators {
            if let Some(existing) = out.insert(row.family.clone(), row.creator_provider_id.clone())
            {
                rejects.push(format!("{existing} vs {}", row.creator_provider_id));
            }
        }
        assert_eq!(
            rejects.len(),
            1,
            "a second row for one family must be reported, since resolving it \
             silently would make the served price depend on file order"
        );
    }
}
