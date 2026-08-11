//! What fusiform actually serves, pinned as a set.
//!
//! Every other test here asserts that a particular fact is right. This one
//! asserts which facts EXIST, because the fact set is the contract: a field
//! joining it is a change no consumer asked for, and a field leaving it makes a
//! consumer's read start returning nothing. Neither shows up as a failure
//! anywhere else.
//!
//! The specific case that motivated it: a consumer asked to be told if fusiform
//! ever serves `last_updated`, because their ingest would then have a timestamp
//! to shortcut on — and that field is measurably unreliable (three models
//! repriced 20-40% on 2026-08-11 with `last_updated` unchanged). Answering
//! "no, and here is what stops it" required something that stops it.

use fusiform_core::normalize::normalize_models_dev;
use fusiform_store::ingest::fact_keys_of;

const FIXTURE: &[u8] = include_bytes!("../../fusiform-core/fixtures/models-dev-excerpt.json");

/// Every fact key fusiform can produce, as a closed set.
///
/// Adding a line here is a deliberate act. That is the entire point: the fact
/// set is the served contract, and it should not be possible to extend it by
/// adding a field to a struct.
const SERVED_FACT_KEYS: &[&str] = &[
    "existence",
    "limit.context",
    "limit.output",
    "capability.reasoning",
    "capability.tool_call",
    "capability.attachment",
    "capability.input_modalities",
    "capability.output_modalities",
    "rate.input",
    "rate.output",
    "rate.cache_read",
    "rate.cache_write",
    "rate.reasoning",
];

/// Fields the normalizer parses that must NEVER become facts.
///
/// Two different reasons, and the distinction matters:
///
/// - **Renderer-selecting** (`provider` overrides, `experimental`, provider
///   `npm`): serving them would let fusiform decide how a request is spoken,
///   which is the one thing it must never do.
/// - **Unreliable** (`last_updated`): serving it would hand consumers a
///   change-detection shortcut that does not work. Measured on 2026-08-11:
///   three models repriced by 20-40% with this field unchanged, and separately
///   a renderer override was deleted within 36 minutes with it unchanged.
///
/// A consumer cannot misuse a field it never receives. That is a producer-side
/// guarantee and it is the only one available here, since fusiform cannot
/// constrain what a consumer does with data it hands over.
const NEVER_SERVED: &[&str] = &[
    "last_updated",
    "provider",
    "experimental",
    "npm",
    "release_date",
    "knowledge",
];

#[test]
fn the_served_fact_set_is_exactly_this() {
    let outcome = normalize_models_dev(FIXTURE).expect("fixture normalizes");

    let mut produced: Vec<String> = Vec::new();
    for model in outcome.catalog.models() {
        for key in fact_keys_of(model) {
            let key = key.as_str().to_string();
            if !produced.contains(&key) {
                produced.push(key);
            }
        }
    }
    // Existence is written by the ingest planner rather than by `facts_of`, so
    // it is added here to make the set complete.
    produced.push("existence".to_string());

    // Tiered rate keys carry their threshold, so they are checked structurally
    // rather than listed: the thresholds come from the upstream and are not
    // fusiform's to enumerate. The separator is read from the key constructor
    // rather than guessed — an earlier version of this test guessed
    // `_above_context_` and the real format is `.above_context.`, which sorted
    // every tiered key into the wrong bucket.
    let (tiered, flat): (Vec<String>, Vec<String>) = produced
        .into_iter()
        .partition(|k| k.contains(".above_context."));

    for key in &flat {
        assert!(
            SERVED_FACT_KEYS.contains(&key.as_str()),
            "fact key {key:?} is produced but not in the served vocabulary. \
             If this is deliberate, add it to SERVED_FACT_KEYS — and consider \
             what a consumer will do with it."
        );
    }

    // A tiered key must be a rate key whose base class is in the vocabulary and
    // whose threshold is a number. Checking the shape rather than the string
    // keeps the upstream's thresholds out of this file while still refusing a
    // key that is tiered by something other than context.
    assert!(
        !tiered.is_empty(),
        "the fixture must produce tiered rate keys, or this branch proves nothing"
    );
    for key in &tiered {
        let (base, threshold) = key
            .split_once(".above_context.")
            .expect("partitioned on this separator");
        assert!(
            SERVED_FACT_KEYS.contains(&base),
            "tiered key {key:?} has base {base:?}, which is not a served fact key"
        );
        assert!(
            base.starts_with("rate."),
            "only a rate can be tiered by context: {key:?}"
        );
        assert!(
            threshold.parse::<u64>().is_ok(),
            "tiered key {key:?} carries a non-numeric threshold {threshold:?}"
        );
    }

    // And the reverse direction: a key in the vocabulary that nothing produces
    // is a consumer reading something that will never arrive.
    for expected in SERVED_FACT_KEYS {
        let produced_somewhere = flat.iter().any(|k| k == expected);
        assert!(
            produced_somewhere,
            "{expected:?} is in the served vocabulary but no model produces it"
        );
    }
}

/// No upstream field name that must never be served appears in any fact key.
///
/// A weaker check than it looks — it catches a field becoming a fact under its
/// own name, which is how it would actually happen. A field smuggled in under a
/// different key would pass this and fail the set assertion above.
#[test]
fn unreliable_and_renderer_selecting_fields_are_not_facts() {
    let outcome = normalize_models_dev(FIXTURE).expect("fixture normalizes");

    for model in outcome.catalog.models() {
        for key in fact_keys_of(model) {
            for forbidden in NEVER_SERVED {
                assert!(
                    !key.as_str().contains(forbidden),
                    "{}: fact key {:?} contains {forbidden:?}, which must never be served",
                    model.key,
                    key.as_str()
                );
            }
        }
    }
}

/// `last_updated` is parsed, and it reaches nothing.
///
/// Parsed rather than dropped because its ABSENCE from a model is itself
/// interesting to an operator, and because dropping a field at the parse
/// boundary makes it impossible to tell a field that was never published from
/// one the parser discarded. What must not happen is it reaching a consumer.
#[test]
fn last_updated_is_parsed_but_never_reaches_a_fact() {
    let outcome = normalize_models_dev(FIXTURE).expect("fixture normalizes");

    // It is genuinely parsed: this test would be vacuous if the field were
    // simply absent from the normalized model.
    let with_timestamp = outcome
        .catalog
        .models()
        .filter(|m| m.last_updated.is_some())
        .count();
    assert!(
        with_timestamp > 0,
        "the fixture must contain models carrying last_updated, or this proves nothing"
    );

    // And it reaches no fact, on any model, including the ones that carry it.
    //
    // Substring rather than equality. An earlier version compared the key for
    // exact equality with "last_updated", and a mutation adding the field as a
    // fact produced `capability.last_updated` — which passed. A field joining
    // the fact set arrives under a namespaced key, never a bare one, so exact
    // equality tests for the one spelling that cannot occur.
    for model in outcome.catalog.models() {
        for key in fact_keys_of(model) {
            assert!(
                !key.as_str().contains("last_updated"),
                "{}: last_updated reached the fact set as {:?}. It is measurably \
                 unreliable — three models repriced 20-40% on 2026-08-11 with \
                 this field unchanged — and serving it hands consumers a \
                 change-detection shortcut that does not work.",
                model.key,
                key.as_str()
            );
        }
    }
}
