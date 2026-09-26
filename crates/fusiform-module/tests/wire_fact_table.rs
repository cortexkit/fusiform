//! The wire crate's fact table, held against the real producer.
//!
//! `SERVED_FACTS` is the only place a consumer can learn which served fields
//! may change their request bytes. That distinction lived in fusiform's charter
//! and in one conversation with the consumer who named the three byte-affecting
//! fields — neither reachable from the artifact a consumer compiles against,
//! which made it a relationship recorded nowhere a check could see.
//!
//! This test lives in `fusiform-module` rather than beside the other vocabulary
//! tests because it needs both the producer and the wire crate, and
//! `fusiform-protocol` stays a leaf: a consumer compiling against it must not
//! pull in the store.
//!
//! # What this test could NOT have told me, stated because it is not obvious
//!
//! ASTRO's test for a check worth trusting: could this artifact have told me I
//! was wrong? Applied here, the answer differs by claim, and only one of the
//! three is genuinely checked from this side.
//!
//! **The key set: yes.** It comes from the real normalizer over real upstream
//! bytes. A fact appearing or vanishing reddens this, and neither the table nor
//! my belief about it can prevent that.
//!
//! **Rates are money: yes, structurally.** The key prefix determines the class,
//! so a rate classified otherwise is a contradiction the code can find.
//!
//! **The classification of everything else: NO.** Whether
//! `capability.attachment` is advisory or byte-affecting is a fact about a
//! CONSUMER'S RENDERER, and nothing in this repository can contradict me about
//! it. The three byte-affecting entries are marked as such because BROCA read
//! them out of their own source and told me; the advisory entries are advisory
//! because nobody has said otherwise, which is a weaker claim wearing the same
//! typeface.
//!
//! That asymmetry is not fixable from here and pretending otherwise would be
//! the defect this file exists to prevent. It is why the classification carries
//! attribution in `SERVED_FACTS` rather than reading as fusiform's own
//! determination, and why the standing question to consumers is whether the
//! byte-affecting set is still complete rather than whether it looks right.

use fusiform_core::normalize::normalize_models_dev;
use fusiform_store::ingest::fact_keys_of;

const FIXTURE: &[u8] = include_bytes!("../../fusiform-core/fixtures/models-dev-excerpt.json");

/// The wire crate's fact table lists exactly the facts fusiform serves.
///
/// `SERVED_FACTS` is the only place a consumer can learn which fields may
/// change their request bytes — the distinction lived in fusiform's charter and
/// in one conversation, neither of which is reachable from the artifact a
/// consumer compiles against. That made it a relationship recorded nowhere a
/// check could see, which is the failure mode this test exists to prevent
/// recurring.
///
/// A table nobody checks drifts, and a drifted table is worse than none: it
/// tells a consumer a fact is advisory while the producer serves it as
/// byte-affecting. So the table is held against the real producer — the
/// normalizer running over real upstream bytes, plus the existence fact that
/// ingest writes from presence logic.
#[test]
fn the_wire_fact_table_matches_what_the_producer_emits() {
    use fusiform_protocol::{FactClass, SERVED_FACTS};

    let outcome = normalize_models_dev(FIXTURE).expect("the fixture must normalize");

    let mut produced: Vec<String> = Vec::new();
    for model in outcome.catalog.models() {
        for key in fact_keys_of(model) {
            produced.push(key.as_str().to_string());
        }
    }
    // Written by plan_ingest from presence logic rather than from a model's
    // fields, so it never appears in fact_keys_of.
    produced.push("existence".to_string());
    produced.sort();
    produced.dedup();

    let tabled: Vec<&str> = SERVED_FACTS.iter().map(|f| f.key).collect();

    // Every produced fact must be in the table. A fact a consumer receives and
    // cannot classify is exactly the gap this closes.
    for key in &produced {
        if key.contains(".above_context.") {
            // Tiered keys carry an upstream threshold; the base class is what
            // the table describes, checked below.
            let base = key.split_once(".above_context.").unwrap().0;
            assert!(
                tabled.contains(&base),
                "tiered fact {key:?} has base {base:?}, which the wire table does not list"
            );
            continue;
        }
        if key.contains(".mode.") {
            // Mode rates carry the upstream's mode name; the base class is
            // what the table describes, as for tiers.
            let base = key.split_once(".mode.").unwrap().0;
            assert!(
                tabled.contains(&base),
                "mode fact {key:?} has base {base:?}, which the wire table does not list"
            );
            continue;
        }
        assert!(
            tabled.contains(&key.as_str()),
            "the producer emits {key:?} and the wire table does not list it — a \
             consumer receiving it cannot tell whether it affects their request bytes"
        );
    }

    // And every tabled fact must be produced, or the table promises something
    // that never arrives.
    for key in &tabled {
        assert!(
            produced.iter().any(|p| p == key),
            "the wire table lists {key:?} and the producer never emits it"
        );
    }

    // The fields BROCA enumerated from their own render path, named
    // individually rather than counted: a count would pass if one were swapped
    // for another.
    //
    // `limit.output` is deliberately NOT here. It was byte-affecting on my
    // belief that a consumer rendered it as a request parameter, and BROCA
    // enumerated `resolve_frozen` and found zero reads of it — their output cap
    // comes from the caller's max_tokens. That correction is the reason this
    // list is short: it is what a consumer read out of their source, not what
    // the field names suggest.
    for key in ["limit.context", "capability.reasoning"] {
        let fact = SERVED_FACTS
            .iter()
            .find(|f| f.key == key)
            .unwrap_or_else(|| panic!("{key} must be in the table"));
        assert_eq!(
            fact.class,
            FactClass::ByteAffecting,
            "{key} is rendered into a provider request by a consumer; \
             classifying it as advisory tells them a wrong value is cosmetic"
        );
    }

    // Every rate is money. A rate classified as advisory would tell a consumer
    // that a wrong value cannot cost anything.
    for fact in SERVED_FACTS.iter().filter(|f| f.key.starts_with("rate.")) {
        assert_eq!(
            fact.class,
            FactClass::Money,
            "{} is a rate and must be classified as money",
            fact.key
        );
    }
}

