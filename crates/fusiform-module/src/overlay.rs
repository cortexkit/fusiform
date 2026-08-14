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
    load_reporting_rejects().0
}

/// Load, and report cells that were skipped because their SHAPE did not match.
///
/// # Why the two kinds of skip must be separable
///
/// This loop skips a cell for two completely different reasons, and a bare
/// `continue` renders them identically:
///
/// - **Deliberate**: a wildcard cell, or a fact whose value is an `unknown`.
///   Both are correct overlay content that produces no correction, and there
///   are more of them than corrections.
/// - **Shape mismatch**: a missing identity field, or a `stated` value that is
///   not an integer. That is a MALFORMED cell, and skipping it silently means a
///   correction stops applying with nothing to say so.
///
/// The second is the dangerous one and it is invisible by construction. Rename
/// a field in the overlay and every cell skips: `load` returns empty, fusiform
/// serves models.dev's 1,000,000 for `claude-sonnet-4-5` again, the catalog is
/// otherwise perfect, health stays green, and the only symptom is a consumer
/// sending five times the real ceiling into a hard 400.
///
/// SUBC's audit rule, applied to production code rather than to a test: look at
/// the places with NO error path, where the success representation is the only
/// representation available. A `continue` inside a parse loop is exactly that —
/// a rejected cell has nowhere to go but "fewer results", which is the shape a
/// correct run also has.
///
/// The rejects are counted rather than fatal. The overlay is embedded at
/// compile time, so it can only change by someone editing this repository, and
/// a test asserting zero rejects fails before anything ships. A panic here
/// would take the whole catalog down for 6,293 models to protect two.
pub fn load_reporting_rejects() -> (Corrections, Vec<String>) {
    let doc: serde_json::Value =
        serde_json::from_str(OVERLAY).expect("the embedded overlay must parse");
    parse(&doc)
}

