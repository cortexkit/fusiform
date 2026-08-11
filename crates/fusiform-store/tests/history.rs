//! Store behaviour, proven against a real SQLite file.
//!
//! No mocked connection: the constraints under test are schema CHECKs, and a
//! mock would assert that the test's own model of them is self-consistent. Each
//! test opens a temporary database, runs the real migrations, and reads back
//! what SQLite actually stored.

use fusiform_core::{
    BoundaryKind, Correction, FailureClass, FieldId, ObservationOutcome, SourceId, Timestamp,
    TokenClass,
};
use fusiform_store::{CatalogError, CatalogStore, FactKey, NewEra, NewObservation};

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};

struct Fixture {
    store: CatalogStore,
    // Held so the directory outlives the store.
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let descriptor = StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_string_lossy().to_string(),
        },
    };
    let store = CatalogStore::open(&descriptor).expect("store opens and migrates");
    Fixture { store, _dir: dir }
}

fn observation(at: i64, outcome: ObservationOutcome) -> NewObservation {
    NewObservation {
        source: SourceId::ModelsDev,
        observed_at: Timestamp(at),
        outcome,
        normalized_hash: Some("hash".to_string()),
        raw_hash: Some("rawhash".to_string()),
        etag: Some("\"etag\"".to_string()),
        duration_ms: Some(42),
        detail: None,
    }
}

fn era(at: i64, kind: BoundaryKind, observation_id: Option<i64>) -> NewEra {
    NewEra {
        source: SourceId::ModelsDev,
        provider_id: "anthropic".to_string(),
        model_id: "claude-sonnet-4-5".to_string(),
        fact_key: FactKey::rate(TokenClass::Input),
        value_json: r#"{"units":3000000000,"exponent":9}"#.to_string(),
        boundary_at: Timestamp(at),
        boundary_kind: kind,
        observation_id,
    }
}

/// An observed era's window comes from the store's own observation history.
#[test]
fn an_observed_era_carries_the_window_the_store_measured() {
    let f = fixture();

    let first = f
        .store
        .record_observation(&observation(1_000, ObservationOutcome::Unchanged))
        .unwrap();
    let _ = first;
    let second = f
        .store
        .record_observation(&observation(
            2_000,
            ObservationOutcome::Changed { snapshot_seq: 1 },
        ))
        .unwrap();

    f.store
        .append_eras(&[era(2_000, BoundaryKind::Observed, Some(second))])
        .unwrap();

    let row = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
            Timestamp(5_000),
        )
        .unwrap()
        .expect("the era must be found");

    // The window is arithmetic, not a disclaimer: the change happened somewhere
    // in (1000, 2000].
    assert_eq!(
        row.observation_window(),
        Some((Timestamp(1_000), Timestamp(2_000)))
    );
}

/// A FAILED poll must never narrow an observation window.
///
/// This is the distinction the observation table exists for. A failed poll
/// observed nothing; if it acted as a window edge, the catalog would claim to
/// have confirmed values it never saw — narrowing a three-hour uncertainty to
/// thirty minutes on the strength of a connection timeout.
#[test]
fn a_failed_poll_does_not_narrow_the_window() {
    let f = fixture();

    f.store
        .record_observation(&observation(1_000, ObservationOutcome::Unchanged))
        .unwrap();
    // Three failures land between the last real observation and the change.
    for at in [1_200, 1_400, 1_600] {
        f.store
            .record_observation(&observation(
                at,
                ObservationOutcome::Failed {
                    class: FailureClass::Network,
                },
            ))
            .unwrap();
    }
    let change = f
        .store
        .record_observation(&observation(
            2_000,
            ObservationOutcome::Changed { snapshot_seq: 1 },
        ))
        .unwrap();

    f.store
        .append_eras(&[era(2_000, BoundaryKind::Observed, Some(change))])
        .unwrap();

    let row = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
            Timestamp(5_000),
        )
        .unwrap()
        .unwrap();

    // 1000, not 1600. The failures are recorded and contribute nothing.
    assert_eq!(
        row.observation_window(),
        Some((Timestamp(1_000), Timestamp(2_000))),
        "a failed poll must not act as a window edge"
    );
}

