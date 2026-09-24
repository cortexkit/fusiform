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
    // Whose wall a request on this path hits. NOT lineage: 3,401 of 6,320 rows
    // carry a vendor-namespaced model id, so "does this provider originate the
    // model" is derivable by any consumer holding the catalog and must never
    // be a cell. This key answers the question the catalog cannot: openrouter
    // and ollama-cloud are BOTH gateways by every id-based test, and only one
    // lets other people's walls through.
    //
    // The distinction licenses opposite consumer behaviour. MC demotes a
    // forwarder's authored input to the conservative of two derivations; doing
    // that to an imposer would be wrong, because its numbers describe its own
    // path — and a gateway sweep that refused ollama-cloud would have deleted
    // the measured 16x output correction, the most valuable row in the dataset.
    "path.wall_ownership",
];

/// Who owns the wall on a serving path.
const WALL_OWNERSHIP: &[&str] = &["forwards", "imposes"];

/// Who made the observation behind a behavioural claim.
///
/// An axis ORTHOGONAL to `grade`, added 2026-08-14 on MC's ruling and for their
/// reason: mixing provenance into the strength enum is exactly how the two got
/// conflated in `provider_asserted_doc`, and prose is where facts go to stop
/// being machine-checkable — the swap-check below could not have read a
/// citation.
///
/// Absent means UNSTATED, never a default. Silence is not the flattering value.
///
/// # The names are record-relative, and that is load-bearing
///
/// First drafted as `first_party | third_party` and renamed before any second
/// cell existed, because those two readings disagree on every cell this seat
/// will ever measure itself:
///
///   - relative to the PROVIDER, fusiform is a stranger to everyone, so its own
///     measurements are third-party and the axis collapses into "did the
///     provider say it" — which `grade` already carries, leaving the field
///     redundant;
///   - relative to the RECORD, an observation made here is distinct from one
///     read elsewhere, which is the made-versus-read gap the field exists to
///     close.
///
/// `first_party`/`third_party` is the vocabulary of PROVIDER relationships,
/// which is precisely the axis this one is orthogonal to, so reusing it invites
/// the collapsed reading. MC's rule for preferring a rename over a documented
/// constant: **a constant is read once by whoever writes the decoder; a name is
/// read every time by everyone.**
const OBSERVED_BY: &[&str] = &["self_observed", "reported"];