/// Parse an overlay document.
///
/// Takes the document rather than reading the embedded constant, so a test can
/// hand it a MALFORMED one. The embedded overlay is well formed by construction
/// — its guards run in CI — which means the reject arms below are unreachable
/// from real data and were therefore untested: a mutation deleting the
/// non-integer reject survived the whole suite.
///
/// That is the same defect as startup logic living in `main.rs` where no
/// integration test can drive it, twice found in this repository. Code that
/// only runs on input the tests cannot supply is code nothing covers, however
/// carefully it is written.
fn parse(doc: &serde_json::Value) -> (Corrections, Vec<String>) {
    let mut out = Corrections::new();
    let mut rejected = Vec::new();

    for (i, cell) in doc["cells"].as_array().into_iter().flatten().enumerate() {
        let (Some(provider_id), Some(model_id)) =
            (cell["provider_id"].as_str(), cell["model_id"].as_str())
        else {
            rejected.push(format!(
                "cell {i} has no provider_id/model_id pair: the identity fields \
                 have been renamed or removed, and EVERY cell will be skipped"
            ));
            continue;
        };
        // A wildcard cell states something about a provider, not about a row a
        // consumer can look up. Nothing to correct, and not a reject.
        if model_id == "*" {
            continue;
        }

        let fact = &cell["facts"]["window.advertised"];
        let value = &fact["value"];
        // Only a stated scalar can replace a published number. An unknown says
        // nobody established the value, which is not grounds to overwrite one.
        // Also not a reject: most cells are unknowns by design.
        if value["kind"].as_str() != Some("stated") {
            continue;
        }
        let Some(served) = value["value"].as_i64() else {
            rejected.push(format!(
                "{provider_id}/{model_id} declares a stated window.advertised \
                 whose value is not an integer ({}), so the correction it exists \
                 to carry will silently not apply",
                value["value"]
            ));
            continue;
        };

        out.insert(
            (
                provider_id.to_string(),
                model_id.to_string(),
                FactKey::limit("context"),
            ),
            Correction {
                // NO `upstream_value` HERE, and its absence is the point.
                //
                // This struct used to carry one, empty, documented as "filled
                // in at apply time from the row actually served". `catalog.get`
                // filled it. Then a new `catalog.history` disclosure read it
                // directly and shipped `"serves 200000 ... not the  recorded
                // below"` to production — one value where the sentence promises
                // two, because a slot is not a value and nothing said so at the
                // point of reading.
                //
                // A reader-side assertion that the slot was filled would have
                // caught it. Removing the slot means there is nowhere to read
                // it from: the upstream value can only come from the row, which
                // is the only place it was ever correct.
                served_value: served.to_string(),
                authority: fact["source_ref"].as_str().unwrap_or_default().to_string(),
            },
        );
    }
    (out, rejected)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cell whose stated value is not an integer is REPORTED, not skipped.
    ///
    /// Driven with a synthetic document because the embedded overlay is well
    /// formed, so this arm is unreachable from real data. A mutation deleting
    /// the reject survived the entire suite before this existed — the arm was
    /// written, correct, and covered by nothing.
    #[test]
    fn a_malformed_stated_value_is_reported_rather_than_dropped() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"cells":[{
                 "provider_id":"anthropic","model_id":"claude-sonnet-4-5",
                 "facts":{"window.advertised":{
                   "value":{"kind":"stated","value":"200000"},
                   "source_ref":"docs"}}}]}"#,
        )
        .unwrap();

        let (corrections, rejected) = parse(&doc);
        assert!(
            corrections.is_empty(),
            "a string value must not become a correction"
        );
        assert_eq!(
            rejected.len(),
            1,
            "the cell must be REPORTED. Silently skipping it means the \
             correction it carries stops applying with nothing to say so, and \
             fusiform serves the upstream's wrong value again."
        );
        assert!(
            rejected[0].contains("claude-sonnet-4-5"),
            "the report must name the cell, or an operator cannot find it: {:?}",
            rejected[0]
        );
    }

    /// A wildcard and an unknown are DELIBERATE skips and must not be reported.
    ///
    /// The other half of the distinction: if every skip were a reject, the list
    /// would be noise on every load and nobody would read the one entry that
    /// matters.
    #[test]
    fn deliberate_skips_are_not_reported_as_rejects() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"cells":[
                 {"provider_id":"openrouter","model_id":"*",
                  "facts":{"window.advertised":{"value":{"kind":"stated","value":1}}}},
                 {"provider_id":"deepseek","model_id":"deepseek-v4",
                  "facts":{"window.advertised":{
                    "value":{"kind":"unknown","why":"never_measured"}}}}]}"#,
        )
        .unwrap();

        let (corrections, rejected) = parse(&doc);
        assert!(corrections.is_empty(), "neither cell yields a correction");
        assert!(
            rejected.is_empty(),
            "a wildcard and an unknown are correct overlay content, not \
             malformed cells: {rejected:?}"
        );
    }

    #[test]
    fn no_cell_is_rejected_for_its_shape() {
        // The guard the reject list exists for.
        //
        // Every correction fusiform serves depends on this loop recognising the
        // overlay's shape, and a shape mismatch renders as ABSENCE — the same
        // thing a correct run produces for a wildcard or an unknown. Rename a
        // field and `load` returns empty, fusiform serves models.dev's 1M for
        // claude-sonnet-4-5 again, and every other surface stays green.
        let (corrections, rejected) = load_reporting_rejects();
        assert!(
            rejected.is_empty(),
            "the overlay contains cells this loader cannot read:\n  {}",
            rejected.join("\n  ")
        );

        // And the control: a loader that rejects nothing because it reads
        // nothing would pass the assertion above. The floor is measured — two
        // corrective cells today, both anthropic sonnet-4.5 ids — and stated as
        // a floor so adding one does not fail a test about parsing.
        assert!(
            corrections.len() >= 2,
            "the loader produced {} corrections and there are at least two \
             (claude-sonnet-4-5 and its dated id). Rejecting nothing while \
             producing nothing is what a loader looks like when it has gone \
             blind to the document's shape.",
            corrections.len()
        );
    }

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
