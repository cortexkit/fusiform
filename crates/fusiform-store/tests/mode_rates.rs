//! Mode rates: models.dev `experimental.modes.<name>.cost`, stored as
//! `rate.<class>.mode.<name>`.
//!
//! A mode fuses a rate schedule with the literal request bytes that switch the
//! mode on. These tests pin that the rates are stored exactly as the mode
//! publishes them, that nothing is filled in from the base rate, that a mode
//! fusiform cannot read is refused and reported rather than half-served, and
//! that the bytes reach nothing.

use std::collections::BTreeMap;

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::{normalize_models_dev, NormalizeOutcome, NormalizedModel};
use fusiform_core::{BoundaryKind, NormalizeError, ObservationOutcome, SourceId, Timestamp};
use fusiform_store::ingest::{catalog_digest, facts_with_values, plan_ingest};
use fusiform_store::{CatalogStore, NewObservation};

/// A synthetic document covering each mode shape the live payload carries,
/// plus the two broken shapes it does not (yet).
///
/// - `with-base`: a base price card, a `fast` mode pricing input, output and a
///   cache read of 0 (so a stated zero), and a `pro` mode with no cost. The
///   base prices cache_write and `fast` does not.
/// - `no-base`: modes and no base `cost`, as five live rows publish.
/// - `refusals`: a mode whose name cannot form a fact key, a mode whose cost
///   has a non-numeric value, and a good `priority` mode beside them.
const DOC: &str = r#"{
  "acme": {
    "id": "acme",
    "name": "Acme (synthetic)",
    "models": {
      "with-base": {
        "id": "with-base",
        "cost": { "input": 2, "output": 8, "cache_read": 0.2, "cache_write": 2.5 },
        "experimental": {
          "modes": {
            "fast": {
              "cost": { "input": 4, "output": 16, "cache_read": 0 },
              "provider": { "body": { "service_tier": "priority" } }
            },
            "pro": {
              "provider": { "body": { "reasoning": { "mode": "pro" } } }
            }
          }
        }
      },
      "no-base": {
        "id": "no-base",
        "experimental": {
          "modes": {
            "fast": {
              "cost": { "input": 30, "output": 150 },
              "provider": {
                "body": { "speed": "fast" },
                "headers": { "anthropic-beta": "fast-mode-2026-02-01" }
              }
            }
          }
        }
      },
      "refusals": {
        "id": "refusals",
        "cost": { "input": 1, "output": 2 },
        "experimental": {
          "modes": {
            "Fast Tier": { "cost": { "input": 5, "output": 10 } },
            "broken": { "cost": { "input": "five", "output": 10 } },
            "priority": { "cost": { "input": 3, "output": 6 } }
          }
        }
      }
    }
  }
}"#;

fn normalized() -> NormalizeOutcome {
    normalize_models_dev(DOC.as_bytes()).expect("a document with refused modes still normalizes")
}

fn model<'a>(outcome: &'a NormalizeOutcome, id: &str) -> &'a NormalizedModel {
    outcome
        .catalog
        .models()
        .find(|m| m.key.model_id == id)
        .unwrap_or_else(|| panic!("{id} must normalize"))
}

/// The rate facts a model stores, key to stored value.
fn rates(model: &NormalizedModel) -> BTreeMap<String, String> {
    facts_with_values(model)
        .into_iter()
        .map(|(k, v)| (k.as_str().to_string(), v))
        .filter(|(k, _)| k.starts_with("rate."))
        .collect()
}

