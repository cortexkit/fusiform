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

/// The overlay's path is a CROSS-REPO CONTRACT, not an implementation detail.
///
/// Fusiform does not serve this dataset yet — no route reads it, and that is
/// deliberate: MC merges the file plugin-side today, and when fusiform serves
/// it over subc the cell shape does not change, only the transport.
///
/// Which means the DELIVERY MECHANISM IS THIS PATH. A consumer in another
/// repository reads these bytes from here, and until today that fact lived only
/// in a chat message — reachable by no check on either side.
///
/// `include_str!` already fails to compile if the file vanishes, but that is an
/// accidental fence and it does not fire on the case that matters: moving the
/// file and updating this path in the same commit leaves the suite green and
/// breaks the consumer silently. The constant below exists so the path is a
/// stated contract a reader must decide to change, rather than a string they
/// can follow while refactoring.
const OVERLAY_PATH: &str = "crates/fusiform-module/data/window-overlay.json";

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
    // Added 2026-08-13, ruled by SUBC and MC. The first four describe evidence
    // states at a valid key; this one says the KEY ITSELF cannot hold a single
    // fact, so measurement samples a routing decision rather than settling
    // anything. `never_measured` would be an instruction to go and measure,
    // aimed at the catalog's largest provider.
    "not_single_valued_at_key",
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
fn the_overlay_is_where_the_consumer_expects_it() {
    // Resolved from the workspace root rather than from this crate, because the
    // consumer's path is repository-relative — they check out fusiform and read
    // that path. Checking `../data/x.json` from here would pass after a move
    // that breaks them.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/<name> is two levels below the workspace root");
    let at = root.join(OVERLAY_PATH);
    assert!(
        at.is_file(),
        "{OVERLAY_PATH} is the path a consumer in another repository reads. \
         Moving it is a breaking change for them and nothing in their build \
         will say so — they will read a stale vendored copy or fail to find it. \
         If the move is intended, tell the consumer before changing this \
         constant."
    );

    // And the bytes at that path must be the bytes under test, or this file is
    // validating something the consumer does not read.
    let on_disk = std::fs::read_to_string(&at).expect("the contract path must be readable");
    assert_eq!(
        on_disk, OVERLAY,
        "the file at the contract path differs from the one this test compiled \
         in: the guards above are checking a different document than the \
         consumer receives"
    );
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

                    // `grade` describes the evidence for THE CELL'S ASSERTION,
                    // and an unknown's assertion depends on its reason.
                    //
                    // For the evidence-absence reasons the assertion is "nobody
                    // has established this", and there is nothing to grade, so
                    // the grade must be unknown or it claims evidence the value
                    // denies.
                    //
                    // `not_single_valued_at_key` asserts something else
                    // entirely: that the KEY cannot hold one fact. That is a
                    // positive claim resting on evidence — for OpenRouter, 348
                    // models spanning other providers' catalogs plus the
                    // consumer's confirmation of the routing behaviour — so it
                    // carries a real grade and a real source_ref.
                    //
                    // This guard originally required grade == unknown for every
                    // unknown, which was right for the four reasons that existed
                    // when it was written and refused the fifth. It is scoped
                    // rather than removed: the original force still applies
                    // where the original premise holds.
                    let evidence_absence = matches!(
                        why,
                        "never_measured"
                            | "placeholder_output_equals_context"
                            | "placeholder_zero"
                            | "retracted"
                    );
                    if evidence_absence {
                        assert_eq!(
                            fact["grade"].as_str().unwrap(),
                            "unknown",
                            "{id} {key}: reason {why:?} asserts that nobody has \
                             established the value, so the grade must be unknown \
                             or it claims evidence the value denies"
                        );
                    } else {
                        assert_ne!(
                            fact["grade"].as_str().unwrap(),
                            "unknown",
                            "{id} {key}: reason {why:?} is a positive claim about \
                             the key, so it must carry the grade of the evidence \
                             behind that claim. Grading it unknown says nobody \
                             established it, which is what the reason denies."
                        );
                    }
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
fn no_cell_states_something_the_consumer_can_derive() {
    // The uniformity rule, settled with MC 2026-08-13. It replaces a test that
    // checked placeholder CLAIMS were accurate — accuracy stopped being the
    // question once the answer became "do not ship them at all".
    //
    // A placeholder flag is one line of the consumer's own spec, computable
    // from the catalog row in front of them. Shipping even one makes absence
    // ambiguous: a consumer cannot tell "not a placeholder" from "fusiform did
    // not ship the derivable cell here". Shipping zero makes absence mean one
    // thing again — the same absent-versus-unknown discipline the schema was
    // built for, applied to the dataset's own contents.
    //
    // Note this is NOT a claim that those rows are fine. 1,175 models publish
    // output == context and 186 publish output: 0, measured 2026-08-13. The
    // scale is reported in the design note; the cells stay reserved for what
    // only measurement can supply.
    let doc = overlay();
    for cell in doc["cells"].as_array().unwrap() {
        let id = format!(
            "{}/{}",
            cell["provider_id"].as_str().unwrap(),
            cell["model_id"].as_str().unwrap()
        );
        for (key, fact) in cell["facts"].as_object().unwrap() {
            let why = fact["value"].get("why").and_then(|v| v.as_str());
            assert!(
                !matches!(
                    why,
                    Some("placeholder_output_equals_context") | Some("placeholder_zero")
                ),
                "{id} {key}: this states a placeholder, which the consumer \
                 derives itself. Shipping one makes the absence of the others \
                 ambiguous."
            );
        }
    }

    // And the enforced value BEHIND a harmful advertisement must survive, or
    // this rule has quietly deleted the cells it was meant to preserve. The
    // ollama-cloud row advertises 1,048,576 output and enforces 65,536; that
    // number is available from nowhere else and is the reason the row leads the
    // batch.
    let ollama = doc["cells"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["provider_id"] == "ollama-cloud")
        .expect("the ollama-cloud cell must survive the placeholder drop");
    assert_eq!(
        ollama["facts"]["output.enforced"]["value"]["value"]
            .as_i64()
            .expect("output.enforced must still carry its measured value"),
        65536
    );
}

/// Keys established as not-single-valued, and therefore closed to promotion.
///
/// The vocabulary check asserts the `why` STRING is legal. This list asserts
/// the CLAIM survives — a different property enforced at a different site,
/// because the failures differ. A transcriber who replaces OpenRouter's refusal
/// with a `stated` geometry from one measured report writes a perfectly legal
/// cell: every legal `why` value is still legal, because the refusal is simply
/// gone.
///
/// SUBC's promotion clause made mechanical: presence of
/// `not_single_valued_at_key` forbids promoting any measured report into a
/// stated or bracket cell at that key WITHOUT a routing discriminant. The v1
/// schema has no discriminant sub-key, so today the clause means: do not
/// promote.
///
/// Removing an entry here is the decision point, and it must be a deliberate
/// edit rather than a side effect of adding a cell.
const NOT_SINGLE_VALUED: &[(&str, &str, &str)] = &[("openrouter", "*", "geometry")];

#[test]
fn a_key_closed_to_promotion_stays_closed() {
    let doc = overlay();
    for (provider, model, fact) in NOT_SINGLE_VALUED {
        let cell = doc["cells"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["provider_id"] == *provider && c["model_id"] == *model)
            .unwrap_or_else(|| {
                panic!(
                    "{provider}/{model} is established as not-single-valued for \
                     {fact:?} and its cell is gone. Deleting the refusal does not \
                     make the key measurable — it removes the only thing telling \
                     the next transcriber not to try."
                )
            });

        let value = &cell["facts"][fact]["value"];
        let kind = value["kind"].as_str().unwrap_or("<missing>");
        assert_eq!(
            kind, "unknown",
            "{provider}/{model} {fact}: promoted to {kind:?}. This key cannot \
             hold a single fact — the wall that fires belongs to whichever \
             upstream served the request, so a measurement samples a routing \
             decision rather than settling one. A stated value is correct for \
             one route and wrong for the next."
        );
        assert_eq!(
            value["why"].as_str().unwrap_or("<missing>"),
            "not_single_valued_at_key",
            "{provider}/{model} {fact}: still unknown, but the reason no longer \
             says the key is unmeasurable. Downgrading to an evidence-absence \
             reason invites exactly the measurement this cell exists to refuse."
        );
    }
}

/// Anthropic publishes context and output caps in fixed pairs.
///
/// From their model comparison table and the context-window guide, read
/// 2026-08-13: a 1M-token context window carries a 128k output cap, a 200k
/// window carries 64k. Every anthropic row in the catalog obeys this except the
/// two sonnet-4.5 spellings, which publish 1M context with 64k output — a
/// pairing that appears nowhere in Anthropic's documentation.
///
/// The row disagrees with ITSELF, which is what makes this checkable without a
/// network call: the output side says 200k-class, the context side says
/// 1M-class, and only one can be right.
const ANTHROPIC_PAIRS: &[(i64, i64)] = &[(1_000_000, 128_000), (200_000, 64_000)];

#[test]
fn every_self_contradicting_anthropic_row_has_a_corrective_cell() {
    // A fence at the producer, not a state of affairs. The two known rows were
    // found by reading a doc passage and then by running this rule across the
    // provider block; a THIRD such row appearing in a future seed must fail
    // here rather than wait for someone to notice.
    //
    // One-directional deliberately: a failing row without a cell is the hazard,
    // and it is a hard failure. A cell whose row has been fixed upstream is
    // merely redundant, so it does not fail — removing it is a judgment call
    // about whether the fix is durable, and this test has no standing to make
    // it.
    let seed: serde_json::Value = serde_json::from_str(SEED).unwrap();
    let doc = overlay();

    let corrected: BTreeSet<String> = doc["cells"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["provider_id"] == "anthropic")
        .filter(|c| c["facts"].get("window.advertised").is_some())
        .map(|c| c["model_id"].as_str().unwrap().to_string())
        .collect();

    let mut unguarded = Vec::new();
    let models = seed["anthropic"]["models"].as_object().unwrap();
    for (model_id, model) in models {
        let limit = &model["limit"];
        let (Some(context), Some(output)) = (limit["context"].as_i64(), limit["output"].as_i64())
        else {
            continue;
        };
        let Some((_, expected)) = ANTHROPIC_PAIRS.iter().find(|(c, _)| *c == context) else {
            // A context size outside both documented classes. Not a
            // contradiction — a new class, which needs a human to read the doc.
            continue;
        };
        if output != *expected && !corrected.contains(model_id) {
            unguarded.push(format!(
                "{model_id} (context {context}, output {output}, 1M class wants {expected})"
            ));
        }
    }

    assert!(
        unguarded.is_empty(),
        "anthropic rows publish a context/output pairing Anthropic's own docs do \
         not have, and no corrective cell covers them: {unguarded:?}. The row \
         contradicts itself, so one of its two numbers is wrong. Read the \
         context-window guide for that model and mint a cell, or record why the \
         pairing rule no longer holds."
    );

    // The control: the rule must actually convict something, or a future
    // refactor that breaks the pairing lookup would leave this test green and
    // silent.
    let known_failures = models
        .iter()
        .filter(|(_, m)| {
            let l = &m["limit"];
            match (l["context"].as_i64(), l["output"].as_i64()) {
                (Some(c), Some(o)) => ANTHROPIC_PAIRS.iter().any(|(pc, pe)| *pc == c && *pe != o),
                _ => false,
            }
        })
        .count();
    assert_eq!(
        known_failures, 2,
        "the seed should contain exactly the two known self-contradicting rows; \
         found {known_failures}. If the upstream fixed them, this number drops \
         and the corrective cells become redundant — a judgment call, not a \
         failure. If it rose, a new row needs a cell."
    );
}
