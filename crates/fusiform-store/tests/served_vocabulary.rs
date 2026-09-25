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
use fusiform_store::FactKey;

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
    // The reasoning settings the model accepts, verbatim from the upstream.
    // Admitted deliberately: a consumer maps its own reasoning levels onto this
    // list and refuses any the model does not name. The mapping from an entry
    // to request bytes stays the consumer's; fusiform serves only the list.
    "capability.reasoning_options",
    "capability.tool_call",
    "capability.attachment",
    "capability.input_modalities",
    "capability.output_modalities",
    "rate.input",
    "rate.output",
    "rate.cache_read",
    "rate.cache_write",
    "rate.reasoning",
    // What the model IS, as upstream states it, independent of who serves it.
    // Admitted deliberately: `family` is published identically by every
    // provider serving the same weights, and `open_weights` marks the
    // population where one provider's list price says something about
    // another's row. A consumer relates rows across providers with them.
    "model.family",
    "model.open_weights",
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

/// Every namespace prefix matches at least one real fact key.
///
/// A prefix filter that matches nothing returns an EMPTY catalog, not an error,
/// so a namespace nobody can select reads as "this catalog has no prices".
///
/// **What this test can and cannot catch, measured by mutation.** Renaming
/// `prefix::RATE` does NOT fail this test: the key constructors are built from
/// the same constant, so both move together and the prefix still matches. That
/// drift is caught by `the_served_fact_set_is_exactly_this`, whose expected
/// keys are written out as literals — the one place the spelling is stated
/// independently.
///
/// What this test does catch is a key landing in a namespace no filter
/// declares: proven by adding a `provenance.` fact, which reddens it. That is
/// the reachability property, and it is the one the constants cannot
/// self-confirm.
///
/// Recording the split because the first version of this comment claimed the
/// test caught prefix drift, in the same paragraph that explained why nothing
/// built from the constants could.
#[test]
fn every_namespace_prefix_selects_real_facts() {
    let outcome = normalize_models_dev(FIXTURE).expect("fixture normalizes");

    let mut produced: Vec<String> = Vec::new();
    for model in outcome.catalog.models() {
        for key in fact_keys_of(model) {
            produced.push(key.as_str().to_string());
        }
    }
    assert!(!produced.is_empty(), "the fixture must produce facts");

    for prefix in [
        fusiform_store::prefix::RATE,
        fusiform_store::prefix::CAPABILITY,
        fusiform_store::prefix::LIMIT,
    ] {
        let matched = produced.iter().filter(|k| k.starts_with(prefix)).count();
        assert!(
            matched > 0,
            "prefix {prefix:?} matches no fact key, so any filter using it \
             returns an empty result that reads as a legitimate answer"
        );
    }

    // And the prefixes partition the non-existence keys: a key matching none of
    // them is unreachable through any plane filter a consumer can express.
    for key in &produced {
        if key == "existence" {
            continue;
        }
        let reachable = [
            fusiform_store::prefix::RATE,
            fusiform_store::prefix::CAPABILITY,
            fusiform_store::prefix::LIMIT,
            fusiform_store::prefix::MODEL,
        ]
        .iter()
        .any(|p| key.starts_with(p));
        assert!(
            reachable,
            "{key:?} belongs to no declared namespace, so no plane filter reaches it"
        );
    }
}

/// An unknown modality list is stored as `null`, never as `[]`.
///
/// The parse boundary keeps the distinction — `None` for a missing block,
/// `Some(vec![])` for a published empty one — and it is worth nothing if the
/// storage boundary flattens it. An empty list is a positive claim that the
/// model accepts no input modality; null says the upstream did not say.
///
/// Mutation-proven, and it had to be: rendering `None` as `[]` passed every
/// other test in this repository, because the only assertion on the
/// distinction lived on the core type rather than on the stored value.
#[test]
fn an_unknown_modality_list_is_stored_as_null() {
    let mut doc: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    let model_obj = doc
        .get_mut("anthropic")
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.get_mut("claude-sonnet-4-5"))
        .and_then(|m| m.as_object_mut())
        .expect("the fixture carries this model");
    assert!(
        model_obj.remove("modalities").is_some(),
        "the fixture must have had a block, or the mutation proves nothing"
    );

    let outcome = normalize_models_dev(&serde_json::to_vec(&doc).unwrap()).unwrap();
    let model = outcome
        .catalog
        .models()
        .find(|m| m.key.model_id == "claude-sonnet-4-5")
        .expect("the model normalizes");

    let facts: std::collections::BTreeMap<_, _> = fusiform_store::ingest::facts_with_values(model)
        .into_iter()
        .collect();
    let input = facts
        .get(&FactKey::capability("input_modalities"))
        .expect("the fact is always emitted");
    assert_eq!(
        input, "null",
        "an unpublished modality block must store as null, not as an empty list"
    );

    // A model that DOES publish one still stores a list, so this is not
    // everything becoming null.
    let published = outcome
        .catalog
        .models()
        .find(|m| m.key.model_id == "gpt-5.6-luna")
        .expect("this model publishes modalities");
    let facts: std::collections::BTreeMap<_, _> =
        fusiform_store::ingest::facts_with_values(published)
            .into_iter()
            .collect();
    let input = facts
        .get(&FactKey::capability("input_modalities"))
        .expect("the fact is always emitted");
    assert!(
        input.starts_with('[') && input.contains("text"),
        "a published block must store as a list: {input}"
    );
}