/// A wall-ownership cell states one of exactly two things, and the two license
/// OPPOSITE consumer behaviour.
///
/// MC demotes a forwarder's authored input to the conservative of two
/// derivations, because the number describes a backend they did not choose.
/// Doing that to an imposer would be wrong: its number describes its own
/// serving path and is correct. So a typo here is not a missing cell, it is a
/// cell that licenses the wrong action — which is why the vocabulary is pinned
/// rather than left to prose.
/// `observed_by`, where present, is in the vocabulary and sits on a claim it
/// can qualify.
///
/// A provenance mark on a DOC-graded cell would be noise: the provider asserted
/// it, so who read the page is not a property of the evidence. The axis only
/// bites on behavioural grades, where the same refusal can be seen by the
/// provider, by this seat, or by a stranger.
#[test]
fn a_provenance_mark_is_in_vocabulary_and_qualifies_a_behavioural_claim() {
    let overlay = overlay();
    let mut checked = 0usize;
    for cell in overlay["cells"].as_array().expect("cells is an array") {
        let id = format!(
            "{}/{}",
            cell["provider_id"].as_str().unwrap_or("?"),
            cell["model_id"].as_str().unwrap_or("?")
        );
        for (key, fact) in cell["facts"].as_object().expect("facts is an object") {
            let Some(by) = fact.get("observed_by").and_then(|v| v.as_str()) else {
                continue;
            };
            assert!(
                OBSERVED_BY.contains(&by),
                "{id} {key}: observed_by {by:?} is outside the vocabulary"
            );
            let grade = fact["grade"].as_str().unwrap_or("");
            assert!(
                matches!(grade, "measured" | "provider_asserted_runtime"),
                "{id} {key}: observed_by qualifies an OBSERVATION, and this cell \
                 is graded {grade:?}. On a doc-sourced claim the provider is the \
                 asserter and who read the page says nothing about the evidence."
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 1,
        "no provenance marks found; the field was added for ollama-cloud's \
         third-party ceiling report and a parse finding none is reading nothing"
    );
}

#[test]
fn a_wall_ownership_cell_states_one_of_the_two_claims() {
    let overlay = overlay();
    let mut checked = 0usize;
    for cell in overlay["cells"].as_array().expect("cells is an array") {
        let Some(fact) = cell["facts"].get("path.wall_ownership") else {
            continue;
        };
        let id = format!(
            "{}/{}",
            cell["provider_id"].as_str().unwrap_or("?"),
            cell["model_id"].as_str().unwrap_or("?")
        );
        let value = fact["value"]["value"]
            .as_str()
            .unwrap_or_else(|| panic!("{id}: wall ownership must be a string"));
        assert!(
            WALL_OWNERSHIP.contains(&value),
            "{id}: wall ownership {value:?} is outside the vocabulary. The two \
             values license opposite consumer behaviour, so an unrecognised one \
             is worse than an absent cell."
        );
        // Behavioural, so it can never be graded from a document alone.
        assert_eq!(
            fact["grade"].as_str(),
            Some("measured"),
            "{id}: wall ownership is a property of what the path DOES, so it \
             cannot be sourced from a doc page. If it was not measured, it is \
             not known."
        );
        checked += 1;
    }
    assert!(
        checked >= 2,
        "found {checked} wall-ownership cells; the deliverable is two \
         providers and a parse finding fewer means this test is reading nothing"
    );
}

/// A wall-ownership claim agrees with the provider's other cells.
///
/// # Why the vocabulary check is not enough
///
/// Mutation found this: swapping `forwards` and `imposes` produces two
/// PERFECTLY LEGAL cells and survives every check above, because both values
/// are in the vocabulary. The result is two providers each licensing the exact
/// opposite of the correct consumer action — a forwarder trusted for
/// pre-carve, an imposer demoted away from its own true numbers.
///
/// A vocabulary can only reject a value nobody defined. It cannot reject the
/// wrong one of two defined values, and that is the failure that matters here.
///
/// What separates them is not the string, it is what else the provider says:
///
/// - **forwards** means the value at a key belongs to a backend, so the
///   provider must carry a refusal (`not_single_valued_at_key`) somewhere.
///   Marking a forwarder while claiming a definite window is incoherent.
/// - **imposes** means the provider's own ceiling is real and measurable, so it
///   must carry a measured enforced value. Marking an imposer with nothing
///   measured is a claim with no observation behind it.
#[test]
fn a_wall_ownership_claim_agrees_with_the_providers_other_cells() {
    let overlay = overlay();
    let cells = overlay["cells"].as_array().expect("cells is an array");
    let mut checked = 0usize;

    for cell in cells {
        let Some(fact) = cell["facts"].get("path.wall_ownership") else {
            continue;
        };
        let provider = cell["provider_id"].as_str().unwrap_or("?");
        let claim = fact["value"]["value"].as_str().unwrap_or("?");

        // Every other fact this provider states.
        let siblings: Vec<&serde_json::Value> = cells
            .iter()
            .filter(|c| c["provider_id"].as_str() == Some(provider))
            .flat_map(|c| c["facts"].as_object().into_iter().flat_map(|o| o.values()))
            .collect();

        match claim {
            "forwards" => {
                let refuses = siblings
                    .iter()
                    .any(|f| f["value"]["why"].as_str() == Some("not_single_valued_at_key"));
                assert!(
                    refuses,
                    "{provider} is marked as forwarding someone else's wall, but \
                     states no not_single_valued_at_key refusal. If its values \
                     really belong to a backend it did not choose, at least one \
                     key cannot hold a single fact — and if every key holds one, \
                     it is not forwarding."
                );
            }
            "imposes" => {
                // Either BEHAVIOURAL grade, not just `measured`.
                //
                // The first version of this check demanded `measured` and fired
                // on real data: ollama-cloud's ceiling is a runtime refusal
                // reported by a third party, which is `provider_asserted_runtime`.
                // What an imposed wall requires is an observation of BEHAVIOUR —
                // a refusal is one whoever watched it happen. A doc assertion is
                // not, which is the distinction that stays load-bearing here.
                let measured = siblings.iter().any(|f| {
                    matches!(
                        f["grade"].as_str(),
                        Some("measured") | Some("provider_asserted_runtime")
                    ) && f["value"]["kind"].as_str() == Some("stated")
                        && f["value"]["value"].is_number()
                });
                assert!(
                    measured,
                    "{provider} is marked as imposing its own wall, but states no \
                     behaviourally-observed numeric value. An imposed ceiling is \
                     observable by definition — that is what distinguishes it from a forwarded \
                     one — so a claim with nothing measured behind it is the \
                     wrong half of the pair."
                );
            }
            other => panic!("{provider}: unknown wall ownership {other:?}"),
        }
        checked += 1;
    }

    assert!(
        checked >= 2,
        "checked {checked} claims; both providers must be reached or a swap \
         goes unexamined at the one that is not"
    );
}

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

            // AN EXEMPTION FOR A MODEL THE SEED NOW CARRIES IS REFUSED.
            //
            // A seed refresh invalidates its own exemptions: the block exists
            // because the model arrived after the snapshot was minted, and the
            // next snapshot contains it. Left in place the exemption is not
            // merely stale — the `continue` below skips the existence check
            // that would now PASS, so a bypass stays live for a case that no
            // longer needs bypassing, and a typo in the model id would be
            // waved through by an escape hatch nobody needs.
            //
            // Found by refreshing the seed on 2026-08-16 and asking which
            // exemptions the refresh had made unnecessary. Exactly one had:
            // `xai/grok-4.6`, exempted when it arrived six hours after the
            // 2026-08-12 snapshot. Nothing would have reported it.
            assert!(
                seed[provider]["models"].get(model).is_none(),
                "{provider}/{model} carries a post_seed exemption AND appears \
                 in the current seed, so the exemption is unnecessary and is \
                 suppressing a check that would pass. Delete the post_seed \
                 block: the cell can be validated against the snapshot \
                 directly now."
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

            // `boundary` says whose clock `observed_at` is on. `Asserted` means
            // a SOURCE stated an effective date, so `observed_at` is that date
            // rather than when anyone looked. A `measured` fact is by
            // definition fusiform's own observation, so its clock is fusiform's
            // and the label must be `Observed`. The two wall-ownership cells
            // carried this contradiction, which told a reader that openrouter
            // began forwarding at the instant it was measured.
            //
            // This catches only the mechanical half. A doc page read with no
            // date on it is `Observed` too, and whether a page states a date
            // is not checkable here.
            assert!(
                !(grade == "measured" && boundary == "Asserted"),
                "{id} {key}: a measured fact is fusiform's own observation, so \
                 its boundary is Observed; Asserted claims a source stated an \
                 effective date"
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

/// No served overlay fact is older than `OVERLAY_MAX_AGE_MS`.
///
/// # Why this plane needs a review date at all
///
/// It is curated: no fetch, no diff, and therefore no possible contradiction.
/// Nothing here can ever disagree with itself, so a review date is the ONLY
/// liveness signal this data can carry — the same argument that put one on the
/// plan-price plane, applied to the plane that predates it by a month.
///
/// The decisive case is already shipped: `claude-sonnet-4-5` carries a
/// `limit.context` override of 200k against an upstream publishing 1M. If
/// Anthropic ever ships a real 1M window, that override becomes ACTIVELY WRONG
/// — serving a limit five times too small, which causes premature compaction —
/// and absolutely nothing would notice. A refusal cell cannot rot that way; a
/// value cell can.
///
/// # Where the interval comes from
///
/// Measured on this store, 2026-09-19, rather than chosen:
///
///     models carrying a context limit          8672
///     models that have ever changed it          341   (3.9% over 38 days)
///
/// which is a per-model rate of about 1% per 10 days. Across the nine concrete
/// models this overlay covers:
///
///      30 days -> 0.28 expected stale cells
///      60 days -> 0.56
///      90 days -> 0.84
///     120 days -> 1.12
///
/// Sixty, because the expected number of stale cells at review time stays
/// comfortably under one. A gate whose expected finding is MORE than one stale
/// row is a gate that is usually right to fire, which sounds good and is not:
/// it means the data is routinely wrong between reviews.
///
/// # The proxy, stated because it is imperfect in a known direction
///
/// That base rate measures how often MODELS.DEV CHANGES ITS PUBLISHED LIMIT,
/// and these cells record MEASURED ENFORCEMENT. Those are different quantities,
/// and the overlay exists precisely because the second is not visible in the
/// first — so this is the best available signal rather than the right one. It
/// is a frequency estimate, not a claim that a republished limit implies a
/// changed wall.
///
/// # Reads `observed_at`, which is already there
///
/// Deliberately no new field. This file is ingested directly from the repo path
/// by a consumer, so a schema change is their problem as well as mine, and the
/// dates needed are already on every fact.
const OVERLAY_MAX_AGE_MS: i64 = 60 * 24 * 60 * 60 * 1000;

#[test]
fn no_served_overlay_fact_has_outrun_its_review() {
    let doc = overlay();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_millis() as i64;

    let mut overdue: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for cell in doc["cells"].as_array().expect("cells is an array") {
        let provider = cell["provider_id"].as_str().unwrap_or("?");
        let model = cell["model_id"].as_str().unwrap_or("?");

        for (key, fact) in cell["facts"].as_object().expect("facts is an object") {
            // A REFUSAL CELL CANNOT GO STALE, and skipping it is not laziness.
            //
            // Its claim is "nobody established this", which stays true until
            // someone measures — and if someone does, they are adding a value,
            // not letting one rot. Reviewing these would be asking an operator
            // to re-confirm an absence on a schedule, which is exactly the
            // busywork that teaches a reader to rubber-stamp the real ones.
            let is_refusal = fact["value"]["kind"].as_str() == Some("unknown");
            if is_refusal {
                continue;
            }

            let stamp = fact["observed_at"]
                .as_str()
                .or_else(|| fact["asserted_at"].as_str())
                .unwrap_or_else(|| {
                    panic!("{provider}/{model} {key} carries no date to review against")
                });

            let ms = chrono_ms(stamp);
            checked += 1;
            let age = now - ms;
            if age > OVERLAY_MAX_AGE_MS {
                overdue.push(format!(
                    "  {provider}/{model}  {key}\n      observed {stamp}, {} days ago\n      source {}",
                    age / (24 * 60 * 60 * 1000),
                    fact["source_ref"].as_str().unwrap_or("(none)")
                ));
            }
        }
    }

    // NON-VACUITY, because every other arm here passes on an empty overlay.
    assert!(
        checked >= 20,
        "this examined {checked} served facts; the overlay had 22 when this was \
         written, so a collapse to nothing means the walk broke rather than the \
         data improving"
    );

    assert!(
        overdue.is_empty(),
        "\n\nNOT A BUILD FAILURE. Nothing changed; a review date passed.\n\n\
         These overlay facts have not been re-checked in {} days:\n\n{}\n\n\
         Each one OVERRIDES what the upstream publishes, so a stale cell serves \
         a value fusiform asserts and nobody has confirmed lately.\n\n\
         Re-read the source, then update observed_at — WHETHER OR NOT the value \
         moved. Confirming a limit is a real result and the date is what records \
         that someone looked.\n\n\
         What this gate does NOT tell you: whether these values are correct. It \
         knows only that nobody has looked recently.\n",
        OVERLAY_MAX_AGE_MS / (24 * 60 * 60 * 1000),
        overdue.join("\n")
    );
}

/// The date parser agrees with an independent implementation.
///
/// Without this the review gate passes VACUOUSLY: a parser returning a constant
/// or a wrong epoch makes every fact look fresh, and the gate reports green
/// while measuring nothing. The values are Python's `datetime`, computed
/// separately rather than copied from my own output — a fixture generated by
/// the thing under test agrees with itself for free.
///
/// The leap day is in there deliberately. It is the one case civil-date
/// arithmetic gets wrong when it is written from memory, and a table of
/// round-numbered dates would never touch it.
#[test]
fn the_date_parser_agrees_with_a_second_implementation() {
    for (stamp, expect) in [
        ("1970-01-01T00:00:00Z", 0i64),
        ("2026-08-13T15:12:00Z", 1_786_633_920_000),
        ("2026-09-19T00:00:00Z", 1_789_776_000_000),
        ("2024-02-29T12:00:00Z", 1_709_208_000_000),
    ] {
        assert_eq!(chrono_ms(stamp), expect, "parsing {stamp}");
    }
}

/// Parse an RFC3339 stamp to epoch millis without pulling in a date crate.
///
/// The overlay's stamps are all `YYYY-MM-DDTHH:MM:SSZ`, written by hand and
/// fenced for that shape by `every_fact_is_completely_specified`, so a full
/// parser would be answering a more general question than this file asks.
fn chrono_ms(stamp: &str) -> i64 {
    let (date, rest) = stamp.split_once('T').expect("an RFC3339 stamp has a T");
    let d: Vec<i64> = date
        .split('-')
        .map(|p| p.parse().expect("numeric"))
        .collect();
    let t: Vec<i64> = rest
        .trim_end_matches('Z')
        .split(':')
        .map(|p| p.parse::<f64>().expect("numeric") as i64)
        .collect();

    // Days since epoch by civil-date arithmetic (Howard Hinnant's algorithm),
    // which is exact for any proleptic Gregorian date and needs no table.
    let (y, m, day) = (d[0], d[1], d[2]);
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    (days * 86_400 + t[0] * 3600 + t[1] * 60 + t[2]) * 1000
}
