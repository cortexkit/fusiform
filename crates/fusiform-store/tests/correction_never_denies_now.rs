//! A correction can never make a CURRENT read refuse.
//!
//! # Why this property is worth its own file
//!
//! A consumer with an armed spending cap treats an unresolved rate as a denial:
//! a request it cannot price is a request it will not allow. A correction makes
//! reads inside its window refuse. Put together, those two facts appear to mean
//! that fusiform admitting its own record was wrong could take a consumer's
//! live traffic offline — fusiform's repair becoming someone else's outage.
//!
//! It cannot, and the reason is structural rather than careful. A correction's
//! `affected_until` may never exceed the instant it is recorded at
//! (`append_eras` refuses it), so the corrected window is always entirely in
//! the past at the moment it is written, and time only moves away from it. A
//! read at `now` is therefore never inside a corrected window.
//!
//! So a correction affects point-in-time reads INTO the corrected past — an
//! audit, a re-pricing, a "what did this cost on Tuesday" — and never the
//! pricing of traffic happening now.
//!
//! Written because a consumer asked about exactly this interaction, and the
//! answer deserved a test rather than an argument. The distinction is invisible
//! from the outside: both cases are `value_at`, and only the instant differs.

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

/// A correction covering everything up to the moment it is written still leaves
/// a current read answerable.
#[test]
fn a_correction_cannot_refuse_a_read_at_now() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let key = FactKey::rate(TokenClass::Input);

    for at in [1_000, 5_000] {
        store
            .record_observation(&NewObservation {
                source: SourceId::ModelsDev,
                observed_at: Timestamp(at),
                outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
                normalized_hash: Some(format!("h{at}")),
                raw_hash: None,
                etag: None,
                duration_ms: Some(10),
                detail: None,
            })
            .unwrap();
    }

    let era = |value: &str, at: i64, kind: BoundaryKind| NewEra {
        source: SourceId::ModelsDev,
        provider_id: "anthropic".into(),
        model_id: "claude-sonnet-4-5".into(),
        fact_key: key.clone(),
        value_json: value.to_string(),
        boundary_at: Timestamp(at),
        boundary_kind: kind,
        observation_id: None,
    };

    store
        .append_eras(&[era(r#"{"units":3000000000}"#, 1_000, BoundaryKind::Seed)])
        .unwrap();
    store
        .append_eras(&[era(
            r#"{"units":3000000000}"#,
            5_000,
            BoundaryKind::Observed,
        )])
        .unwrap();

    // The most aggressive correction the store will accept: it reaches right up
    // to the instant it is recorded at. `append_eras` refuses anything further.
    const RECORDED_AT: i64 = 9_000;
    store
        .append_eras(&[era(
            r#"{"units":3000000000}"#,
            RECORDED_AT,
            BoundaryKind::Corrected(Correction {
                fields: vec![FieldId::Rate {
                    class: TokenClass::Input,
                }],
                affected_from: Timestamp(0),
                affected_until: Timestamp(RECORDED_AT),
                reason: "everything up to this instant was wrong".to_string(),
            }),
        )])
        .unwrap();

    // Inside the window: refused, which is the whole point of a correction.
    assert!(
        matches!(
            store
                .value_at(
                    SourceId::ModelsDev,
                    "anthropic",
                    "claude-sonnet-4-5",
                    &key,
                    Timestamp(3_000)
                )
                .unwrap(),
            PointInTime::Corrected { .. }
        ),
        "a read inside the corrected window must refuse, or this test proves nothing"
    );

    // At and after the recording instant: answerable. A consumer pricing
    // traffic as it happens is never denied by a correction.
    for now in [
        RECORDED_AT + 1,
        RECORDED_AT + 1_000,
        RECORDED_AT + 86_400_000,
    ] {
        let read = store
            .value_at(
                SourceId::ModelsDev,
                "anthropic",
                "claude-sonnet-4-5",
                &key,
                Timestamp(now),
            )
            .unwrap();
        assert!(
            matches!(read, PointInTime::Known(_)),
            "a read at now (+{}ms) must be answerable, got {read:?}",
            now - RECORDED_AT
        );
    }
}

/// The structural reason, asserted directly: no accepted correction can reach
/// past its own recording instant.
///
/// The test above demonstrates the consequence on one history. This asserts the
/// invariant that makes it true in general, so a future change loosening the
/// guard fails here with a message naming what it broke rather than showing up
/// as a consumer outage.
#[test]
fn no_correction_may_reach_past_the_instant_it_is_recorded_at() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let key = FactKey::rate(TokenClass::Input);

    store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: key.clone(),
            value_json: r#"{"units":3000000000}"#.into(),
            boundary_at: Timestamp(1_000),
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        }])
        .unwrap();

    // One millisecond past the recording instant is enough to be refused.
    let err = store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: key,
            value_json: r#"{"units":3000000000}"#.into(),
            boundary_at: Timestamp(9_000),
            boundary_kind: BoundaryKind::Corrected(Correction {
                fields: vec![FieldId::Rate {
                    class: TokenClass::Input,
                }],
                affected_from: Timestamp(1_000),
                affected_until: Timestamp(9_001),
                reason: "reaches one millisecond into the future".to_string(),
            }),
            observation_id: None,
        }])
        .expect_err("a correction reaching past its recording instant must be refused");

    assert!(
        format!("{err}").contains("has not reached"),
        "the refusal must say why: {err}"
    );
}
