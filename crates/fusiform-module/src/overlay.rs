//! Corrections fusiform applies to what the upstream published.
//!
//! # Why this is a serve-side layer and not an ingest one
//!
//! models.dev publishes `context = 1000000` for `claude-sonnet-4-5`, and
//! Anthropic's own documentation names that exact model as its example of the
//! 200k class. Fusiform serves 200k, because a consumer trusting 1M sends five
//! times the real ceiling into a hard 400 — the Anthropic wall is prompt-only,
//! so nothing truncates.
//!
//! The correction lives here rather than in the normalizer for the reason
//! recorded in `docs/design/window-overlay.md` §11: ingest diffs normalized
//! facts against stored eras, so changing a value at that layer writes an era
//! asserting the UPSTREAM changed. It did not. A wrong value in a served row is
//! a wrong value; a wrong era is a falsified observation.
//!
//! # What is corrected, and what deliberately is not
//!
//! Only `limit.context`, from the overlay's `window.advertised`. It is the one
//! field where the catalog and the overlay provably ask the same question.
//!
//! `limit.output` is NOT corrected even where the overlay has a stated value,
//! because models.dev's `output` answers a different question per row: for
//! `moonshotai/kimi-k3` it carries the default you get without asking (131,072),
//! for `google/gemini-3.5-flash` a per-model cap (65,536). Replacing one with
//! the other is a substitution, not a correction.

use std::collections::BTreeMap;

use fusiform_store::FactKey;

/// The overlay, embedded so a correction needs no file at runtime.
///
/// Embedded rather than read from disk because a correction that silently stops
/// applying is the failure mode this layer exists to prevent: a missing file
/// would serve the upstream's wrong value with nothing to say why.
const OVERLAY: &str = include_str!("../data/window-overlay.json");

/// One correction, ready to serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    /// What models.dev published.
    pub upstream_value: String,
    /// What fusiform serves instead.
    pub served_value: String,
    /// WHO says so — the authority, not the correction event.
    ///
    /// BROCA's requirement, and it is right: a divergence line inviting "says
    /// who" that cannot answer from the line makes distrusting the correction
    /// the operator's cheapest move.
    pub authority: String,
}

/// Corrections keyed by the fact they replace.
pub type Corrections = BTreeMap<(String, String, FactKey), Correction>;

/// The corrections fusiform serves, parsed once.
///
/// A process-lifetime constant rather than a parameter threaded through the
/// serve path: the overlay is embedded in the binary, so every request in this
/// process resolves the same set, and passing it around would suggest a caller
/// could vary it.
pub fn corrections() -> &'static Corrections {
    static PARSED: std::sync::OnceLock<Corrections> = std::sync::OnceLock::new();
    PARSED.get_or_init(load)
}

/// Load the corrections fusiform serves.
///
/// Parsed once at startup rather than per request. A malformed overlay is a
/// build-time failure in the tests, so this cannot fail at runtime in a way an
/// operator would have to diagnose.
pub fn load() -> Corrections {
    let doc: serde_json::Value =
        serde_json::from_str(OVERLAY).expect("the embedded overlay must parse");
    let mut out = Corrections::new();

    for cell in doc["cells"].as_array().into_iter().flatten() {
        let (Some(provider_id), Some(model_id)) =
            (cell["provider_id"].as_str(), cell["model_id"].as_str())
        else {
            continue;
        };
        // A wildcard cell states something about a provider, not about a row a
        // consumer can look up. Nothing to correct.
        if model_id == "*" {
            continue;
        }

        let fact = &cell["facts"]["window.advertised"];
        let value = &fact["value"];
        // Only a stated scalar can replace a published number. An unknown says
        // nobody established the value, which is not grounds to overwrite one.
        if value["kind"].as_str() != Some("stated") {
            continue;
        }
        let Some(served) = value["value"].as_i64() else {
            continue;
        };

        out.insert(
            (
                provider_id.to_string(),
                model_id.to_string(),
                FactKey::limit("context"),
            ),
            Correction {
                // Filled in at apply time from the row actually served, so the
                // line reports what THIS poll published rather than what the
                // overlay's author saw.
                upstream_value: String::new(),
                served_value: served.to_string(),
                authority: fact["source_ref"].as_str().unwrap_or_default().to_string(),
            },
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_known_corrections_load() {
        let c = load();
        let key = (
            "anthropic".to_string(),
            "claude-sonnet-4-5".to_string(),
            FactKey::limit("context"),
        );
        let found = c.get(&key).expect("sonnet-4.5 must carry a correction");
        assert_eq!(found.served_value, "200000");
        assert!(
            found.authority.contains("docs.claude.com"),
            "the authority must name a source a reader can go to, got {:?}",
            found.authority
        );
    }

    #[test]
    fn a_wildcard_cell_corrects_nothing() {
        // openrouter/* states that its geometry key holds no single fact. That
        // is a claim about a key, not a replacement for a published row, and
        // treating it as one would have fusiform overwriting 348 models from a
        // cell that deliberately asserts nothing measurable.
        let c = load();
        assert!(
            !c.keys().any(|(_, m, _)| m == "*"),
            "a wildcard cell must not produce a correction"
        );
    }

    #[test]
    fn only_the_context_limit_is_corrected() {
        // kimi-k3 has a stated output.advertised of 1,048,576 against a
        // published 131,072, and it must NOT appear here: the catalog's output
        // field carries the DEFAULT for that row, so replacing it swaps one
        // true statement for another about a different question.
        let c = load();
        assert!(
            c.keys().all(|(_, _, f)| f.as_str() == "limit.context"),
            "only limit.context is correctable; found {:?}",
            c.keys().map(|(_, _, f)| f.as_str()).collect::<Vec<_>>()
        );
        assert!(
            !c.contains_key(&(
                "moonshotai".to_string(),
                "kimi-k3".to_string(),
                FactKey::limit("output")
            )),
            "kimi-k3's output must not be corrected"
        );
    }
}