/// A 304 confirms current values and legitimately narrows the window.
///
/// The other side of the same rule: a conditional GET answered with 304 is a
/// real observation that the upstream's content is unchanged, so it is exactly
/// as good a window edge as a full 200.
#[test]
fn a_not_modified_response_does_narrow_the_window() {
    let f = fixture();

    f.store
        .record_observation(&observation(1_000, ObservationOutcome::Unchanged))
        .unwrap();
    f.store
        .record_observation(&observation(1_800, ObservationOutcome::NotModified))
        .unwrap();
    let change = f
        .store
        .record_observation(&observation(
            2_000,
            ObservationOutcome::Changed { snapshot_seq: 1 },
        ))
        .unwrap();

    f.store
        .append_eras(&[era(2_000, BoundaryKind::Observed, Some(change))])
        .unwrap();

    let row = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
            Timestamp(5_000),
        )
        .unwrap()
        .unwrap();

    assert_eq!(
        row.observation_window(),
        Some((Timestamp(1_800), Timestamp(2_000))),
        "a 304 confirms current values and is a valid window edge"
    );
}

/// An observation window can never have zero width.
///
/// The near edge is the last time fusiform saw the OLD value, which is the
/// observation BEFORE the one that detected the change. Taking "the latest
/// confirming observation" instead returns the boundary itself — the detecting
/// observation is already recorded when the era is written — and produces a
/// window of `(2000, 2000]`, claiming the change happened in an instant of no
/// duration. That is a stronger claim than any poll can support, and it is what
/// this store's first write path actually did until the schema rejected it.
#[test]
fn an_observation_window_is_never_zero_width() {
    let f = fixture();

    f.store
        .record_observation(&observation(1_000, ObservationOutcome::Unchanged))
        .unwrap();
    let detecting = f
        .store
        .record_observation(&observation(
            2_000,
            ObservationOutcome::Changed { snapshot_seq: 1 },
        ))
        .unwrap();

    f.store
        .append_eras(&[era(2_000, BoundaryKind::Observed, Some(detecting))])
        .unwrap();

    let row = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
            Timestamp(5_000),
        )
        .unwrap()
        .unwrap();

    let (near, far) = row
        .observation_window()
        .expect("an observed era has a window");
    assert!(
        near.0 < far.0,
        "a window must have positive width, got ({}, {}]",
        near.0,
        far.0
    );
    // Specifically: the edge is the observation that last saw the old value,
    // not the one that detected the change.
    assert_eq!(near, Timestamp(1_000));
}

/// Eras written at different boundaries in one batch get their own windows.
///
/// A batch is not a single instant. Resolving one near edge for the whole batch
/// would attach the wrong window to whichever era did not own it, and the error
/// is invisible: both windows look plausible.
#[test]
fn each_boundary_in_a_batch_gets_its_own_window() {
    let f = fixture();

    f.store
        .record_observation(&observation(1_000, ObservationOutcome::Unchanged))
        .unwrap();
    let early = f
        .store
        .record_observation(&observation(
            2_000,
            ObservationOutcome::Changed { snapshot_seq: 1 },
        ))
        .unwrap();
    f.store
        .record_observation(&observation(3_000, ObservationOutcome::Unchanged))
        .unwrap();
    let late = f
        .store
        .record_observation(&observation(
            4_000,
            ObservationOutcome::Changed { snapshot_seq: 2 },
        ))
        .unwrap();

    // Two facts whose eras open at different instants, written together.
    let mut first = era(2_000, BoundaryKind::Observed, Some(early));
    first.fact_key = FactKey::rate(TokenClass::Input);
    let mut second = era(4_000, BoundaryKind::Observed, Some(late));
    second.fact_key = FactKey::rate(TokenClass::Output);

    f.store.append_eras(&[first, second]).unwrap();

    let window = |class| {
        f.store
            .value_at(
                SourceId::ModelsDev,
                "anthropic",
                "claude-sonnet-4-5",
                &FactKey::rate(class),
                Timestamp(9_999),
            )
            .unwrap()
            .unwrap()
            .observation_window()
    };

    assert_eq!(
        window(TokenClass::Input),
        Some((Timestamp(1_000), Timestamp(2_000)))
    );
    assert_eq!(
        window(TokenClass::Output),
        Some((Timestamp(3_000), Timestamp(4_000)))
    );
}

/// A seed era carries no window, and says so rather than inventing one.
#[test]
fn a_seed_era_has_no_observation_window() {
    let f = fixture();

    // No observations at all: a fresh install coming up from the embedded seed.
    f.store
        .append_eras(&[era(500, BoundaryKind::Seed, None)])
        .unwrap();

    let row = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
            Timestamp(1_000),
        )
        .unwrap()
        .unwrap();

    assert_eq!(row.boundary_kind, BoundaryKind::Seed);
    assert_eq!(
        row.observation_window(),
        None,
        "a seed is not an observation and must not claim a window"
    );
    assert_eq!(row.observation_id, None);
}