/// A tier rate may exist with NO base rate of the same class, and that is a
/// real published state rather than a defect to repair.
///
/// # Why this is pinned
///
/// I told a consumer that every tiered model also publishes a base rate valid
/// below its threshold — and flagged it as a measurement rather than a
/// contract. Measuring it properly found the counterexample the same hour:
/// `auriko/qwen-3.6-plus` publishes `cache_write` at the over-threshold tier
/// and no base `cache_write` at all, so 1 of 355 tiered models on the payload
/// of 2026-08-15 breaks the property.
///
/// The served result is correct and must stay correct: the model carries
/// `rate.cache_write.above_context.256000` and NO `rate.cache_write`. A
/// consumer below the threshold has no cache-write rate, which is
/// unpriced-for-this-class — an honest absence, distinct from a rate of zero.
///
/// The hazard this fences is the repair someone would reach for: synthesising
/// a base rate from the tier rate, or dropping the tier because it looks
/// orphaned. The first invents a price the provider never published; the
/// second discards one they did.
#[test]
fn a_tier_rate_without_a_base_rate_of_the_same_class_survives() {
    let payload = r#"{
      "acme": {
        "id": "acme",
        "name": "Acme",
        "models": {
          "m1": {
            "id": "m1",
            "name": "M1",
            "cost": {
              "input": 5,
              "output": 30,
              "tiers": [
                { "input": 10, "output": 45, "cache_write": 2.5,
                  "tier": { "type": "context", "size": 256000 } }
              ]
            },
            "limit": { "context": 400000, "output": 128000 },
            "modalities": { "input": ["text"], "output": ["text"] }
          }
        }
      }
    }"#;

    let outcome = normalize_models_dev(payload.as_bytes())
        .expect("a tier rate without a base rate must normalize, not refuse");
    let model = outcome
        .catalog
        .models()
        .next()
        .expect("the model must survive");

    let keys: Vec<String> = fact_keys_of(model)
        .into_iter()
        .map(|k| k.as_str().to_string())
        .collect();

    assert!(
        keys.iter()
            .any(|k| k == "rate.cache_write.above_context.256000"),
        "the over-threshold cache-write rate the provider DID publish must be \
         served: dropping it as orphaned discards a real price. Got: {keys:?}"
    );
    assert!(
        !keys.iter().any(|k| k == "rate.cache_write"),
        "and no base cache-write rate may be invented from the tier rate: the \
         provider published none, and an absence is not a zero. Got: {keys:?}"
    );

    // CONTROL: a class that DOES have both keeps both, or the assertions above
    // would pass on a normalizer that dropped every cache rate.
    assert!(
        keys.iter().any(|k| k == "rate.input")
            && keys.iter().any(|k| k == "rate.input.above_context.256000"),
        "control: a class with a base and a tier must carry both: {keys:?}"
    );
}

/// STORAGE holds what the upstream said, and nothing fusiform added.
///
/// # Why this asserts the opposite of what it did yesterday
///
/// This test used to require every stored rate to carry `unit_provenance`. It
/// was wrong, and the cost of it was 17,455 corrupted eras.
///
/// The annotation is fusiform's: models.dev publishes no currency at all, so
/// "USD under policy models-dev-usd-v1" is a statement about this code. The
/// serve path attaches it to every priced rate that lacks it, so consumers get
/// it regardless. Writing it into storage as well was a second path to the
/// same outcome — and a second path is not redundancy, it is a second thing
/// that can be wrong.
///
/// It went wrong the day it shipped. The ingest diff compared serialized
/// strings, every stored rate differed textually from every newly normalized
/// one, and the first poll after placement wrote a `boundary_kind = observed`
/// era for every priced rate: the store claiming each provider had moved its
/// price, when only the encoding had changed.
#[test]
fn a_stored_rate_carries_no_annotation_fusiform_invented() {
    let dir = tempfile::tempdir().unwrap();
    let store = fusiform_store::CatalogStore::open(&cortexkit_store_types::StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: cortexkit_store_types::Isolation::Module,
        backend: cortexkit_store_types::StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    })
    .unwrap();
    let catalog = normalize_models_dev(FIXTURE)
        .expect("fixture normalizes")
        .catalog;
    let plan = fusiform_store::ingest::plan_ingest(
        &store,
        &catalog,
        fusiform_core::Timestamp(1_000),
        fusiform_core::BoundaryKind::Seed,
        None,
    )
    .expect("the plan must build");

    let mut checked = 0usize;
    for era in &plan.eras {
        if !era.fact_key.as_str().starts_with("rate.") {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(&era.value_json).expect("a stored rate must be valid JSON");
        if v["state"] != "priced" {
            continue;
        }
        checked += 1;
        assert!(
            v.get("currency").is_some(),
            "a priced rate must still state its currency, which the upstream \
             does not publish but the AMOUNT genuinely carries: {}",
            era.value_json
        );
        assert!(
            v.get("unit_provenance").is_none(),
            "storage must not carry fusiform's own annotation. It is attached \
             at serve time, and writing it here makes every stored rate differ \
             from every newly normalized one the moment the annotation changes: {}",
            era.value_json
        );
    }
    assert!(
        checked >= 2,
        "the fixture must contain priced rates, or this asserts nothing about \
         a population it never found: checked {checked}"
    );
}

