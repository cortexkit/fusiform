//! A correction's declared extent must describe the fact it is written on.
//!
//! A correction is two statements of one belief: the ROW says "this fact's
//! recorded value was wrong", and the EXTENT says which fields a consumer
//! should partition on. Each is well formed alone, so nothing catches a
//! correction on `rate.input` whose extent names limits — and a consumer
//! following it partitions the wrong charges while every other check passes.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{
    BoundaryKind, CapabilityId, Correction, FieldId, LimitId, ModelAttributeId, SourceId,
    Timestamp, TokenClass,
};
use fusiform_store::{CatalogStore, FactKey, NewEra};

fn store(dir: &tempfile::TempDir) -> CatalogStore {
    CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    })
    .unwrap()
}

fn correction(fields: Vec<FieldId>) -> BoundaryKind {
    BoundaryKind::Corrected(Correction {
        fields,
        affected_from: Timestamp(1_000),
        affected_until: Timestamp(5_000),
        reason: "a defect".to_string(),
    })
}

fn era(fact: FactKey, kind: BoundaryKind) -> NewEra {
    NewEra {
        source: SourceId::ModelsDev,
        provider_id: "anthropic".into(),
        model_id: "claude-sonnet-4-5".into(),
        fact_key: fact,
        value_json: "1".into(),
        boundary_at: Timestamp(9_000),
        boundary_kind: kind,
        observation_id: None,
    }
}

/// The ordinary case: a correction naming its own fact is accepted.
#[test]
fn a_correction_naming_its_own_fact_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);

    store
        .append_eras(&[era(
            FactKey::rate(TokenClass::Input),
            correction(vec![FieldId::Rate {
                class: TokenClass::Input,
            }]),
        )])
        .expect("a coherent correction must be accepted");
}

/// A correction whose extent describes a different fact is refused.
#[test]
fn a_correction_naming_a_different_fact_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);

    // Written on rate.input, extent names limit.context. Both halves are well
    // formed; together they are incoherent.
    let err = store
        .append_eras(&[era(
            FactKey::rate(TokenClass::Input),
            correction(vec![FieldId::Limit {
                limit: LimitId::Context,
            }]),
        )])
        .expect_err("an extent that omits its own fact must be refused");

    let text = format!("{err}");
    assert!(
        text.contains("rate.input") && text.contains("does not"),
        "the error must name the fact and the mismatch: {text}"
    );
}

/// A correction spanning several facts is accepted on each of them.
///
/// One defect commonly touches more than one fact \u2014 a normalizer bug can
/// misread every rate a model has \u2014 and each row may carry the whole extent.
/// Containment, not equality: what a row may not do is omit its own fact.
#[test]
fn a_multi_fact_correction_is_accepted_on_each_fact_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);

    let fields = vec![
        FieldId::Rate {
            class: TokenClass::Input,
        },
        FieldId::Rate {
            class: TokenClass::Output,
        },
    ];

    store
        .append_eras(&[
            era(FactKey::rate(TokenClass::Input), correction(fields.clone())),
            era(
                FactKey::rate(TokenClass::Output),
                correction(fields.clone()),
            ),
        ])
        .expect("each row names its own fact within a shared extent");

    // And a THIRD fact carrying the same extent is still refused, so the rule
    // is containment rather than "any overlap is fine".
    store
        .append_eras(&[era(
            FactKey::rate(TokenClass::CacheRead),
            correction(fields),
        )])
        .expect_err("a fact outside the extent must still be refused");
}

/// Every served fact can be named by a `FieldId`.
///
/// Found by probing rather than by review: the vocabulary was built for money
/// partitioning, and seven served facts — both limits and all five capabilities
/// — had no `FieldId` at all. A correction to `capability.reasoning` could be
/// WRITTEN, since the store takes a fact key and a field list separately, but
/// it could not name the field it was correcting.
///
/// That matters beyond tidiness: `capability.reasoning` gates a consumer's
/// reasoning policy, so a wrong value changes the bytes of every request to
/// that model. It is among the facts most worth being able to correct.
///
/// Asserts against `SERVED_FACT_NAMESPACE` rather than deriving the served set
/// from `fact_keys_of`. The first version did the latter and silently skipped
/// `existence`, which that function does not produce — a mutation mapping
/// `FieldId::Existence` to the wrong key survived because of it. Two lists of
/// served facts, one belief, and the test was reading the shorter one.
#[test]
fn every_served_fact_has_a_field_id() {
    use fusiform_store::ingest::SERVED_FACT_NAMESPACE;

    // Every fact key a FieldId can name. Exhaustive by construction: a new
    // FieldId variant fails to compile in `FactKey::for_field`.
    let nameable: Vec<String> = [
        FieldId::Existence,
        FieldId::Rate {
            class: TokenClass::Input,
        },
        FieldId::Rate {
            class: TokenClass::Output,
        },
        FieldId::Rate {
            class: TokenClass::CacheRead,
        },
        FieldId::Rate {
            class: TokenClass::CacheWrite,
        },
        FieldId::Rate {
            class: TokenClass::Reasoning,
        },
        FieldId::Limit {
            limit: LimitId::Context,
        },
        FieldId::Limit {
            limit: LimitId::Output,
        },
        FieldId::Capability {
            capability: CapabilityId::Reasoning,
        },
        FieldId::Capability {
            capability: CapabilityId::ReasoningOptions,
        },
        FieldId::Capability {
            capability: CapabilityId::ToolCall,
        },
        FieldId::Capability {
            capability: CapabilityId::Attachment,
        },
        FieldId::Capability {
            capability: CapabilityId::InputModalities,
        },
        FieldId::Capability {
            capability: CapabilityId::OutputModalities,
        },
        FieldId::Model {
            attribute: ModelAttributeId::Family,
        },
        FieldId::Model {
            attribute: ModelAttributeId::OpenWeights,
        },
    ]
    .into_iter()
    .filter_map(FactKey::for_field)
    .map(|k| k.as_str().to_string())
    .collect();

    let orphans: Vec<&&str> = SERVED_FACT_NAMESPACE
        .iter()
        .filter(|k| !nameable.contains(&k.to_string()))
        .collect();

    assert!(
        orphans.is_empty(),
        "these served facts cannot be named by any FieldId, so a correction to \
         them cannot state its own extent: {orphans:?}"
    );
}

