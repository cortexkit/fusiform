//! The window overlay is well formed, and every cell can actually join.
//!
//! # Why a dataset needs a test at all
//!
//! This file is hand-curated from documents and source trees rather than
//! generated from an upstream, so nothing else checks it. A cell keyed on a
//! model id that does not exist is not a wrong value — it is data that silently
//! never applies, and a consumer merging it sees no error and gets no
//! correction. The failure is indistinguishable from the cell not being there.
//!
//! Joinability is checked against the EMBEDDED SEED rather than the live
//! upstream, so the check runs in CI with no network and no dependence on
//! models.dev being reachable. The seed is a real 6,280-model snapshot, so a
//! typo cannot pass.

use std::collections::BTreeSet;

const OVERLAY: &str = include_str!("../data/window-overlay.json");
const SEED: &str = include_str!("../data/models-dev-seed.json");

/// Every grade the schema allows, strongest first.
const GRADES: &[&str] = &[
    "provider_asserted_runtime",
    "measured",
    "provider_asserted_doc",
    "catalog",
    "unknown",
];

/// Every reason an unknown may carry.
const UNKNOWN_WHY: &[&str] = &[
    "placeholder_output_equals_context",
    "placeholder_zero",
    "never_measured",
    "retracted",
];

/// Every fact key the schema defines.
const FACT_KEYS: &[&str] = &[
    "window.advertised",
    "window.enforced",
    "output.advertised",
    "output.enforced",
    "output.default",
    "geometry",
];

/// Every geometry class.
const GEOMETRIES: &[&str] = &["shared_upfront", "shared_truncating", "separate"];

fn overlay() -> serde_json::Value {
    serde_json::from_str(OVERLAY).expect("the overlay must be valid JSON")
}

#[test]
fn every_cell_joins_a_real_model_or_a_declared_mint() {
    let doc = overlay();
    let seed: serde_json::Value = serde_json::from_str(SEED).unwrap();

    let minted: BTreeSet<&str> = doc["minted_provider_ids"]
        .as_array()
        .expect("minted_provider_ids is required")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();

    let mut checked = 0usize;
    for cell in doc["cells"].as_array().unwrap() {
        let provider = cell["provider_id"].as_str().unwrap();
        let model = cell["model_id"].as_str().unwrap();

        if minted.contains(provider) {
            // A minted id does not resolve upstream by construction — that is
            // what minting means. What must still hold is that the MODEL exists
            // under the provider the mint was derived from, otherwise the mint
            // is covering a typo.
            let base = provider.split('-').next().unwrap();
            let exists = seed[base]["models"].get(model).is_some();
            assert!(
                exists,
                "minted id {provider:?} carries model {model:?}, which does not \
                 exist under the base provider {base:?} either — a mint must \
                 fork a real model, not introduce one"
            );
            checked += 1;
            continue;
        }

        assert!(
            seed[provider].is_object(),
            "cell provider {provider:?} does not exist upstream and is not \
             declared in minted_provider_ids"
        );

        // A model that arrived AFTER the seed snapshot cannot be join-checked
        // against it, and that is not a defect in either the cell or the seed.
        // The upstream publishes new models daily — grok-4.6 first appeared six
        // hours after this seed was fetched — so an exemption is necessary or
        // the dataset can never describe anything new.
        //
        // What the exemption must not become is a way to wave through a typo.
        // So it carries EVIDENCE: how the model's existence was verified, which
        // for a fusiform cell means its own era history, where an arrival has a
        // bounded observation window rather than a bare assertion.
        if let Some(post_seed) = cell.get("post_seed") {
            let evidence = post_seed
                .get("evidence")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            assert!(
                !evidence.is_empty(),
                "{provider}/{model} claims post_seed arrival with no evidence: \
                 an unevidenced exemption is a typo escape hatch"
            );
            assert!(
                post_seed.get("verified_at").is_some(),
                "{provider}/{model}: a post_seed exemption must say WHEN the \
                 existence was verified, or a future reader cannot tell a live \
                 check from a stale one"
            );
            checked += 1;
            continue;
        }

        if model != "*" {
            assert!(
                seed[provider]["models"].get(model).is_some(),
                "cell {provider}/{model} does not exist upstream: the cell can \
                 never join, and a consumer merging it sees no error. If the \
                 model arrived after the seed snapshot, record a post_seed \
                 block with evidence rather than removing this check."
            );
        }
        checked += 1;
    }

    assert!(
        checked >= 7,
        "the overlay must carry the batch it claims; found {checked} cells"
    );
}

#[test]
fn every_fact_is_completely_specified() {
    let doc = overlay();
    for cell in doc["cells"].as_array().unwrap() {
        let id = format!(
            "{}/{}",
            cell["provider_id"].as_str().unwrap(),
            cell["model_id"].as_str().unwrap()
        );
        let facts = cell["facts"].as_object().expect("facts is required");
        assert!(!facts.is_empty(), "{id}: a cell with no facts says nothing");

        for (key, fact) in facts {
            assert!(
                FACT_KEYS.contains(&key.as_str()),
                "{id}: {key:?} is not a fact key this schema defines"
            );

            for required in [
                "value",
                "grade",
                "units",
                "boundary",
                "source_ref",
                "observed_at",
            ] {
                assert!(
                    fact.get(required).is_some(),
                    "{id} {key}: missing required field {required:?}"
                );
            }

            let grade = fact["grade"].as_str().unwrap();
            assert!(
                GRADES.contains(&grade),
                "{id} {key}: grade {grade:?} is outside the vocabulary"
            );

            let units = fact["units"].as_str().unwrap();
            assert!(
                units == "provider" || units == "estimate",
                "{id} {key}: units {units:?} is neither provider nor estimate"
            );

            let boundary = fact["boundary"].as_str().unwrap();
            assert!(
                ["Observed", "Asserted", "Corrected"].contains(&boundary),
                "{id} {key}: boundary {boundary:?} is outside the vocabulary"
            );

            assert!(
                !fact["source_ref"].as_str().unwrap().is_empty(),
                "{id} {key}: source_ref is required and must resolve to something"
            );
        }
    }
}