/// Every field of `Capabilities` reaches a served fact.
///
/// # The gap this closes, which is in the neighbouring fence rather than here
///
/// `measured_fields_are_read_or_declared` asks whether the PARSER reads each
/// measured upstream field. That is the right question for a field the parser
/// ignores, and it counts a field as handled the moment it lands in a struct —
/// so a field read into the domain and dropped one layer later passes it.
///
/// `temperature` did exactly that until 2026-08-16: parsed from the payload,
/// copied into `Capabilities`, and never turned into a fact. Read, and served
/// to nobody. Worse than unread, because the fence that exists to catch unread
/// fields reported it as handled.
///
/// So this asks the other half: does every field of the domain capability type
/// reach the wire. The two together mean a field cannot hide in the gap
/// between them.
///
/// # Why total destructuring rather than a name list
///
/// A list would need updating and would silently pass when it was not. The
/// destructuring pattern fails to COMPILE when a field is added, which forces
/// the decision at the moment the field appears rather than at review time.
#[test]
fn every_capability_field_reaches_a_fact() {
    let outcome = normalize_models_dev(FIXTURE).expect("fixture normalizes");
    let model = outcome
        .catalog
        .models()
        .next()
        .expect("the fixture must produce a model");

    // Total destructuring: adding a field to `Capabilities` breaks this line,
    // and the fix is to decide whether the new field is served or declared
    // unread — not to add it here and move on.
    let fusiform_core::normalize::Capabilities {
        input_modalities,
        output_modalities,
        reasoning,
        reasoning_options,
        tool_call,
        attachment,
    } = &model.capabilities;

    let keys: Vec<String> = fact_keys_of(model)
        .into_iter()
        .map(|k| k.as_str().to_string())
        .collect();

    for (present, key) in [
        (input_modalities.is_some(), "capability.input_modalities"),
        (output_modalities.is_some(), "capability.output_modalities"),
        (reasoning.is_some(), "capability.reasoning"),
        (reasoning_options.is_some(), "capability.reasoning_options"),
        (tool_call.is_some(), "capability.tool_call"),
        (attachment.is_some(), "capability.attachment"),
    ] {
        assert!(
            !present || keys.iter().any(|k| k == key),
            "the domain carries a value for {key:?} and no fact reaches the \
             wire: a field read into the domain and dropped is invisible to \
             the parser-side fence, which counts it as handled. Keys: {keys:?}"
        );
    }

    assert!(
        keys.iter().filter(|k| k.starts_with("capability.")).count() >= 3,
        "the fixture must exercise several capabilities, or this asserts \
         nothing about a population it never found: {keys:?}"
    );
}