/// The closed vocabulary matches what the normalizer actually produces.
///
/// `SERVED_FACT_NAMESPACE` is hand-maintained, so on its own it is a claim
/// rather than a fact. This runs the real normalizer over real upstream bytes
/// and checks the two agree — otherwise the constant could drift and every test
/// asserting against it would keep passing.
#[test]
fn the_closed_vocabulary_matches_the_normalizer() {
    use fusiform_core::normalize::normalize_models_dev;
    use fusiform_store::ingest::{fact_keys_of, SERVED_FACT_NAMESPACE};

    const FIXTURE: &[u8] =
        include_bytes!("../../../fusiform-core/fixtures/models-dev-excerpt.json");
    let outcome = normalize_models_dev(FIXTURE).unwrap();

    let mut produced: Vec<String> = Vec::new();
    for model in outcome.catalog.models() {
        for k in fact_keys_of(model) {
            let k = k.as_str().to_string();
            // Tiered and mode keys carry an upstream threshold or mode name
            // and are not enumerable.
            if !k.contains(".above_context.") && !k.contains(".mode.") {
                produced.push(k);
            }
        }
    }
    produced.sort();
    produced.dedup();

    for key in &produced {
        assert!(
            SERVED_FACT_NAMESPACE.contains(&key.as_str()),
            "the normalizer produces {key:?}, which the closed vocabulary omits"
        );
    }

    // And the reverse, minus `existence`, which plan_ingest writes rather than
    // the normalizer.
    for key in SERVED_FACT_NAMESPACE {
        if *key == "existence" {
            continue;
        }
        assert!(
            produced.contains(&key.to_string()),
            "the closed vocabulary lists {key:?}, which the normalizer never produces"
        );
    }
}

/// A mode rate is correctable: its field id names exactly its fact key.
///
/// A wrong mode price misprices requests made in that mode, so an operator must
/// be able to write a correction for it whose extent the store can check
/// against the row. The wire spelling is pinned too, because the CLI and the
/// route both build or parse it as JSON.
#[test]
fn a_mode_rate_field_names_exactly_its_fact() {
    let field = FieldId::ModeRate {
        class: TokenClass::CacheRead,
        mode: "fast".to_string(),
    };
    assert_eq!(
        FactKey::for_field(field.clone()).map(|k| k.as_str().to_string()),
        Some("rate.cache_read.mode.fast".to_string())
    );
    assert_eq!(
        serde_json::to_value(&field).unwrap(),
        serde_json::json!({"field": "mode_rate", "class": "cache_read", "mode": "fast"})
    );
    // A name the normalizer would refuse names no fact, rather than an odd key.
    assert_eq!(
        FactKey::for_field(FieldId::ModeRate {
            class: TokenClass::Input,
            mode: "Fast.Tier".to_string(),
        }),
        None
    );
}

/// A correction naming nothing is refused.
#[test]
fn a_correction_with_an_empty_extent_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);

    let err = store
        .append_eras(&[era(FactKey::rate(TokenClass::Input), correction(vec![]))])
        .expect_err("an extent naming nothing cannot be partitioned on");
    assert!(format!("{err}").contains("at least one field"));
}

/// A correction cannot cover instants fusiform has not reached.
#[test]
fn a_correction_reaching_past_its_own_recording_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);

    let mut row = era(
        FactKey::rate(TokenClass::Input),
        BoundaryKind::Corrected(Correction {
            fields: vec![FieldId::Rate {
                class: TokenClass::Input,
            }],
            affected_from: Timestamp(1_000),
            // Later than the boundary_at of 9_000 below.
            affected_until: Timestamp(50_000),
            reason: "a defect".to_string(),
        }),
    );
    row.boundary_at = Timestamp(9_000);

    let err = store
        .append_eras(&[row])
        .expect_err("an extent reaching into the future must be refused");
    assert!(format!("{err}").contains("has not reached"));
}

/// A backwards interval is refused.
#[test]
fn a_backwards_correction_interval_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);

    let err = store
        .append_eras(&[era(
            FactKey::rate(TokenClass::Input),
            BoundaryKind::Corrected(Correction {
                fields: vec![FieldId::Rate {
                    class: TokenClass::Input,
                }],
                affected_from: Timestamp(5_000),
                affected_until: Timestamp(1_000),
                reason: "a defect".to_string(),
            }),
        )])
        .expect_err("a backwards interval must be refused");
    assert!(format!("{err}").contains("backwards"));
}