/// The first era for a source cannot be `Observed`.
///
/// There is no prior observation to bound it, so an observed boundary would be
/// claiming a window it cannot measure. The store names the era rather than
/// letting a constraint violation surface from the driver.
#[test]
fn an_observed_era_with_no_prior_observation_is_refused() {
    let f = fixture();

    let obs = f
        .store
        .record_observation(&observation(
            1_000,
            ObservationOutcome::Changed { snapshot_seq: 1 },
        ))
        .unwrap();

    // The only observation IS this one, so there is no prior confirming
    // observation to open a window against.
    let result = f
        .store
        .append_eras(&[era(1_000, BoundaryKind::Observed, Some(obs))]);

    match result {
        Err(CatalogError::Invariant(msg)) => {
            assert!(
                msg.contains("seed"),
                "the error should point at the fix: {msg}"
            );
        }
        other => panic!("expected an invariant error, got {other:?}"),
    }
}

/// A correction round-trips with its full extent.
#[test]
fn a_correction_round_trips_with_its_extent() {
    let f = fixture();

    let written = Correction {
        fields: vec![FieldId::TierThreshold, FieldId::TierRate],
        affected_from: Timestamp(1_000),
        affected_until: Timestamp(4_000),
        reason: "docs/findings/2026-08-11-legacy-tier-name.md".to_string(),
    };

    f.store
        .append_eras(&[era(5_000, BoundaryKind::Corrected(written.clone()), None)])
        .unwrap();

    let row = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
            Timestamp(6_000),
        )
        .unwrap()
        .unwrap();

    let BoundaryKind::Corrected(read_back) = &row.boundary_kind else {
        panic!("the boundary kind must survive the round trip");
    };
    assert_eq!(read_back, &written);

    // A correction is about fusiform's own record, not an upstream change, so
    // it carries no observation window even though observations may exist.
    assert_eq!(row.observation_window(), None);
}