fn priced(units: u64) -> String {
    format!(r#"{{"state":"priced","units":{units},"exponent":9,"currency":"USD"}}"#)
}

/// A model with a base card, a priced mode and an unpriced mode stores exactly
/// the base rates plus the priced mode's dimensions, with the values it
/// published. `pro`, which publishes no cost, stores nothing.
#[test]
fn a_moded_model_stores_its_mode_rates_beside_its_base_rates() {
    let outcome = normalized();
    let got = rates(model(&outcome, "with-base"));

    let expected: BTreeMap<String, String> = [
        ("rate.input", priced(2_000_000_000)),
        ("rate.output", priced(8_000_000_000)),
        ("rate.cache_read", priced(200_000_000)),
        ("rate.cache_write", priced(2_500_000_000)),
        ("rate.input.mode.fast", priced(4_000_000_000)),
        ("rate.output.mode.fast", priced(16_000_000_000)),
        // A published 0 inside a mode is a price of nothing, as for a base
        // rate, and never an absence.
        (
            "rate.cache_read.mode.fast",
            r#"{"state":"stated_zero"}"#.to_string(),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();

    assert_eq!(got, expected);
    assert!(
        !got.keys().any(|k| k.contains(".mode.pro")),
        "a mode with no cost must produce no fact: {got:?}"
    );
}

/// A dimension the mode does not price stays absent, even where the base rate
/// prices it. Filling it from the base would state a price for the mode that
/// the upstream never published.
#[test]
fn a_dimension_the_mode_omits_stays_absent() {
    let outcome = normalized();
    let got = rates(model(&outcome, "with-base"));

    assert!(
        got.contains_key("rate.cache_write"),
        "control: the base prices cache_write"
    );
    assert!(
        !got.contains_key("rate.cache_write.mode.fast"),
        "the fast mode publishes no cache_write price and must serve none: {got:?}"
    );
}

/// A model with modes and no base cost stores only its mode rates.
#[test]
fn a_model_with_no_base_cost_stores_only_mode_rates() {
    let outcome = normalized();
    let got = rates(model(&outcome, "no-base"));

    let expected: BTreeMap<String, String> = [
        ("rate.input.mode.fast", priced(30_000_000_000)),
        ("rate.output.mode.fast", priced(150_000_000_000)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(got, expected);
}

/// A mode whose name cannot form a fact key, and a mode whose cost does not
/// parse, each store nothing and are reported. The model's base rates and its
/// good mode are unaffected, and nothing falls back to the base rate.
#[test]
fn an_unreadable_mode_is_refused_and_reported() {
    let outcome = normalized();
    let got = rates(model(&outcome, "refusals"));

    let expected: BTreeMap<String, String> = [
        ("rate.input", priced(1_000_000_000)),
        ("rate.output", priced(2_000_000_000)),
        ("rate.input.mode.priority", priced(3_000_000_000)),
        ("rate.output.mode.priority", priced(6_000_000_000)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(
        got, expected,
        "only the base card and the readable mode may be stored"
    );

    let refused: Vec<(&str, &str)> = outcome
        .findings
        .iter()
        .filter_map(|f| match f {
            NormalizeError::RefusedMode { model, mode, .. } => {
                Some((model.as_str(), mode.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        refused,
        vec![("acme/refusals", "Fast Tier"), ("acme/refusals", "broken")],
        "each refused mode must be reported exactly once, and a mode with no \
         cost (`pro`) is not a refusal"
    );
}

/// The request bytes a mode carries reach no stored fact, key or value.
#[test]
fn a_modes_request_bytes_are_never_stored() {
    let outcome = normalized();
    for id in ["with-base", "no-base"] {
        for (key, value) in facts_with_values(model(&outcome, id)) {
            for needle in [
                "service_tier",
                "speed",
                "anthropic-beta",
                "fast-mode-2026-02-01",
                "priority\"",
            ] {
                assert!(
                    !key.as_str().contains(needle) && !value.contains(needle),
                    "acme/{id}: {needle:?} reached {} = {value}",
                    key.as_str()
                );
            }
        }
    }
}

const EXCERPT: &str = include_str!("../../fusiform-core/fixtures/models-dev-excerpt.json");

fn store() -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    })
    .unwrap();
    (store, dir)
}

fn observe(store: &CatalogStore, at: i64) {
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(at),
            outcome: ObservationOutcome::Unchanged,
            normalized_hash: Some(format!("h{at}")),
            raw_hash: Some(format!("r{at}")),
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
}

/// The excerpt with one edit applied to gpt-5.6-luna's `fast` mode.
fn with_luna_fast(edit: impl FnOnce(&mut serde_json::Value)) -> String {
    let mut doc: serde_json::Value = serde_json::from_str(EXCERPT).unwrap();
    let fast = &mut doc["openai"]["models"]["gpt-5.6-luna"]["experimental"]["modes"]["fast"];
    assert!(fast.is_object(), "the excerpt must carry luna's fast mode");
    edit(fast);
    serde_json::to_string(&doc).unwrap()
}

/// Re-polling identical modes writes no era; repricing one mode dimension
/// writes exactly one era, on that key; changing only the mode's request bytes
/// writes none and leaves the digest alone.
#[test]
fn ingest_opens_an_era_only_when_a_mode_price_moves() {
    let (store, _dir) = store();
    let seed = normalize_models_dev(EXCERPT.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(&store, &seed, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    assert!(
        plan.eras
            .iter()
            .any(|e| e.fact_key.as_str() == "rate.input.mode.fast"),
        "control: the seed must store luna's fast mode rate"
    );
    store.append_eras(&plan.eras).unwrap();
    observe(&store, 2_000);

    // Identical document: nothing moves.
    let plan = plan_ingest(
        &store,
        &seed,
        Timestamp(3_000),
        BoundaryKind::Observed,
        None,
    )
    .unwrap();
    assert!(
        plan.is_empty(),
        "an identical re-poll must open no era: {:?}",
        plan.eras
            .iter()
            .map(|e| e.fact_key.as_str())
            .collect::<Vec<_>>()
    );

    // Only the bytes that select the mode change. Not a price, so no era and
    // no digest movement.
    let bytes_only = with_luna_fast(|fast| {
        fast["provider"]["body"]["service_tier"] = serde_json::json!("flex");
    });
    let bytes_only = normalize_models_dev(bytes_only.as_bytes()).unwrap().catalog;
    assert_eq!(
        catalog_digest(&bytes_only),
        catalog_digest(&seed),
        "a mode's request bytes must not move the served digest"
    );
    let plan = plan_ingest(
        &store,
        &bytes_only,
        Timestamp(3_000),
        BoundaryKind::Observed,
        None,
    )
    .unwrap();
    assert!(plan.is_empty(), "a mode's request bytes must open no era");

    // One mode price moves.
    let repriced = with_luna_fast(|fast| {
        fast["cost"]["input"] = serde_json::json!(0.5);
    });
    let repriced = normalize_models_dev(repriced.as_bytes()).unwrap().catalog;
    assert_ne!(
        catalog_digest(&repriced),
        catalog_digest(&seed),
        "a mode price change must move the served digest"
    );
    let plan = plan_ingest(
        &store,
        &repriced,
        Timestamp(4_000),
        BoundaryKind::Observed,
        None,
    )
    .unwrap();
    let touched: Vec<(String, String, String)> = plan
        .eras
        .iter()
        .map(|e| {
            (
                e.model_id.clone(),
                e.fact_key.as_str().to_string(),
                e.value_json.clone(),
            )
        })
        .collect();
    assert_eq!(
        touched,
        vec![(
            "gpt-5.6-luna".to_string(),
            "rate.input.mode.fast".to_string(),
            priced(500_000_000),
        )],
        "exactly one era, on the repriced mode key"
    );
}