#[test]
fn a_value_is_one_of_the_three_kinds_and_says_what_it_must() {
    let doc = overlay();
    for cell in doc["cells"].as_array().unwrap() {
        let id = format!(
            "{}/{}",
            cell["provider_id"].as_str().unwrap(),
            cell["model_id"].as_str().unwrap()
        );
        for (key, fact) in cell["facts"].as_object().unwrap() {
            let value = &fact["value"];
            match value["kind"].as_str().expect("every value has a kind") {
                "stated" => {
                    assert!(
                        value.get("value").is_some(),
                        "{id} {key}: a stated value must carry one"
                    );
                    if key == "geometry" {
                        let g = value["value"].as_str().unwrap();
                        assert!(
                            GEOMETRIES.contains(&g),
                            "{id}: geometry {g:?} is outside the vocabulary"
                        );
                    }
                }
                "bracket" => {
                    let at_least = value.get("at_least").and_then(|v| v.as_i64());
                    let below = value.get("below").and_then(|v| v.as_i64());
                    assert!(
                        at_least.is_some() || below.is_some(),
                        "{id} {key}: a bracket with neither bound is an unknown \
                         wearing a bracket's shape"
                    );
                    if let (Some(lo), Some(hi)) = (at_least, below) {
                        assert!(
                            lo < hi,
                            "{id} {key}: bracket is inverted ({lo} >= {hi}) — the \
                             witnesses contradict each other"
                        );
                    }
                }
                "unknown" => {
                    let why = value["why"].as_str().expect("an unknown must say why");
                    assert!(
                        UNKNOWN_WHY.contains(&why),
                        "{id} {key}: unknown reason {why:?} is outside the \
                         vocabulary — a free-text reason cannot be branched on"
                    );
                    assert_eq!(
                        fact["grade"].as_str().unwrap(),
                        "unknown",
                        "{id} {key}: a value nobody has established must carry \
                         grade unknown, or the grade claims evidence the value \
                         denies"
                    );
                }
                other => panic!("{id} {key}: unknown value kind {other:?}"),
            }
        }
    }
}

#[test]
fn a_minted_id_is_declared_and_a_declared_mint_is_used() {
    let doc = overlay();
    let declared: BTreeSet<&str> = doc["minted_provider_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();

    let seed: serde_json::Value = serde_json::from_str(SEED).unwrap();

    // Every declared mint must be absent upstream. A mint that resolves is not
    // a mint, and leaving it declared tells a future audit to expect a miss
    // where there is a hit.
    for id in &declared {
        assert!(
            !seed[*id].is_object(),
            "{id:?} is declared as minted but exists upstream — either the \
             mint is unnecessary or the upstream has adopted the name"
        );
    }

    // And every mint must be used. An unused declaration is a reserved name
    // pretending to be data, which is exactly what mint-on-divergence exists to
    // prevent: the namespace must not accrete speculative ids.
    let used: BTreeSet<&str> = doc["cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["provider_id"].as_str().unwrap())
        .collect();
    for id in &declared {
        assert!(
            used.contains(id),
            "{id:?} is declared as minted but no cell uses it — a mint is only \
             justified by a measured divergence"
        );
    }
}

#[test]
fn a_placeholder_unknown_names_a_row_that_is_genuinely_a_placeholder() {
    // The strongest check in this file: a cell claiming the catalog publishes a
    // placeholder is checked AGAINST the catalog. Writing
    // `placeholder_output_equals_context` for a row whose output does not equal
    // its context is a false accusation, and it would suppress a real value.
    let doc = overlay();
    let seed: serde_json::Value = serde_json::from_str(SEED).unwrap();

    let mut checked = 0usize;
    for cell in doc["cells"].as_array().unwrap() {
        let provider = cell["provider_id"].as_str().unwrap();
        let model = cell["model_id"].as_str().unwrap();
        for (key, fact) in cell["facts"].as_object().unwrap() {
            let why = fact["value"].get("why").and_then(|v| v.as_str());
            if why != Some("placeholder_output_equals_context") {
                continue;
            }
            // For a post-seed model the seed cannot answer, so the values
            // measured at verification time are used instead. They are recorded
            // in the cell rather than re-fetched, so this test needs no network
            // and a future reader sees exactly what was observed.
            let limit = match cell.get("post_seed") {
                Some(ps) => ps["catalog_limit_at_verification"].clone(),
                None => seed[provider]["models"][model]["limit"].clone(),
            };
            let (ctx, out) = (
                limit["context"].as_i64().unwrap_or(-1),
                limit["output"].as_i64().unwrap_or(-1),
            );
            assert!(
                out >= ctx && ctx > 0,
                "{provider}/{model} {key}: claimed output-equals-context \
                 placeholder, but the catalog says context={ctx} output={out}"
            );
            checked += 1;
        }
    }

    assert!(
        checked >= 2,
        "the batch must exercise the placeholder path — the two rows where the \
         catalog is actively harmful are its highest-value cells; found {checked}"
    );
}
