//! Ingest behaviour, driven by the real normalizer over real upstream bytes.
//!
//! The catalogs here are produced by `normalize_models_dev` from the fixture
//! excerpt cut from a live fetch, then mutated at the JSON level to simulate an
//! upstream change. Nothing constructs a `NormalizedModel` by hand: a
//! hand-built catalog would encode this test author's belief about what the
//! normalizer produces, and the assertions would then certify that belief
//! rather than the code.

use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, FactKey, NewObservation};

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_testkit::mutate;

const FIXTURE: &str = include_str!("../../fusiform-core/fixtures/models-dev-excerpt.json");

struct Fixture {
    store: CatalogStore,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let descriptor = StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_string_lossy().to_string(),
        },
    };
    Fixture {
        store: CatalogStore::open(&descriptor).unwrap(),
        _dir: dir,
    }
}

fn observe(f: &Fixture, at: i64, outcome: ObservationOutcome) -> i64 {
    f.store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(at),
            outcome,
            normalized_hash: Some(format!("h{at}")),
            raw_hash: Some(format!("r{at}")),
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap()
}

/// Seed the store from the fixture as it stands, returning the era count.
fn seed(f: &Fixture, at: i64) -> usize {
    let catalog = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(&f.store, &catalog, Timestamp(at), BoundaryKind::Seed, None).unwrap();
    f.store.append_eras(&plan.eras).unwrap()
}

/// A first ingest writes every fact and counts nothing as a change.
#[test]
fn a_first_ingest_records_every_fact_as_new() {
    let f = fixture();
    let catalog = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;

    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(1_000),
        BoundaryKind::Seed,
        None,
    )
    .unwrap();

    // Counted from the catalog rather than hardcoded: the fixture grows when a
    // new upstream shape needs covering, and a literal here turns that into an
    // unrelated test failure that teaches nothing.
    let model_count = catalog.model_count();
    assert!(model_count > 10, "the fixture must be substantial");

    assert_eq!(
        plan.new_models, model_count,
        "every model in the fixture is new"
    );
    assert_eq!(plan.disappeared_models, 0);
    assert_eq!(
        plan.changed_facts, 0,
        "a first ingest has no changes; its facts are first values, and \
         counting them as changes makes a fresh install look like a repricing"
    );
    assert!(
        plan.eras.len() > model_count,
        "each model contributes several facts"
    );

    f.store.append_eras(&plan.eras).unwrap();
}

/// Re-ingesting an unchanged document produces nothing.
///
/// The property that makes the era table finite: polling every 30 minutes for a
/// year must not write 17,520 identical rows per fact.
#[test]
fn re_ingesting_the_same_document_writes_nothing() {
    let f = fixture();
    seed(&f, 1_000);

    observe(&f, 2_000, ObservationOutcome::Unchanged);
    let catalog = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(3_000),
        BoundaryKind::Observed,
        None,
    )
    .unwrap();

    assert!(
        plan.is_empty(),
        "an unchanged document must produce no eras, got {}",
        plan.eras.len()
    );
    assert_eq!(plan.new_models, 0);
    assert_eq!(plan.changed_facts, 0);
    assert_eq!(plan.disappeared_models, 0);
}

/// One rate moving opens exactly one era.
///
/// The granularity claim, tested rather than asserted: if a model were one
/// fact, this would open an era for every field the model publishes and a
/// consumer asking when the input price moved would get the answer for "when
/// did anything about this model move".
#[test]
fn one_rate_change_opens_exactly_one_era() {
    let f = fixture();
    seed(&f, 1_000);
    observe(&f, 2_000, ObservationOutcome::Unchanged);
    let detecting = observe(&f, 3_000, ObservationOutcome::Changed { snapshot_seq: 1 });

    // Anthropic's input rate goes from 3 to 4.
    let mutated = mutate(FIXTURE, "\"input\": 3,", "\"input\": 4,");
    assert_ne!(mutated, FIXTURE, "the mutation must actually apply");

    let catalog = normalize_models_dev(mutated.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(3_000),
        BoundaryKind::Observed,
        Some(detecting),
    )
    .unwrap();

    assert_eq!(plan.changed_facts, 1, "exactly one fact moved");
    assert_eq!(plan.eras.len(), 1);
    assert_eq!(
        plan.eras[0].fact_key,
        FactKey::rate(fusiform_core::TokenClass::Input)
    );
    assert_eq!(plan.eras[0].provider_id, "anthropic");

    f.store.append_eras(&plan.eras).unwrap();

    // And the history is now readable at both instants.
    let read = |at: i64| {
        f.store
            .value_at(
                SourceId::ModelsDev,
                "anthropic",
                "claude-sonnet-4-5",
                &FactKey::rate(fusiform_core::TokenClass::Input),
                Timestamp(at),
            )
            .unwrap()
            .known()
            .unwrap()
            .value_json
            .clone()
    };
    assert!(
        read(2_500).contains("3000000000"),
        "the old rate is retrievable"
    );
    assert!(
        read(3_500).contains("4000000000"),
        "the new rate is in force"
    );
}

