//! Every curated creator must be able to produce a price.
//!
//! # The check this makes permanent
//!
//! Nine families were seeded into `model-creators.json` and two were removed
//! before shipping, by a check run once, by hand, in a throwaway script:
//! `deepseek` prices only its `-flash` and `-thinking` lines and publishes no
//! bare `deepseek` family row, and at the time `google` published `gemma`
//! unpriced while pricing every closed Gemini family.
//!
//! Both rows would have shipped looking like coverage and inheriting nothing.
//! THE ORIGINATOR OF AN OPEN-WEIGHT MODEL IS NOT OBLIGED TO SELL IT, so
//! `creator` and `has a list price` are independent facts, and a table built on
//! the assumption that they coincide ships rows that silently never fire.
//!
//! A check that runs once protects the rows present the day it ran. This makes
//! it protect the next one somebody adds.
//!
//! # Why the SEED is the subject, and what that costs
//!
//! This resolves "does the creator publish a priced row in this family" against
//! the embedded seed, which is a snapshot. The live store is current and the
//! seed ages, so the two can disagree — a creator that stops pricing a family
//! leaves this fence passing on stale evidence. That is not hypothetical:
//! `gemma -> google` was removed when google published gemma unpriced; Google
//! now prices both seed rows. A creator can change that in either direction.
//!
//! The seed is used anyway because CI has no store, and a test that skips when
//! a live store is absent reports green while doing nothing — which this
//! repository refuses. So the limit is declared rather than papered over: this
//! fence answers for the seed's vintage, and the complement is an operator
//! check against the store.
//!
//! Run after a seed refresh, when the fence's subject moves:
//!
//! ```text
//! for each curated (family, creator):
//!   SELECT COUNT(DISTINCT model_id) FROM era
//!    WHERE provider_id = <creator> AND fact_key = 'rate.input'
//!      AND EXISTS (model.family = <family> for that model)
//! ```
//!
//! A row alive in the seed and zero in the store is a mapping that has died
//! since the snapshot. Measured 2026-09-15 across all seven rows: seed and
//! store agree on every one, the largest gap being deepseek-flash at 1 in the
//! seed and 3 in the store — the catalog growing, not the mapping dying.
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

const SEED: &str = include_str!("../../data/models-dev-seed.json");
const CREATORS: &str = include_str!("../../data/model-creators.json");

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
fn priced_rows_per_curated_family(
    seed: &serde_json::Value,
    doc: Doc,
) -> BTreeMap<String, (String, usize)> {
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
                                && m.pointer("/cost/text/input/price")
                                    .is_some_and(serde_json::Value::is_number)
                        })
                        .count()
                })
                .unwrap_or(0);
            (row.family, (row.creator_provider_id, n))
        })
        .collect()
}

fn dead_creators(table: &BTreeMap<String, (String, usize)>) -> Vec<String> {
    table
        .iter()
        .filter(|(_, (_, n))| *n == 0)
        .map(|(family, (creator, _))| format!("{family} -> {creator}"))
        .collect()
}

#[test]
fn every_curated_creator_publishes_a_price_in_the_family_it_claims() {
    let seed: serde_json::Value = serde_json::from_str(SEED).expect("the seed parses");
    let doc: Doc = serde_json::from_str(CREATORS).expect("the creator table parses");
    let table = priced_rows_per_curated_family(&seed, doc);

    assert!(
        !table.is_empty(),
        "an empty table would make the loop below assert nothing — the shipped \
         file must carry rows"
    );

    let dead = dead_creators(&table);

    assert!(
        dead.is_empty(),
        "these curated rows can never produce a price, because the named \
         creator publishes no PRICED row in that family at all — they are \
         coverage that inherits nothing: {dead:?}. Either the creator is wrong, \
         or that originator does not sell its own weights (which is ordinary: \
         an originator might not price a family) and the row should be removed rather \
         than kept."
    );
}

/// The fence must convict an unpriced family while accepting a priced one.
///
/// Without this, the test above passes identically against a build whose
/// counter always returns a positive number — and a fence that cannot fail is
/// the shape this repository keeps finding.
#[test]
fn the_fence_convicts_a_creator_that_does_not_sell_its_own_weights() {
    let seed = serde_json::json!({"creator":{"models":{
        "unpriced":{"family":"unpriced"},
        "priced":{"family":"priced","cost":{"input":1}}
    }}});
    let doc: Doc = serde_json::from_value(serde_json::json!({"creators":[
        {"family":"unpriced","creator_provider_id":"creator"},
        {"family":"priced","creator_provider_id":"creator"}
    ]}))
    .unwrap();
    let table = priced_rows_per_curated_family(&seed, doc);
    assert_eq!(table["priced"].1, 1);
    assert_eq!(dead_creators(&table), vec!["unpriced -> creator"]);
}
