//! Every curated creator must be able to produce a price.
//!
//! # The check this makes permanent
//!
//! Nine families were seeded into `model-creators.json` and two were removed
//! before shipping, by a check run once, by hand, in a throwaway script:
//! `deepseek` prices only its `-flash` and `-thinking` lines and publishes no
//! bare `deepseek` family row, and `google` publishes `gemma` UNPRICED while
//! pricing every closed Gemini family.
//!
//! Both rows would have shipped looking like coverage and inheriting nothing.
//! THE ORIGINATOR OF AN OPEN-WEIGHT MODEL IS NOT OBLIGED TO SELL IT, so
//! `creator` and `has a list price` are independent facts, and a table built on
//! the assumption that they coincide ships rows that silently never fire.
//!
//! A check that runs once protects the rows present the day it ran. This makes
//! it protect the next one somebody adds.
//!
//! # What this deliberately does NOT assert
//!
//! That a row fires TODAY. Measured against the live store on 2026-09-13, three
//! shipping rows — `mistral-large`, `nemotron`, `qwen` — resolved zero models,
//! because inheritance matches on the exact model id and resellers carry
//! suffixed ids (`deepseek-v4-flash:0731`, `nemotron-3-nano:30b`) that the
//! originator does not publish.
//!
//! Those rows are correct and stay. "Can this mapping ever produce a price" is
//! a property of the TABLE; "does it produce one right now" is a property of
//! today's catalog, and deleting a true mapping because the current data does
//! not exercise it would break the row the moment a reseller lists a matching
//! id. Only the first property belongs in a fence.

use std::collections::BTreeMap;

const SEED: &str = include_str!("../data/models-dev-seed.json");
const CREATORS: &str = include_str!("../data/model-creators.json");

#[derive(serde::Deserialize)]
struct Row {
    family: String,
    creator_provider_id: String,
}

#[derive(serde::Deserialize)]
struct Doc {
    creators: Vec<Row>,
}

/// `family` -> how many priced rows the named creator publishes in it.
fn priced_rows_per_curated_family() -> BTreeMap<String, (String, usize)> {
    let seed: serde_json::Value = serde_json::from_str(SEED).expect("the seed parses");
    let doc: Doc = serde_json::from_str(CREATORS).expect("the creator table parses");

    doc.creators
        .into_iter()
        .map(|row| {
            let n = seed
                .get(&row.creator_provider_id)
                .and_then(|p| p.get("models"))
                .and_then(|m| m.as_object())
                .map(|models| {
                    models
                        .values()
                        .filter(|m| {
                            m.get("family").and_then(|f| f.as_str()) == Some(row.family.as_str())
                                && m.get("cost")
                                    .and_then(|c| c.get("input"))
                                    .is_some_and(|v| !v.is_null())
                        })
                        .count()
                })
                .unwrap_or(0);
            (row.family, (row.creator_provider_id, n))
        })
        .collect()
}

#[test]
fn every_curated_creator_publishes_a_price_in_the_family_it_claims() {
    let table = priced_rows_per_curated_family();

    assert!(
        !table.is_empty(),
        "an empty table would make the loop below assert nothing — the shipped \
         file must carry rows"
    );

    let dead: Vec<String> = table
        .iter()
        .filter(|(_, (_, n))| *n == 0)
        .map(|(family, (creator, _))| format!("{family} -> {creator}"))
        .collect();

    assert!(
        dead.is_empty(),
        "these curated rows can never produce a price, because the named \
         creator publishes no PRICED row in that family at all — they are \
         coverage that inherits nothing: {dead:?}. Either the creator is wrong, \
         or that originator does not sell its own weights (which is ordinary: \
         google publishes gemma unpriced) and the row should be removed rather \
         than kept."
    );
}

/// The fence must be able to fail, proven against a creator known to be dead.
///
/// Without this, the test above passes identically against a build whose
/// counter always returns a positive number — and a fence that cannot fail is
/// the shape this repository keeps finding.
#[test]
fn the_fence_convicts_a_creator_that_does_not_sell_its_own_weights() {
    let seed: serde_json::Value = serde_json::from_str(SEED).expect("the seed parses");

    // `google` publishes `gemma` and prices none of it. This is the exact row
    // removed before shipping, held here as the fence's control rather than as
    // a claim about what the table contains.
    let priced = seed
        .get("google")
        .and_then(|p| p.get("models"))
        .and_then(|m| m.as_object())
        .map(|models| {
            models
                .values()
                .filter(|m| {
                    m.get("family").and_then(|f| f.as_str()) == Some("gemma")
                        && m.get("cost")
                            .and_then(|c| c.get("input"))
                            .is_some_and(|v| !v.is_null())
                })
                .count()
        })
        .expect("google must be in the seed");

    assert_eq!(
        priced, 0,
        "control: google must still publish gemma unpriced. If this changes, \
         the fence above has lost the example that proves it can fail, and \
         `gemma -> google` becomes a row worth adding rather than one worth \
         refusing"
    );
}