/// `limit.output` is byte-affecting, because a named consumer renders it.
///
/// Its class follows a consumer's render path, not how the field sounds, and
/// that path has changed once already. Until 2026-09-25 BROCA took the output
/// cap only from the caller's max_tokens and had zero reads of this fact, so it
/// was advisory. BROCA v0.3.125 (placed 2026-09-25) made the catalog maximum
/// the default max_tokens for any send whose caller sets no cap, so a wrong
/// value now changes request bytes.
///
/// Pinned so a move in either direction is deliberate: moving it back needs
/// BROCA to stop rendering it, not a reader deciding it looks advisory.
#[test]
fn limit_output_is_byte_affecting_because_broca_renders_it_as_the_default_cap() {
    use fusiform_protocol::{FactClass, SERVED_FACTS};

    let fact = SERVED_FACTS
        .iter()
        .find(|f| f.key == "limit.output")
        .expect("limit.output is served");

    assert_eq!(
        fact.class,
        FactClass::ByteAffecting,
        "limit.output is BROCA's default max_tokens since v0.3.125, so a wrong \
         value changes request bytes. Reclassifying it advisory needs BROCA to \
         stop rendering it first."
    );
}

/// The design note's status table lists every tool the module actually serves.
///
/// Found stale: §10 Serve listed three tools for the several hours after
/// `catalog.correct` went live. The row was written when the read surface was
/// the whole surface, and adding a tool did not feel like changing what §10
/// says — so a reader citing it would have believed the served surface was
/// read-only, which is the most consequential property in that row to be wrong
/// about.
///
/// This is the narrow, mechanical part of the staleness problem: the tool list
/// is checkable, so a test should check it rather than a human re-reading. The
/// rest of that table is prose about behaviour and stays a human's job.
#[test]
fn the_design_notes_serve_row_lists_every_served_tool() {
    const NOTE: &str = include_str!("../../../docs/design/schema-and-store.md");

    let row = NOTE
        .lines()
        .find(|l| l.starts_with("| §10 Serve"))
        .expect("the status table must have a §10 Serve row");

    for tool in fusiform_protocol::TOOLS {
        assert!(
            row.contains(tool),
            "the module serves {tool} and the design note's §10 Serve row does \
             not mention it. A reader citing that row would get a wrong picture \
             of the served surface.\n\nrow: {row}"
        );
    }
}