/// A model disappearing writes a tombstone, never a deletion.
///
/// The distinction is visible at read time: a deleted row makes a
/// point-in-time query for an instant when the model DID exist return nothing,
/// which reads identically to "never existed".
#[test]
fn a_disappeared_model_is_tombstoned_and_stays_readable() {
    let f = fixture();
    seed(&f, 1_000);
    observe(&f, 2_000, ObservationOutcome::Unchanged);
    let detecting = observe(&f, 3_000, ObservationOutcome::Changed { snapshot_seq: 1 });

    // Remove the zhipuai provider's only model from the document.
    let doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let mut doc = doc.as_object().unwrap().clone();
    let zhipu = doc.get_mut("zhipuai").unwrap().as_object_mut().unwrap();
    zhipu.insert("models".to_string(), serde_json::json!({}));
    let mutated = serde_json::to_string(&doc).unwrap();

    let catalog = normalize_models_dev(mutated.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(3_000),
        BoundaryKind::Observed,
        Some(detecting),
    )
    .unwrap();

    assert_eq!(plan.disappeared_models, 1);
    assert_eq!(plan.eras.len(), 1);
    assert_eq!(plan.eras[0].fact_key, FactKey::existence());
    assert!(plan.eras[0].value_json.contains("absent"));

    f.store.append_eras(&plan.eras).unwrap();

    // The rate the model used to publish is still retrievable at the instant it
    // was true. This is what a deletion would have destroyed.
    let old = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "zhipuai",
            "glm-4.5-flash",
            &FactKey::rate(fusiform_core::TokenClass::Input),
            Timestamp(2_500),
        )
        .unwrap();
    assert!(
        old.known().is_some(),
        "a tombstoned model's history must survive its disappearance"
    );

    // And existence reads correctly on both sides of the boundary.
    let existence = |at: i64| {
        f.store
            .value_at(
                SourceId::ModelsDev,
                "zhipuai",
                "glm-4.5-flash",
                &FactKey::existence(),
                Timestamp(at),
            )
            .unwrap()
            .known()
            .unwrap()
            .value_json
            .clone()
    };
    assert!(existence(2_500).contains("present"));
    assert!(existence(3_500).contains("absent"));
}

/// A model that disappears and comes back gets two existence eras, not one.
#[test]
fn a_returning_model_opens_a_new_existence_era() {
    let f = fixture();
    seed(&f, 1_000);
    observe(&f, 2_000, ObservationOutcome::Unchanged);
    let gone = observe(&f, 3_000, ObservationOutcome::Changed { snapshot_seq: 1 });

    let doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let mut without = doc.as_object().unwrap().clone();
    without
        .get_mut("zhipuai")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("models".to_string(), serde_json::json!({}));
    let removed = serde_json::to_string(&without).unwrap();

    let catalog = normalize_models_dev(removed.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(3_000),
        BoundaryKind::Observed,
        Some(gone),
    )
    .unwrap();
    f.store.append_eras(&plan.eras).unwrap();

    // It comes back, unchanged from its original form.
    observe(&f, 4_000, ObservationOutcome::Unchanged);
    let back = observe(&f, 5_000, ObservationOutcome::Changed { snapshot_seq: 2 });
    let catalog = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(5_000),
        BoundaryKind::Observed,
        Some(back),
    )
    .unwrap();

    assert_eq!(plan.new_models, 1, "a returning model is new again");
    let existence_eras = plan
        .eras
        .iter()
        .filter(|e| e.fact_key == FactKey::existence())
        .count();
    assert_eq!(existence_eras, 1);
    f.store.append_eras(&plan.eras).unwrap();

    // Three existence eras now: present, absent, present. The middle one is
    // what a delete-and-reinsert design would have lost.
    let existence = |at: i64| {
        f.store
            .value_at(
                SourceId::ModelsDev,
                "zhipuai",
                "glm-4.5-flash",
                &FactKey::existence(),
                Timestamp(at),
            )
            .unwrap()
            .known()
            .unwrap()
            .value_json
            .clone()
    };
    assert!(existence(1_500).contains("present"));
    assert!(existence(3_500).contains("absent"));
    assert!(existence(5_500).contains("present"));
}

