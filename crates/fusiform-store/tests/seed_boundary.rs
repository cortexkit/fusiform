//! What happens to a fact whose first real fetch disagrees with the seed.
//!
//! This is a probe, not a guard. The design note settles era zero — an embedded
//! seed writes eras with `boundary_kind: Seed`, and a first fetch that AGREES
//! writes nothing (§3, §7). It does not say what kind the DISAGREEING facts get
//! on that first fetch, and the answer decides what a history means for every
//! model that moved between the snapshot being cut and the install running.
//!
//! Written to observe the shipped behaviour rather than to assert a preference.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp, TokenClass};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, FactKey, NewObservation};

const FIXTURE: &str = include_str!("../../fusiform-core/fixtures/models-dev-excerpt.json");

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

/// Seeding writes eras and records no observation.
///
/// The second half is the load-bearing one: a seed is not a poll. Recording an
/// observation for it would claim fusiform looked at the upstream at that
/// instant, when in fact someone ran a refresh script at build time.
#[test]
fn seeding_writes_eras_but_records_no_observation() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let catalog = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;

    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    let written = store.append_eras(&plan.eras).unwrap();
    assert!(written > 100, "the seed must populate the store");

    let polls = store.recent_observations(SourceId::ModelsDev, 10).unwrap();
    assert!(
        polls.is_empty(),
        "a seed is not a poll and must record no observation"
    );

    // And the seeded eras carry no window, which is the honest answer: fusiform
    // has no prior observation of anything.
    let history = store
        .fact_history(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
        )
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].boundary_kind, "seed");
    assert_eq!(history[0].prior_observation_at, None);
}

/// A first fetch that AGREES with the seed writes nothing.
///
/// Settled by the design note: appending an era would assert a change that did
/// not happen, and the seed era already reads correctly as "this value has
/// never been observed to change since bootstrap".
#[test]
fn a_first_fetch_agreeing_with_the_seed_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let catalog = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;

    let seed = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&seed.eras).unwrap();

    // The first real poll, returning the same document the seed was cut from.
    let obs = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(5_000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some("h".to_string()),
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();

    let plan = plan_ingest(
        &store,
        &catalog,
        Timestamp(5_000),
        BoundaryKind::Seed,
        Some(obs),
    )
    .unwrap();
    assert!(
        plan.eras.is_empty(),
        "an agreeing fetch must write nothing, got {} eras",
        plan.eras.len()
    );
}

/// PROBE: what kind does a disagreeing fact get on the first fetch after a seed?
///
/// Prints rather than asserts. The point is to observe what the shipped code
/// does before deciding whether it is right, because the decision writes
/// history that a later fix can only reach through a Correction.
#[test]
fn probe_what_a_disagreeing_first_fetch_produces() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let catalog = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;

    let seed = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&seed.eras).unwrap();

    // The upstream moved between the snapshot being cut and this install
    // running: a real case, since the seed is compiled in at release time.
    let mut doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    doc.get_mut("anthropic")
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.get_mut("claude-sonnet-4-5"))
        .and_then(|m| m.get_mut("cost"))
        .and_then(|c| c.as_object_mut())
        .unwrap()
        .insert("input".to_string(), serde_json::json!(2.5));
    let moved = normalize_models_dev(&serde_json::to_vec(&doc).unwrap())
        .unwrap()
        .catalog;

    let obs = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(5_000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some("h2".to_string()),
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();

    // The rule the poll loop applies: no prior CONFIRMING observation strictly
    // before this instant means no left edge, so the boundary is a Seed.
    let prior = store
        .last_confirming_observation_before(SourceId::ModelsDev, Timestamp(5_000))
        .unwrap();
    eprintln!("prior confirming observation before the first fetch: {prior:?}");

    let kind = if prior.is_some() {
        BoundaryKind::Observed
    } else {
        BoundaryKind::Seed
    };
    eprintln!("boundary kind the loop would choose: {kind:?}");

    let plan = plan_ingest(
        &store,
        &moved,
        Timestamp(5_000),
        kind.clone(),
        match kind {
            BoundaryKind::Seed => None,
            _ => Some(obs),
        },
    )
    .unwrap();
    eprintln!(
        "eras written by the disagreeing fetch: {} (changed_facts={})",
        plan.eras.len(),
        plan.changed_facts
    );
    store.append_eras(&plan.eras).unwrap();

    let history = store
        .fact_history(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
        )
        .unwrap();
    eprintln!("\nhistory of rate.input as an operator would read it:");
    for row in &history {
        eprintln!(
            "  boundary={} kind={} window_from={:?} value={}",
            row.boundary_at.0, row.boundary_kind, row.prior_observation_at, row.value_json
        );
    }
}
