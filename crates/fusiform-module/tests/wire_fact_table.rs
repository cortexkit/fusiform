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

    // The three fields BROCA named from their own source must be classified as
    // byte-affecting. Naming them here rather than counting: a count would pass
    // if one were swapped for another.
    for key in ["limit.context", "limit.output", "capability.reasoning"] {
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