/// Reordering the upstream's modality array is not a capability change.
///
/// The array carries no ranking, so treating its order as information would
/// manufacture change events on every reshuffle — and a consumer woken for a
/// non-change learns to ignore the channel.
#[test]
fn reordering_a_modality_array_is_not_a_change() {
    let f = fixture();
    seed(&f, 1_000);
    observe(&f, 2_000, ObservationOutcome::Unchanged);

    // Reordered through the PARSED form rather than by text substitution.
    //
    // The fixture is pretty-printed, so `["text","image","pdf"]` does not occur
    // in it as a string and a text replace is silently a no-op -- which would
    // leave this test asserting that an UNCHANGED document produces no eras.
    // True, and nothing to do with ordering.
    //
    // This was previously written as a text replace with a parsed-form
    // fallback, so it worked; the guard is that reversing the parsed array
    // cannot no-op silently, because a wrong path panics rather than passing.
    let mutated = {
        let mut doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let input = doc["anthropic"]["models"]["claude-sonnet-4-5"]["modalities"]["input"]
            .as_array_mut()
            .expect("this model publishes an input modality array");
        assert!(
            input.len() > 1,
            "reordering a one-element array is not a reordering"
        );
        let before = input.clone();
        input.reverse();
        assert_ne!(
            *input, before,
            "the reversal must actually change the order"
        );
        serde_json::to_string(&doc).unwrap()
    };

    let catalog = normalize_models_dev(mutated.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(3_000),
        BoundaryKind::Observed,
        None,
    )
    .unwrap();

    assert!(
        plan.is_empty(),
        "a reordered modality array must not open an era, got {:?}",
        plan.eras
            .iter()
            .map(|e| e.fact_key.as_str())
            .collect::<Vec<_>>()
    );
}

