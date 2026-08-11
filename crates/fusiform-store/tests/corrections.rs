//! Reading a fact whose record fusiform knows it got wrong.
//!
//! A `Corrected` era says the value recorded across an interval was wrong. The
//! hard part is not writing one — it is that a point-in-time read selecting by
//! `boundary_at <= T` cannot see it, because a correction is always written
//! AFTER the interval it describes. So the naive read lands on the bad era and
//! returns it looking exactly like any other answer.
//!
//! Found by a probe, not by reading: corrections could be written and every
//! point-in-time read ignored them completely.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{
    BoundaryKind, Correction, FieldId, ObservationOutcome, SourceId, Timestamp, TokenClass,
};
use fusiform_store::{CatalogStore, FactKey, NewEra, NewObservation, PointInTime};

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

fn key() -> FactKey {
    FactKey::rate(TokenClass::Input)
}

/// A store holding a seed at t=1000, a WRONG observed value at t=5000, and a
/// correction at t=9000 covering 5000..9000.
fn corrected_history(store: &CatalogStore) {
    // The seeded observation the bootstrap path writes. Without it the observed
    // era below has no prior confirming observation and the store correctly
    // refuses it — the fixture must build the same history the real seed does,
    // not a plausible-looking approximation of it.
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1_000),
            outcome: ObservationOutcome::Seeded,
            normalized_hash: Some("h0".into()),
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();

    store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: key(),
            value_json: r#"{"units":3000000000}"#.into(),
            boundary_at: Timestamp(1_000),
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        }])
        .unwrap();

    let bad = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(5_000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some("h1".into()),
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: key(),
            value_json: r#"{"units":999000000000}"#.into(),
            boundary_at: Timestamp(5_000),
            boundary_kind: BoundaryKind::Observed,
            observation_id: Some(bad),
        }])
        .unwrap();

    store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: key(),
            value_json: r#"{"units":3000000000}"#.into(),
            boundary_at: Timestamp(9_000),
            boundary_kind: BoundaryKind::Corrected(Correction {
                fields: vec![FieldId::Rate {
                    class: TokenClass::Input,
                }],
                affected_from: Timestamp(5_000),
                affected_until: Timestamp(9_000),
                reason: "parser scaled the rate by 333x".to_string(),
            }),
            observation_id: None,
        }])
        .unwrap();
}

fn read(store: &CatalogStore, at: i64) -> PointInTime {
    store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &key(),
            Timestamp(at),
        )
        .unwrap()
}

/// A read inside a corrected interval refuses, and says why.
///
/// Not the corrected value: fusiform does not know what the upstream held at
/// that instant, only that its own record was bad. Not the recorded value
/// either — that is how a consumer re-prices against a rate that was never
/// real. The honest answer is a refusal carrying the reason.
#[test]
fn a_read_inside_a_corrected_interval_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    corrected_history(&store);

    match read(&store, 6_000) {
        PointInTime::Corrected { corrections } => {
            assert_eq!(corrections.len(), 1);
            let c = &corrections[0];
            assert_eq!(c.affected_from, Timestamp(5_000));
            assert_eq!(c.affected_until, Timestamp(9_000));
            assert!(
                c.reason.contains("333x"),
                "the refusal must carry the reason: {:?}",
                c.reason
            );
            // And the corrected_at is after the interval, which is what makes
            // this case invisible to a boundary_at-ordered read.
            assert!(c.corrected_at > c.affected_until || c.corrected_at == c.affected_until);
        }
        other => panic!("a corrected instant must refuse, got {other:?}"),
    }

    // The value is not reachable through the ergonomic accessor either.
    assert!(
        read(&store, 6_000).value_json().is_none(),
        "a corrected read must not leak a value"
    );
    assert!(read(&store, 6_000).known().is_none());
}

/// Instants outside the interval still answer.
///
/// A correction is not a poison pill for the whole fact. If it were, one
/// correction would make a model's entire history unreadable, and the
/// incentive would be to not record corrections at all.
#[test]
fn instants_outside_the_interval_still_answer() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    corrected_history(&store);

    // Before the bad region.
    let before = read(&store, 2_000);
    assert_eq!(
        before.value_json(),
        Some(r#"{"units":3000000000}"#),
        "an instant before the correction is unaffected"
    );

    // After it.
    let after = read(&store, 20_000);
    assert_eq!(
        after.value_json(),
        Some(r#"{"units":3000000000}"#),
        "an instant after the correction gets the corrected value"
    );
}

/// The interval is closed at both ends.
///
/// `affected_from` is a lower bound and `affected_until` is when the fix
/// deployed. Both endpoints are inside the region the correction describes:
/// an off-by-one at either end returns a known-bad value confidently, which is
/// the exact failure the refusal exists to prevent.
#[test]
fn both_endpoints_are_inside_the_corrected_interval() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    corrected_history(&store);

    for at in [5_000i64, 9_000] {
        assert!(
            matches!(read(&store, at), PointInTime::Corrected { .. }),
            "t={at} is an endpoint of the corrected interval and must refuse"
        );
    }
    // And one millisecond outside each end answers.
    assert!(matches!(read(&store, 4_999), PointInTime::Known(_)));
    assert!(matches!(read(&store, 9_001), PointInTime::Known(_)));
}

