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

const FIXTURE: &str = include_str!("../../../fusiform-core/fixtures/models-dev-excerpt.json");

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

/// A `Seed` boundary written through the store primitive carries no window.
///
/// This exercises `plan_ingest` directly, below the module's seeding path. The
/// module DOES record an observation when it seeds — stamped at the instant the
/// snapshot was fetched, so a later disagreeing fetch has a left edge (see
/// `fusiform-module/tests/it/bootstrap.rs`). What is checked here is narrower and
/// still load-bearing: the era rows themselves claim no observation and no
/// window, whatever wrote them.
#[test]
fn a_seed_boundary_claims_no_window() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let catalog = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;

    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    let written = store.append_eras(&plan.eras).unwrap();
    assert!(written > 100, "the seed must populate the store");

    // Nothing recorded an observation here because nothing was asked to: this
    // is the store primitive, below the module's seeding path.
    let polls = store.recent_observations(SourceId::ModelsDev, 10).unwrap();
    assert!(polls.is_empty(), "plan_ingest does not record observations");

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

/// Without a prior observation, a disagreeing fetch cannot claim a window.
///
/// The defect this file was written to find, kept as a regression guard. A
/// store holding eras but NO observation leaves a later disagreeing fetch with
/// no left edge, so an observed boundary at that instant is unwritable.
///
/// The module's seeding path avoids this by recording a `Seeded` observation
/// (see `fusiform-module/tests/it/bootstrap.rs`). This asserts the underlying
/// mechanism, so a change that stops recording that observation fails here with
/// an explanation rather than silently producing a second seed boundary that
/// claims the store came into existence twice.
#[test]
fn eras_without_an_observation_leave_a_later_fetch_with_no_window() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let catalog = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;

    // Eras with no observation: what the module produced before it recorded a
    // seeded observation.
    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();

    let prior = store
        .last_confirming_observation_before(SourceId::ModelsDev, Timestamp(5_000))
        .unwrap();
    assert_eq!(
        prior, None,
        "eras alone supply no window edge; only an observation can"
    );

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

    // The observation at 5000 is not STRICTLY BEFORE itself, so it supplies no
    // edge for a boundary at the same instant. An observed boundary is
    // therefore refused rather than written with an empty window.
    let plan = plan_ingest(
        &store,
        &moved,
        Timestamp(5_000),
        BoundaryKind::Observed,
        Some(obs),
    )
    .unwrap();
    let result = store.append_eras(&plan.eras);
    assert!(
        result.is_err(),
        "an observed boundary with no prior observation must be refused, not \
         written with an empty window"
    );
}

/// The three statements of "which outcomes confirm current values" agree.
///
/// The rule lives in three places by necessity: the domain's
/// `CONFIRMING_OUTCOMES`, the query built from it, and the schema's partial
/// index — DDL that cannot be built from a Rust constant. The first two are now
/// one source; this holds the third against it.
///
/// Found by a surviving mutation: making the domain predicate wrong changed no
/// test result, because every real decision was made in SQL. One belief in
/// three artifacts, with nothing comparing them.
#[test]
fn the_schema_index_matches_the_domain_list() {
    let ddl = fusiform_store::schema::MIGRATIONS
        .iter()
        .map(|m| m.statements)
        .collect::<String>();

    let start = ddl
        .find("CREATE INDEX observation_confirming")
        .expect("the partial index must exist");
    let clause = &ddl[start..];
    let clause = &clause[..clause.find(';').expect("the statement must terminate")];

    for outcome in fusiform_core::CONFIRMING_OUTCOMES {
        assert!(
            clause.contains(&format!("'{outcome}'")),
            "the confirming index omits {outcome:?}, so a query using it would \
             skip those rows:\n{clause}"
        );
    }

    // And the reverse: an outcome in the index that is NOT confirming would
    // make a failed poll act as a window edge.
    for outcome in fusiform_core::ALL_OUTCOMES {
        if fusiform_core::CONFIRMING_OUTCOMES.contains(outcome) {
            continue;
        }
        assert!(
            !clause.contains(&format!("'{outcome}'")),
            "the confirming index includes {outcome:?}, which does not confirm \
             anything:\n{clause}"
        );
    }
}

/// Every enum variant has a wire string, and the two lists partition it.
///
/// `ALL_OUTCOMES` is written by hand, so it can fall behind the enum. This
/// walks real values through `wire_str` and checks the constant covers exactly
/// what the code produces.
#[test]
fn the_outcome_lists_cover_every_variant() {
    use fusiform_core::{FailureClass, ObservationOutcome};

    let every = [
        ObservationOutcome::Changed { snapshot_seq: 1 },
        ObservationOutcome::Unchanged,
        ObservationOutcome::NotModified,
        ObservationOutcome::Seeded,
        ObservationOutcome::Failed {
            class: FailureClass::Network,
        },
    ];

    let produced: Vec<&str> = every.iter().map(|o| o.wire_str()).collect();
    assert_eq!(
        produced.len(),
        fusiform_core::ALL_OUTCOMES.len(),
        "ALL_OUTCOMES has drifted from the enum: {produced:?}"
    );
    for wire in &produced {
        assert!(
            fusiform_core::ALL_OUTCOMES.contains(wire),
            "{wire:?} is produced by the enum but missing from ALL_OUTCOMES"
        );
    }

    // The predicate and the list agree for every variant.
    for outcome in &every {
        assert_eq!(
            outcome.confirms_current_values(),
            fusiform_core::CONFIRMING_OUTCOMES.contains(&outcome.wire_str()),
            "the predicate and the list disagree about {:?}",
            outcome.wire_str()
        );
    }

    // A failed poll confirms nothing: the invariant the whole split exists for.
    assert!(!ObservationOutcome::Failed {
        class: FailureClass::Parse
    }
    .confirms_current_values());
}