/// A withdrawn rate is tombstoned, so a stale price never stays current.
///
/// The gap this closes: a diff that only visits facts present in the new
/// document never sees a withdrawn one. The last published value would stay
/// current forever, and fusiform would serve a price the provider had stopped
/// publishing. A stale number is worse than no number, because it is spendable.
///
/// The transition is also the one a spend cap must not miss: "free" becoming
/// "we stopped publishing a price" is a real change, and rendering the two
/// alike would hide it.
#[test]
fn a_withdrawn_rate_is_tombstoned_rather_than_left_current() {
    let f = fixture();
    seed(&f, 1_000);
    observe(&f, 2_000, ObservationOutcome::Unchanged);
    let detecting = observe(&f, 3_000, ObservationOutcome::Changed { snapshot_seq: 1 });

    // zhipuai/glm-4.5-flash publishes all-zero rates. Remove the input rate
    // entirely: the fact goes from a stated zero to not being published.
    let doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let mut doc = doc.as_object().unwrap().clone();
    doc.get_mut("zhipuai")
        .unwrap()
        .get_mut("models")
        .unwrap()
        .get_mut("glm-4.5-flash")
        .unwrap()
        .get_mut("cost")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("input");
    let mutated = serde_json::to_string(&doc).unwrap();

    let catalog = normalize_models_dev(mutated.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(3_000),
        BoundaryKind::Observed,
        Some(detecting),
    )
    .unwrap();

    // The withdrawal is a change: exactly one fact moved.
    assert_eq!(plan.changed_facts, 1, "the withdrawn rate is a change");
    assert_eq!(plan.eras.len(), 1);
    assert_eq!(
        plan.eras[0].fact_key,
        FactKey::rate(fusiform_core::TokenClass::Input)
    );

    f.store.append_eras(&plan.eras).unwrap();

    let read = |at: i64| {
        f.store
            .value_at(
                SourceId::ModelsDev,
                "zhipuai",
                "glm-4.5-flash",
                &FactKey::rate(fusiform_core::TokenClass::Input),
                Timestamp(at),
            )
            .unwrap()
            .known()
            .unwrap()
            .value_json
            .clone()
    };

    // Before the withdrawal: a stated zero, which is a real published price.
    let before = read(2_500);
    assert!(
        before.contains("stated_zero"),
        "the old value must be a stated zero, got {before}"
    );

    // After: unpriced with a reason, never zero and never the stale value.
    let after = read(3_500);
    assert!(
        after.contains("unpriced") && after.contains("missing_rate"),
        "a withdrawn rate must be unpriced with a reason, got {after}"
    );
    assert!(
        !after.contains("stated_zero"),
        "the withdrawn rate must not still read as a stated zero"
    );

    // And a second poll with the rate still absent writes nothing more: the
    // fact was withdrawn once, not once per poll.
    observe(&f, 4_000, ObservationOutcome::Unchanged);
    let again = plan_ingest(
        &f.store,
        &normalize_models_dev(mutated.as_bytes()).unwrap().catalog,
        Timestamp(5_000),
        BoundaryKind::Observed,
        None,
    )
    .unwrap();
    assert!(
        again.is_empty(),
        "a still-absent fact must not be tombstoned again, got {}",
        again.eras.len()
    );
}

/// A withdrawn limit becomes null, not a rate tombstone.
///
/// The withdrawal value depends on what kind of fact withdrew. `null` is the
/// same value the normalizer emits for a limit that was never stated, so a
/// withdrawal and a never-stated field agree instead of being two encodings of
/// one condition.
#[test]
fn a_withdrawn_limit_becomes_null() {
    let f = fixture();
    seed(&f, 1_000);
    observe(&f, 2_000, ObservationOutcome::Unchanged);
    let detecting = observe(&f, 3_000, ObservationOutcome::Changed { snapshot_seq: 1 });

    let doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let mut doc = doc.as_object().unwrap().clone();
    doc.get_mut("anthropic")
        .unwrap()
        .get_mut("models")
        .unwrap()
        .get_mut("claude-sonnet-4-5")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("limit");
    let mutated = serde_json::to_string(&doc).unwrap();

    let catalog = normalize_models_dev(mutated.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(3_000),
        BoundaryKind::Observed,
        Some(detecting),
    )
    .unwrap();
    f.store.append_eras(&plan.eras).unwrap();

    let after = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::limit("context"),
            Timestamp(3_500),
        )
        .unwrap()
        .known()
        .unwrap()
        .value_json
        .clone();
    assert_eq!(
        after, "null",
        "a withdrawn limit is null, not a rate tombstone"
    );

    // The old capacity is still retrievable at the instant it was true.
    let before = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::limit("context"),
            Timestamp(2_500),
        )
        .unwrap()
        .known()
        .unwrap()
        .value_json
        .clone();
    assert_eq!(before, "1000000");
}

/// Tiered rates get their own keys, so adding a tier does not rewrite the base
/// rate's history.
#[test]
fn a_tiered_rate_has_its_own_fact_key() {
    let f = fixture();
    seed(&f, 1_000);

    // gpt-5.6-luna publishes a base rate and a 272k tier.
    let base = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "openai",
            "gpt-5.6-luna",
            &FactKey::rate(fusiform_core::TokenClass::Input),
            Timestamp(2_000),
        )
        .unwrap()
        .known()
        .cloned()
        .expect("the base rate has its own era");
    let tiered = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "openai",
            "gpt-5.6-luna",
            &FactKey::rate_above_context(fusiform_core::TokenClass::Input, 272_000),
            Timestamp(2_000),
        )
        .unwrap()
        .known()
        .cloned()
        .expect("the tiered rate has its own era");

    assert_ne!(
        base.value_json, tiered.value_json,
        "the two rates are different values under different keys"
    );
    assert!(base.value_json.contains("200000000"));
    assert!(tiered.value_json.contains("400000000"));
}