/// Point-in-time lookup returns the era in force at the instant asked about.
#[test]
fn a_point_in_time_read_returns_the_era_in_force() {
    let f = fixture();

    f.store
        .record_observation(&observation(1_000, ObservationOutcome::Unchanged))
        .unwrap();
    let o2 = f
        .store
        .record_observation(&observation(
            2_000,
            ObservationOutcome::Changed { snapshot_seq: 1 },
        ))
        .unwrap();

    let mut first = era(2_000, BoundaryKind::Observed, Some(o2));
    first.value_json = r#"{"units":3000000000}"#.to_string();
    f.store.append_eras(&[first]).unwrap();

    f.store
        .record_observation(&observation(3_000, ObservationOutcome::Unchanged))
        .unwrap();
    let o4 = f
        .store
        .record_observation(&observation(
            4_000,
            ObservationOutcome::Changed { snapshot_seq: 2 },
        ))
        .unwrap();
    let mut second = era(4_000, BoundaryKind::Observed, Some(o4));
    second.value_json = r#"{"units":6000000000}"#.to_string();
    f.store.append_eras(&[second]).unwrap();

    let read = |at: i64| {
        f.store
            .value_at(
                SourceId::ModelsDev,
                "anthropic",
                "claude-sonnet-4-5",
                &FactKey::rate(TokenClass::Input),
                Timestamp(at),
            )
            .unwrap()
    };

    // Before anything was known.
    assert!(
        read(1_500).is_none(),
        "an instant before every era is not an answer of zero"
    );
    // Inside the first era.
    assert_eq!(read(3_500).unwrap().value_json, r#"{"units":3000000000}"#);
    // Exactly on the second boundary: the new value is in force AT its boundary.
    assert_eq!(read(4_000).unwrap().value_json, r#"{"units":6000000000}"#);
    // After.
    assert_eq!(read(9_999).unwrap().value_json, r#"{"units":6000000000}"#);
}

/// The catalog version refuses to move backwards or stand still.
///
/// A rewound version is silent at the producer and total at the consumer: every
/// consumer correctly refuses every subsequent push, and nothing in fusiform
/// looks wrong. Rejecting the write is what turns that into an error at the
/// moment it happens rather than a mystery later.
#[test]
fn the_catalog_version_only_ever_advances() {
    let f = fixture();
    let obs = f
        .store
        .record_observation(&observation(
            1_000,
            ObservationOutcome::Changed { snapshot_seq: 1 },
        ))
        .unwrap();

    assert_eq!(f.store.catalog_version().unwrap(), 0);

    f.store
        .advance_catalog_version(1, obs, Timestamp(1_000))
        .unwrap();
    assert_eq!(f.store.catalog_version().unwrap(), 1);

    // Backwards: refused.
    assert!(f
        .store
        .advance_catalog_version(0, obs, Timestamp(2_000))
        .is_err());
    // Sideways: also refused. An unchanged version with new content is the same
    // corruption as a rewind, seen from the consumer.
    assert!(f
        .store
        .advance_catalog_version(1, obs, Timestamp(2_000))
        .is_err());

    // And the refusals did not partially apply.
    assert_eq!(f.store.catalog_version().unwrap(), 1);

    f.store
        .advance_catalog_version(2, obs, Timestamp(3_000))
        .unwrap();
    assert_eq!(f.store.catalog_version().unwrap(), 2);
}

/// The schema itself refuses a window on a non-observed boundary.
///
/// The store's code writes NULL for those kinds, so this asserts the database
/// would reject the row even if that code were wrong — two independent
/// barriers, which is the right shape for an error that would otherwise be
/// silent.
#[test]
fn the_schema_rejects_a_window_on_a_seed_boundary() {
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
    let store = CatalogStore::open(&descriptor).unwrap();
    drop(store);

    // Open the file directly, bypassing the store's own write path, and try to
    // insert what the store would never construct.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let result = conn.execute(
        "INSERT INTO era \
         (source, provider_id, model_id, fact_key, value_json, \
          boundary_at_ms, boundary_kind, prior_observation_at_ms) \
         VALUES ('models.dev', 'p', 'm', 'rate.input', '{}', 2000, 'seed', 1000)",
        [],
    );
    assert!(
        result.is_err(),
        "a seed boundary carrying an observation window must be rejected by the schema"
    );

    // And the same row without the window is accepted, so the constraint is not
    // simply rejecting everything.
    conn.execute(
        "INSERT INTO era \
         (source, provider_id, model_id, fact_key, value_json, \
          boundary_at_ms, boundary_kind) \
         VALUES ('models.dev', 'p', 'm', 'rate.input', '{}', 2000, 'seed')",
        [],
    )
    .expect("a seed with no window is valid");
}

/// The schema refuses a correction with no extent.
#[test]
fn the_schema_rejects_a_correction_without_its_extent() {
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
    let store = CatalogStore::open(&descriptor).unwrap();
    drop(store);

    let conn = rusqlite::Connection::open(&path).unwrap();

    // A correction with no extent cannot partition anything, which is the only
    // thing an extent is for.
    assert!(conn
        .execute(
            "INSERT INTO era \
             (source, provider_id, model_id, fact_key, value_json, \
              boundary_at_ms, boundary_kind) \
             VALUES ('models.dev', 'p', 'm', 'rate.input', '{}', 2000, 'corrected')",
            [],
        )
        .is_err());

    // A partial extent is refused too: the four columns are all-or-nothing.
    assert!(conn
        .execute(
            "INSERT INTO era \
             (source, provider_id, model_id, fact_key, value_json, \
              boundary_at_ms, boundary_kind, corrected_fields_json, affected_from_ms) \
             VALUES ('models.dev', 'p', 'm', 'rate.input', '{}', 2000, 'corrected', '[]', 100)",
            [],
        )
        .is_err());
}

/// Two eras for one fact at one instant would make a point-in-time read
/// ambiguous, and the tie-break would be silent.
#[test]
fn one_fact_cannot_have_two_eras_at_the_same_instant() {
    let f = fixture();

    f.store
        .append_eras(&[era(1_000, BoundaryKind::Seed, None)])
        .unwrap();
    let second = f.store.append_eras(&[era(1_000, BoundaryKind::Seed, None)]);
    assert!(
        second.is_err(),
        "a second era at the same instant must be rejected"
    );
}

/// A batch of eras commits atomically.
///
/// A partially-applied change leaves the catalog describing a state the
/// upstream never published — some facts moved, others not — and a consumer
/// reading between the two writes would believe it, and nothing later would tell it otherwise.
#[test]
fn a_batch_of_eras_is_all_or_nothing() {
    let f = fixture();

    let good = era(1_000, BoundaryKind::Seed, None);
    let mut other_fact = era(1_000, BoundaryKind::Seed, None);
    other_fact.fact_key = FactKey::rate(TokenClass::Output);
    // A duplicate of the first: this row will violate the uniqueness index.
    let duplicate = era(1_000, BoundaryKind::Seed, None);

    let result = f.store.append_eras(&[good, other_fact, duplicate]);
    assert!(result.is_err(), "the batch must fail");

    // Nothing from the batch survived, including the two rows that were valid.
    for class in [TokenClass::Input, TokenClass::Output] {
        assert!(
            f.store
                .value_at(
                    SourceId::ModelsDev,
                    "anthropic",
                    "claude-sonnet-4-5",
                    &FactKey::rate(class),
                    Timestamp(9_999),
                )
                .unwrap()
                .is_none(),
            "a failed batch must leave no rows behind"
        );
    }
}