/// Every field of `Limits` reaches a served fact.
///
/// The sibling of `every_capability_field_reaches_a_fact`, and it completes
/// the third layer rather than adding a new idea. Three boundaries exist
/// between the payload and the wire:
///
/// ```text
/// payload -> parser   measured_fields_are_read_or_declared
/// raw     -> domain   total destructuring at the conversion sites
/// domain  -> wire     THIS, and the capability sibling
/// ```
///
/// Both of the fields that died unnoticed this week died in a layer that had
/// no fence: `temperature` between domain and wire, `limit.input` between raw
/// and domain. Fencing two of three layers for one of the domain types leaves
/// the same gap in a different shape, which is the failure mode of fixing an
/// instance rather than a class.
///
/// Total destructuring for the same reason as the sibling: a field added to
/// `Limits` must fail to COMPILE, so the decision happens when the field
/// appears rather than at review time.
#[test]
fn every_limit_field_reaches_a_fact() {
    let outcome = normalize_models_dev(FIXTURE).expect("fixture normalizes");
    let model = outcome
        .catalog
        .models()
        .next()
        .expect("the fixture must produce a model");

    let fusiform_core::normalize::Limits {
        context_tokens,
        output_tokens,
    } = &model.limits;

    let keys: Vec<String> = fact_keys_of(model)
        .into_iter()
        .map(|k| k.as_str().to_string())
        .collect();

    for (present, key) in [
        (context_tokens.is_some(), "limit.context"),
        (output_tokens.is_some(), "limit.output"),
    ] {
        assert!(
            !present || keys.iter().any(|k| k == key),
            "the domain carries a value for {key:?} and no fact reaches the \
             wire. A field read into the domain and dropped is invisible to \
             the parser-side fence, which counts it as handled. Keys: {keys:?}"
        );
    }

    assert!(
        keys.iter().any(|k| k.starts_with("limit.")),
        "the fixture must exercise limits, or this asserts nothing about a \
         population it never found: {keys:?}"
    );
}

/// Every field on `NormalizedModel` either reaches a fact or is named here as
/// deliberately not reaching one.
///
/// # The gap this closes, and how it was found
///
/// There are three boundaries a field crosses: payload -> parser,
/// raw -> domain, domain -> fact. The first two are fenced. The third was
/// fenced only for `Capabilities` and `Limits` — the two nested structs — and
/// `NormalizedModel`'s OWN fields had nothing.
///
/// That gap is invisible from the parser-side fence by construction: it counts
/// a field as handled once the parser touches it, and a field read into the
/// domain and then dropped has been touched. So `family` and `open_weights`
/// sat in the domain for months, passing every check, reaching nothing.
///
/// They were found because a consumer asked for a capability that needed them,
/// not because anything here objected. A fence that waits for a consumer to
/// notice is not a fence.
///
/// # Why total destructuring rather than a list of names
///
/// A list goes stale silently when a field is added. Destructuring makes the
/// COMPILER refuse to build until the new field is named in one of the two
/// arms below — the same mechanism `Capabilities` and `Limits` already use.
#[test]
fn every_domain_field_reaches_a_fact_or_is_declared_unread() {
    let outcome = normalize_models_dev(FIXTURE).expect("fixture normalizes");
    let model = outcome
        .catalog
        .models()
        .next()
        .expect("the fixture must produce a model");

    // Total destructuring: adding a field to NormalizedModel fails to compile
    // here until it is classified.
    let fusiform_core::normalize::NormalizedModel {
        // Reaches a fact.
        key: _,
        family,
        open_weights,
        capabilities: _,
        limits: _,
        rates: _,

        // Declared unread, with the reason each is not served.
        //
        // `display_name` — prose for humans. Nothing branches on it, and a
        // consumer rendering a name should use the id it queried by, which is
        // stable, rather than a label the upstream may reword.
        display_name: _,
        // `release_date` — the MODEL's release, identical across every
        // provider serving it. Measured 2026-09-06 while testing whether the
        // creator of an open-weight family is derivable: it is not, precisely
        // because this field says nothing about WHO is serving. Advisory at
        // best and misleading at worst, since a reseller's row carries the
        // originator's date.
        release_date: _,
        // `last_updated` — upstream's own edit stamp, deliberately not served.
        // Measured on day one: prices changed 20-40% across three models while
        // this field did not move, so it cannot drive change detection and
        // serving it would invite a consumer to try.
        last_updated: _,
        // `knowledge_cutoff` — a property of the training data rather than of
        // the offering. Nothing in the served contract branches on it, and it
        // is not a capacity, a price, or a capability.
        knowledge_cutoff: _,
        // Recorded so an operator can see these exist; never served. Serving a
        // renderer-selecting field would invite a consumer to select a renderer
        // from the catalog, which is the one thing this catalog must not decide.
        quarantined: _,
    } = model;

    let keys: Vec<String> = fact_keys_of(model)
        .into_iter()
        .map(|k| k.as_str().to_string())
        .collect();

    for (present, key) in [
        (family.is_some(), "model.family"),
        (open_weights.is_some(), "model.open_weights"),
    ] {
        assert!(
            !present || keys.iter().any(|k| k == key),
            "the domain carries a value for {key:?} and no fact reaches the \
             wire. This is the shape that hid `family` and `open_weights` for \
             months: the parser-side fence counts a touched field as handled, \
             so a field read into the domain and dropped is invisible to it. \
             Keys: {keys:?}"
        );
    }

    assert!(
        family.is_some() || open_weights.is_some(),
        "control: the fixture must carry at least one of these, or the loop \
         above asserts nothing and passes against a build that emits no model \
         facts at all"
    );
}