/// The digest changes exactly when the diff produces eras.
///
/// This is the property that makes the digest usable as a change signal: it and
/// the era set come from one function, so they cannot disagree. A digest
/// computed independently would drift silently — telling a consumer nothing
/// changed while eras were written, or waking it for a change that produced
/// none.
#[test]
fn the_digest_moves_exactly_when_the_diff_does() {
    use fusiform_store::ingest::catalog_digest;

    let f = fixture();
    seed(&f, 1_000);
    observe(&f, 2_000, ObservationOutcome::Unchanged);

    let base = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;
    let base_digest = catalog_digest(&base);

    // A reserialization: different bytes, same facts. The diff writes nothing,
    // and the digest must not move.
    let doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let reserialized = serde_json::to_vec(&doc).unwrap();
    assert_ne!(
        reserialized,
        FIXTURE.as_bytes(),
        "the round trip must change bytes"
    );
    let same = normalize_models_dev(&reserialized).unwrap().catalog;

    assert_eq!(
        catalog_digest(&same),
        base_digest,
        "a reserialized document must not move the digest"
    );
    let plan = plan_ingest(
        &f.store,
        &same,
        Timestamp(3_000),
        BoundaryKind::Observed,
        None,
    )
    .unwrap();
    assert!(plan.is_empty(), "and it must not produce eras either");

    // A real change: one rate moves. Both must move together.
    let mutated = mutate(FIXTURE, "\"input\": 3,", "\"input\": 4,");
    assert_ne!(mutated, FIXTURE, "the mutation must apply");
    let changed = normalize_models_dev(mutated.as_bytes()).unwrap().catalog;

    assert_ne!(
        catalog_digest(&changed),
        base_digest,
        "a changed rate must move the digest"
    );
    let plan = plan_ingest(
        &f.store,
        &changed,
        Timestamp(3_000),
        BoundaryKind::Observed,
        None,
    )
    .unwrap();
    assert_eq!(plan.eras.len(), 1, "and it must produce exactly one era");
}

/// The digest ignores fields fusiform does not serve.
///
/// A renderer-selecting field changing is a real upstream event and an operator
/// may want to see it, but it is not a change to anything fusiform serves.
/// Waking every consumer for it would train them to ignore the channel.
#[test]
fn the_digest_ignores_quarantined_fields() {
    use fusiform_store::ingest::catalog_digest;

    let base = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;
    let before = catalog_digest(&base);

    // Change the provider's SDK adapter — the single most renderer-selecting
    // fact in the document.
    let doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let mut doc = doc.as_object().unwrap().clone();
    doc.get_mut("anthropic")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert(
            "npm".to_string(),
            serde_json::json!("@ai-sdk/something-else"),
        );
    let mutated = serde_json::to_vec(&doc).unwrap();

    let changed = normalize_models_dev(&mutated).unwrap().catalog;
    assert_eq!(
        catalog_digest(&changed),
        before,
        "a renderer-selecting field must not move the served digest"
    );
}

/// Two different fact sets must not hash alike.
///
/// The length-prefix guard. Provider and model ids are upstream-controlled
/// strings, and models.dev model ids routinely contain `/`
/// (`openai/gpt-oss-120b`), so any separator would come from a namespace the
/// upstream can write into a value. Without length prefixes, a provider id
/// that absorbs the start of a model id concatenates identically to the
/// unshifted pair, and two genuinely different catalogs hash the same.
///
/// The two documents below differ only in where the provider/model boundary
/// falls: `ab` + `/c` against `a` + `b/c`. Their concatenations are equal, so
/// this fails without the prefixes.
#[test]
fn the_digest_is_not_confusable_across_field_boundaries() {
    use fusiform_store::ingest::catalog_digest;

    let doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    let template = doc
        .as_object()
        .unwrap()
        .get("anthropic")
        .unwrap()
        .get("models")
        .unwrap()
        .get("claude-sonnet-4-5")
        .unwrap()
        .clone();

    let build = |provider: &str, model: &str| {
        let mut m = template.clone();
        m.as_object_mut()
            .unwrap()
            .insert("id".to_string(), serde_json::json!(model));
        serde_json::json!({
            provider: {
                "id": provider,
                "models": { model: m }
            }
        })
    };

    // "ab" + "/c" and "a" + "b/c" concatenate to the same bytes.
    let left = build("ab", "/c");
    let right = build("a", "b/c");

    let dl = catalog_digest(
        &normalize_models_dev(&serde_json::to_vec(&left).unwrap())
            .unwrap()
            .catalog,
    );
    let dr = catalog_digest(
        &normalize_models_dev(&serde_json::to_vec(&right).unwrap())
            .unwrap()
            .catalog,
    );
    assert_ne!(
        dl, dr,
        "a shifted field boundary produced an identical digest: the fields are \
         being concatenated without length prefixes"
    );
}