/// An instant with no era at all is Unknown, not Corrected and not a value.
///
/// Three states rather than an Option: "never recorded" and "recorded wrong"
/// are different facts. Collapsing them would make a corrected interval look
/// like a gap, which reads as "nothing was published".
#[test]
fn an_instant_before_every_era_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    corrected_history(&store);

    assert_eq!(read(&store, 500), PointInTime::Unknown);
}

/// A correction on one fact does not silence another.
#[test]
fn a_correction_is_scoped_to_its_fact() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    corrected_history(&store);

    // Another fact on the same model, spanning the corrected interval.
    store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: FactKey::rate(TokenClass::Output),
            value_json: r#"{"units":15000000000}"#.into(),
            boundary_at: Timestamp(1_000),
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        }])
        .unwrap();

    let output = store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Output),
            Timestamp(6_000),
        )
        .unwrap();
    assert_eq!(
        output.value_json(),
        Some(r#"{"units":15000000000}"#),
        "correcting the input rate must not refuse a read of the output rate"
    );
}

/// `recorded_value_at` still returns what the store recorded, corrections and
/// all.
///
/// The two queries are genuinely different and both are needed. An auditor
/// reconstructing what fusiform believed at a past instant — to explain a
/// charge a consumer made on that belief — needs the bad value. What they must
/// not do is get it by accident, which is why it has a different name.
#[test]
fn the_recorded_value_is_still_reachable_deliberately() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    corrected_history(&store);

    let recorded = store
        .recorded_value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &key(),
            Timestamp(6_000),
        )
        .unwrap()
        .expect("the bad era is still in the store");
    assert_eq!(
        recorded.value_json, r#"{"units":999000000000}"#,
        "the recorded history is preserved; only the interpretation changed"
    );
}

/// Two overlapping corrections both surface.
///
/// Picking the newest would hide the other, and two corrections to overlapping
/// intervals are two separate statements about what went wrong.
#[test]
fn overlapping_corrections_all_surface() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    corrected_history(&store);

    store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: key(),
            value_json: r#"{"units":3000000000}"#.into(),
            boundary_at: Timestamp(12_000),
            boundary_kind: BoundaryKind::Corrected(Correction {
                fields: vec![FieldId::Rate {
                    class: TokenClass::Input,
                }],
                affected_from: Timestamp(4_000),
                affected_until: Timestamp(7_000),
                reason: "a second defect covering part of the same window".to_string(),
            }),
            observation_id: None,
        }])
        .unwrap();

    match read(&store, 6_000) {
        PointInTime::Corrected { corrections } => {
            assert_eq!(
                corrections.len(),
                2,
                "both corrections cover t=6000 and both must be reported"
            );
            // Oldest first, so a reader sees them in the order they were made.
            assert!(corrections[0].corrected_at < corrections[1].corrected_at);
        }
        other => panic!("expected two corrections, got {other:?}"),
    }
}

/// The BULK read honours corrections too.
///
/// `value_at` is not the surface a consumer uses — `read_catalog` is, and it
/// takes the same `at`. When this was a probe it printed the known-bad
/// `999000000000` at an instant where `value_at` refused: two surfaces
/// disagreeing about the same fact, with the honest one being the one nobody
/// calls.
///
/// The corrected fact is OMITTED rather than replaced with a marker. A
/// consumer that needs to know why asks `value_at`, which names the
/// correction. A sentinel in the value position would have to be recognised by
/// every consumer parsing a rate, and the ones that did not would read it as
/// data.
#[test]
fn the_bulk_read_omits_a_corrected_fact() {
    use fusiform_store::serve::CatalogQuery;

    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    corrected_history(&store);

    // Existence, so the model is present in a default read.
    store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: FactKey::existence(),
            value_json: "\"present\"".into(),
            boundary_at: Timestamp(1_000),
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        }])
        .unwrap();

    let snapshot = store
        .read_catalog(&CatalogQuery::at(SourceId::ModelsDev, Timestamp(6_000)))
        .unwrap();
    let model = snapshot
        .models
        .iter()
        .find(|m| m.model_id == "claude-sonnet-4-5")
        .expect("the model is present");

    // The corrected fact is gone.
    assert!(
        !model.facts.contains_key(&key()),
        "a corrected fact must not appear in a bulk read: {:?}",
        model.facts.get(&key())
    );
    // Specifically, the known-bad value is not what came back.
    assert!(
        !model.facts.values().any(|v| v.contains("999000000000")),
        "the known-bad value leaked into the catalog"
    );

    // Facts the correction does not cover are unaffected — a correction is not
    // a poison pill for the model.
    assert_eq!(
        model.facts.get(&FactKey::existence()).map(String::as_str),
        Some("\"present\"")
    );

    // And outside the interval the fact is back.
    let later = store
        .read_catalog(&CatalogQuery::at(SourceId::ModelsDev, Timestamp(20_000)))
        .unwrap();
    let model = later
        .models
        .iter()
        .find(|m| m.model_id == "claude-sonnet-4-5")
        .expect("the model is present");
    assert_eq!(
        model.facts.get(&key()).map(String::as_str),
        Some(r#"{"units":3000000000}"#),
        "outside the corrected interval the fact reads normally"
    );
}