/// The digest depends on the fact SET, not on the order it arrives in.
///
/// Today the normalizer emits providers in sorted order, so the sort inside the
/// digest is redundant — which is exactly why this test constructs the
/// disordered case explicitly. A guard that is only correct because of an
/// upstream implementation detail is one refactor away from being wrong, and
/// nothing would fail when it broke.
#[test]
fn the_digest_does_not_depend_on_provider_order() {
    use fusiform_store::ingest::catalog_digest;

    let mut catalog = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;
    let ordered = catalog_digest(&catalog);

    catalog.providers.reverse();
    let reversed = catalog_digest(&catalog);
    assert_eq!(
        ordered, reversed,
        "reversing provider order changed the digest, so it depends on \
         emission order rather than on the fact set"
    );

    // And within a provider: reversing its models must not matter either.
    for provider in &mut catalog.providers {
        provider.models.reverse();
    }
    assert_eq!(
        ordered,
        catalog_digest(&catalog),
        "reversing model order changed the digest"
    );
}

/// A store BEHIND the document converges after one changed poll.
///
/// # Why this test exists, and what it does not establish
///
/// This is the load-bearing half of the argument that REJECTED the
/// content-derived catalog version (`f8eedf0`, reasoning at
/// `advance_catalog_version`). BROCA refuses a refresh whose identity set is
/// smaller than the one they hold, and freeze their comparison basis on
/// refusal — so if a store that has fallen behind could never catch up, that
/// refusal would be permanent and the rejected fix would have been the right
/// one after all.
///
/// I argued from the diff's design that it cannot be permanent: `plan_ingest`
/// visits every fact in the DOCUMENT as well as every fact in the STORE, so a
/// model the store has never seen is an arrival like any other, and one
/// changed poll installs it. That reasoning was never driven. This drives it.
///
/// WHAT THIS IS NOT: a restore. Nobody has restored a fusiform store and
/// polled it, and a truncated copy would be my guess at what engram leaves
/// behind — a synthetic reproduction of an event nobody has observed tests the
/// model of the event and reports as if it tested the event (BROCA's form).
/// What this establishes is narrower and is the part the argument actually
/// used: THE DIFF HAS NO MEMORY OF HAVING BEEN BEHIND. A store missing a model
/// converges on the next changed poll regardless of why it was missing.
#[test]
fn a_store_behind_the_document_catches_up_in_one_changed_poll() {
    let f = fixture();

    // Seed from a document with one model REMOVED: a store that is behind,
    // however it got that way.
    let mut behind: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    behind["anthropic"]["models"]
        .as_object_mut()
        .expect("the fixture's anthropic provider publishes models")
        .remove("claude-sonnet-4-5")
        .expect("the model this test removes must be in the fixture");
    let behind_doc = serde_json::to_string(&behind).unwrap();

    let catalog = normalize_models_dev(behind_doc.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(1_000),
        BoundaryKind::Seed,
        None,
    )
    .unwrap();
    f.store.append_eras(&plan.eras).unwrap();

    // The control. Without it, the convergence below is consistent with the
    // model having been present all along, and this test would assert nothing.
    let missing = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::existence(),
            Timestamp(1_500),
        )
        .unwrap();
    assert!(
        missing.known().is_none(),
        "control: the store must genuinely not hold this model, or the \
         convergence assertion proves nothing"
    );

    // One changed poll against the FULL document.
    //
    // The confirming observation first: the store refuses an observed boundary
    // with no prior confirming poll, because such a boundary would carry a
    // window it cannot bound. That refusal caught the first version of this
    // test, which is the invariant working rather than an inconvenience.
    observe(&f, 1_500, ObservationOutcome::Unchanged);
    let detecting = observe(&f, 2_000, ObservationOutcome::Changed { snapshot_seq: 1 });
    let full = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(
        &f.store,
        &full,
        Timestamp(2_000),
        BoundaryKind::Observed,
        Some(detecting),
    )
    .unwrap();

    assert_eq!(
        plan.new_models, 1,
        "the model the store lacks must be counted as an ARRIVAL: the diff \
         reads the document as well as the store, so it has no memory of \
         having been behind"
    );
    f.store.append_eras(&plan.eras).unwrap();

    let now = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::existence(),
            Timestamp(2_500),
        )
        .unwrap();
    assert_eq!(
        now.known().map(|r| r.value_json.as_str()),
        Some("\"present\""),
        "one changed poll must close the gap"
    );

    // And the identity set is no smaller than it was: convergence must not be
    // achieved by dropping something else. This is the property BROCA's
    // completeness guard actually compares.
    let all = f
        .store
        .read_catalog(&fusiform_store::serve::CatalogQuery::current(
            SourceId::ModelsDev,
        ))
        .unwrap();
    assert_eq!(
        all.models.len(),
        full.model_count(),
        "after catching up, the store must describe every model the document \
         does — a refusal that never clears is what the rejected fix existed \
         to prevent"
    );
}

/// The limit of the claim above, stated so nobody reads it as more.
///
/// Convergence is toward the DOCUMENT, not toward whatever a consumer happens
/// to hold. A model that has left the upstream does not come back, and a
/// consumer holding it will keep finding it absent from the present tense
/// forever — correctly, because it is absent.
///
/// This matters because the convergence argument is easy to over-read as "any
/// consumer refusal clears itself". It clears a refusal caused by the store
/// being BEHIND. It does not clear one caused by the upstream having
/// genuinely retired something the consumer still lists.
#[test]
fn convergence_is_toward_the_document_not_toward_a_consumer() {
    let f = fixture();
    seed(&f, 1_000);
    observe(&f, 1_500, ObservationOutcome::Unchanged);

    let detecting = observe(&f, 2_000, ObservationOutcome::Changed { snapshot_seq: 1 });
    let mut retired: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    retired["anthropic"]["models"]
        .as_object_mut()
        .unwrap()
        .remove("claude-sonnet-4-5")
        .expect("the model this test retires must be in the fixture");
    let catalog = normalize_models_dev(serde_json::to_string(&retired).unwrap().as_bytes())
        .unwrap()
        .catalog;
    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(2_000),
        BoundaryKind::Observed,
        Some(detecting),
    )
    .unwrap();
    assert_eq!(
        plan.disappeared_models, 1,
        "control: the retirement happened"
    );
    f.store.append_eras(&plan.eras).unwrap();

    // A second changed poll against the same reduced document adds nothing
    // back. Convergence has a direction.
    let detecting = observe(&f, 3_000, ObservationOutcome::Changed { snapshot_seq: 2 });
    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(3_000),
        BoundaryKind::Observed,
        Some(detecting),
    )
    .unwrap();
    assert_eq!(
        plan.new_models, 0,
        "polling again must not resurrect a retired model: the store converges \
         on what the upstream publishes, not on what a consumer remembers"
    );

    // It stays readable as history, which is the distinction that makes the
    // refusal survivable: the fact is recoverable even though the identity is
    // no longer present.
    let then = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::existence(),
            Timestamp(1_500),
        )
        .unwrap();
    assert_eq!(
        then.known().map(|r| r.value_json.as_str()),
        Some("\"present\""),
        "the retired model's history must survive the retirement"
    );
}

/// A change to fusiform's own serialization must not open an era.
///
/// # The production incident this pins
///
/// On 2026-08-16 a binary shipped that added `unit_provenance` to the stored
/// rate representation. The ingest diff compared serialized strings, so every
/// stored rate differed from every freshly normalized one, and the first poll
/// after placement wrote 17,455 eras — one per priced rate — each with
/// `boundary_kind = observed` and an observation window implying the provider
/// had moved its price.
///
/// Nothing moved. `anthropic/claude-sonnet-4-5` `rate.input` reads
/// `units: 3000000000` on both sides of that boundary. A store whose whole
/// purpose is recording what the upstream said recorded fusiform's own
/// encoding change as an upstream event, and `normalized_hash` moved with it,
/// which is the signal consumers watch.
///
/// # What the test asserts
///
/// An era boundary is a claim about the SOURCE. A stored value carrying an
/// annotation fusiform added must compare equal to the same value without it,
/// so re-ingesting an unchanged catalog against an annotated store writes
/// nothing.
#[test]
fn an_annotation_fusiform_added_does_not_open_an_era() {
    let f = fixture();
    let store = &f.store;

    let catalog = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;
    let obs = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1_000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some("h1".into()),
            raw_hash: Some("r1".into()),
            etag: None,
            duration_ms: Some(5),
            detail: None,
        })
        .unwrap();
    let _ = obs;
    let plan = plan_ingest(store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();

    // A stored rate WITH the annotation, written directly. This is the LIVE
    // population and the direction that matters for deployment: 17,455 rows
    // carry `unit_provenance` because a binary wrote it into storage for one
    // day, and the fixed binary no longer emits it.
    //
    // So the next poll after the fix is placed compares stored-with against
    // incoming-without. If the comparison were not symmetric about the
    // annotation, placing the FIX would cause a second mass rewrite — worse
    // than the first, because the repair would have caused it.
    //
    // This test previously stored a row WITHOUT the annotation, which was the
    // right direction while storage still emitted it. After that write was
    // removed, both sides lacked the field, the strings matched exactly, and
    // the test passed without exercising the comparison at all.
    let key = fusiform_store::FactKey::rate(fusiform_core::TokenClass::Input);
    store
        .append_eras(&[fusiform_store::NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: key.clone(),
            value_json: r#"{"state":"priced","units":3000000000,"exponent":9,"currency":"USD","unit_provenance":{"kind":"assumed_by_policy","policy":"models-dev-usd-v1"}}"#
                .to_string(),
            boundary_at: Timestamp(1_500),
            boundary_kind: BoundaryKind::Observed,
            observation_id: None,
        }])
        .expect("the pre-annotation row must store");

    // Re-ingest the SAME catalog. The upstream said nothing new.
    let obs2 = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(2_000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 2 },
            normalized_hash: Some("h2".into()),
            raw_hash: Some("r2".into()),
            etag: None,
            duration_ms: Some(5),
            detail: None,
        })
        .unwrap();
    let plan2 = plan_ingest(
        store,
        &catalog,
        Timestamp(2_000),
        BoundaryKind::Observed,
        Some(obs2),
    )
    .unwrap();

    let rate_eras = plan2
        .eras
        .iter()
        .filter(|e| {
            e.fact_key == key && e.provider_id == "anthropic" && e.model_id == "claude-sonnet-4-5"
        })
        .count();
    assert_eq!(
        rate_eras, 0,
        "re-ingesting an unchanged catalog against rows lacking a fusiform \
         annotation must write NO rate eras. Each one would claim the provider \
         changed its price at this instant, in a history that exists to say \
         what the provider did. Wrote {rate_eras}."
    );

    // CONTROL: a real value change must still open an era, or the comparison
    // above has been made blind rather than precise.
    let moved = {
        let mut doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        doc["anthropic"]["models"]["claude-sonnet-4-5"]["cost"]["input"] = serde_json::json!(99.0);
        serde_json::to_vec(&doc).unwrap()
    };
    let moved_catalog = normalize_models_dev(&moved).unwrap().catalog;
    let plan3 = plan_ingest(
        store,
        &moved_catalog,
        Timestamp(3_000),
        BoundaryKind::Observed,
        None,
    )
    .unwrap();
    assert!(
        plan3
            .eras
            .iter()
            .any(|e| e.fact_key.as_str() == "rate.input"),
        "control: a genuine rate change must still open an era"
    );
}
