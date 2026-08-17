//! The operator correction path, against a real store.
//!
//! The scenario throughout is the one that motivated this: a parser defect ran
//! for a while, was fixed, and a subsequent poll wrote the right value. What
//! remains wrong is the record of the window in between.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{
    BoundaryKind, CapabilityId, FieldId, LimitId, ObservationOutcome, SourceId, Timestamp,
    TokenClass,
};
use fusiform_store::correct::{apply_correction, plan_correction, CorrectionRefusal};
use fusiform_store::PointInTime;
use fusiform_store::{CatalogStore, FactKey, NewEra, NewObservation};

const BAD_FROM: i64 = 3_000;
const FIXED_AT: i64 = 8_000;
const NOW: i64 = 10_000;

struct Fixture {
    store: CatalogStore,
    _dir: tempfile::TempDir,
}

/// A history with a defect in it: a bad value from t=3000, the fix landing at
/// t=8000 and a poll writing the right value.
fn fixture() -> Fixture {
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

    for (at, outcome) in [
        (1_000, ObservationOutcome::Seeded),
        (BAD_FROM, ObservationOutcome::Changed { snapshot_seq: 1 }),
        (FIXED_AT, ObservationOutcome::Changed { snapshot_seq: 2 }),
    ] {
        store
            .record_observation(&NewObservation {
                source: SourceId::ModelsDev,
                observed_at: Timestamp(at),
                outcome,
                normalized_hash: Some(format!("h{at}")),
                raw_hash: None,
                etag: None,
                duration_ms: Some(10),
                detail: None,
            })
            .unwrap();
    }

    let era = |fact: FactKey, value: &str, at: i64, kind: BoundaryKind| NewEra {
        source: SourceId::ModelsDev,
        provider_id: "anthropic".into(),
        model_id: "claude-sonnet-4-5".into(),
        fact_key: fact,
        value_json: value.to_string(),
        boundary_at: Timestamp(at),
        boundary_kind: kind,
        observation_id: None,
    };

    store
        .append_eras(&[
            // The right value, then the bad one, then the right one again.
            era(
                FactKey::rate(TokenClass::Input),
                r#"{"units":3000000000}"#,
                1_000,
                BoundaryKind::Seed,
            ),
            era(
                FactKey::rate(TokenClass::Output),
                r#"{"units":15000000000}"#,
                1_000,
                BoundaryKind::Seed,
            ),
            era(
                FactKey::limit("context"),
                "1000000",
                1_000,
                BoundaryKind::Seed,
            ),
        ])
        .unwrap();
    store
        .append_eras(&[
            era(
                FactKey::rate(TokenClass::Input),
                r#"{"units":999000000000}"#,
                BAD_FROM,
                BoundaryKind::Observed,
            ),
            era(
                FactKey::rate(TokenClass::Output),
                r#"{"units":888000000000}"#,
                BAD_FROM,
                BoundaryKind::Observed,
            ),
        ])
        .unwrap();
    store
        .append_eras(&[
            era(
                FactKey::rate(TokenClass::Input),
                r#"{"units":3000000000}"#,
                FIXED_AT,
                BoundaryKind::Observed,
            ),
            era(
                FactKey::rate(TokenClass::Output),
                r#"{"units":15000000000}"#,
                FIXED_AT,
                BoundaryKind::Observed,
            ),
        ])
        .unwrap();

    Fixture { store, _dir: dir }
}

fn rates() -> Vec<FieldId> {
    vec![
        FieldId::Rate {
            class: TokenClass::Input,
        },
        FieldId::Rate {
            class: TokenClass::Output,
        },
    ]
}

fn plan(
    f: &Fixture,
    fields: &[FieldId],
    from: i64,
    until: i64,
) -> fusiform_store::correct::CorrectionPlan {
    plan_correction(
        &f.store,
        SourceId::ModelsDev,
        "anthropic",
        "claude-sonnet-4-5",
        fields,
        Timestamp(from),
        Timestamp(until),
        "docs/findings/2026-08-12-example.md",
        Timestamp(NOW),
    )
    .expect("the store must be readable")
    .expect("this correction must plan")
}

fn refusals(f: &Fixture, fields: &[FieldId], from: i64, until: i64) -> Vec<CorrectionRefusal> {
    plan_correction(
        &f.store,
        SourceId::ModelsDev,
        "anthropic",
        "claude-sonnet-4-5",
        fields,
        Timestamp(from),
        Timestamp(until),
        "docs/findings/2026-08-12-example.md",
        Timestamp(NOW),
    )
    .expect("the store must be readable")
    .expect_err("this correction must be refused")
}

/// The plan carries the value already in force, read from the store.
#[test]
fn a_plan_takes_its_value_from_the_store() {
    let f = fixture();
    let plan = plan(&f, &rates(), BAD_FROM, FIXED_AT);

    assert_eq!(plan.rows.len(), 2);
    let input = plan
        .rows
        .iter()
        .find(|r| r.fact_key == FactKey::rate(TokenClass::Input))
        .unwrap();

    // The corrected value, not the bad one: the fix already landed.
    assert_eq!(input.value_json, r#"{"units":3000000000}"#);
    assert_eq!(input.current_since, Timestamp(FIXED_AT));
}

/// After applying, a read inside the window refuses and names the reason.
#[test]
fn applying_a_correction_makes_the_window_refuse() {
    let f = fixture();
    let plan = plan(&f, &rates(), BAD_FROM, FIXED_AT);
    let written = apply_correction(&f.store, SourceId::ModelsDev, &plan, Timestamp(NOW)).unwrap();
    assert_eq!(written, 2);

    // Inside the window: refused.
    let inside = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
            Timestamp(5_000),
        )
        .unwrap();
    match inside {
        PointInTime::Corrected { corrections } => {
            assert_eq!(corrections.len(), 1);
            assert!(corrections[0].reason.contains("2026-08-12-example"));
            assert_eq!(corrections[0].affected_from, Timestamp(BAD_FROM));
            assert_eq!(corrections[0].affected_until, Timestamp(FIXED_AT));
        }
        other => panic!("a read inside the window must refuse, got {other:?}"),
    }

    // Before it: unaffected.
    let before = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
            Timestamp(2_000),
        )
        .unwrap();
    // Unaffected BY THE CORRECTION, which is what this test is about: it must
    // not refuse. `KnownStale` is accepted because t=2000 sits inside the next
    // era's observation window, which is an unrelated and honest qualification.
    assert!(
        matches!(
            before,
            PointInTime::Known(_) | PointInTime::KnownStale { .. }
        ),
        "a read before the window must not refuse, got {before:?}"
    );

    // After it: the corrected value, served normally.
    let after = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
            Timestamp(NOW + 1),
        )
        .unwrap();
    assert_eq!(
        after.value_json(),
        Some(r#"{"units":3000000000}"#),
        "the current value must still be served"
    );
}

/// A correction does not change what the catalog currently asserts.
///
/// The operator supplies no value, so the era written carries the one already
/// in force. If that ever stops being true, an operator could change the
/// catalog through a command whose name says it is repairing history.
#[test]
fn a_correction_does_not_move_the_current_value() {
    let f = fixture();
    let before = f
        .store
        .recorded_value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
            Timestamp(NOW),
        )
        .unwrap()
        .unwrap()
        .value_json;

    let plan = plan(&f, &rates(), BAD_FROM, FIXED_AT);
    apply_correction(&f.store, SourceId::ModelsDev, &plan, Timestamp(NOW)).unwrap();

    let after = f
        .store
        .recorded_value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
            Timestamp(NOW + 1),
        )
        .unwrap()
        .unwrap()
        .value_json;

    assert_eq!(
        before, after,
        "a correction must not move the current value"
    );
}

/// A window containing the value in force is refused.
///
/// This is the case where marking the past is not enough: reads at now fall
/// outside the corrected interval, so the store would look repaired while still
/// serving the defect as current.
#[test]
fn a_window_containing_the_current_value_is_refused() {
    let f = fixture();
    // Window extends past the fix, so the era in force began inside it.
    let refusals = refusals(&f, &rates(), BAD_FROM, 9_000);

    assert_eq!(refusals.len(), 2, "both rates are still affected");
    for refusal in &refusals {
        match refusal {
            CorrectionRefusal::PresentStillAffected { current_since, .. } => {
                assert_eq!(*current_since, Timestamp(FIXED_AT));
            }
            other => panic!("expected PresentStillAffected, got {other:?}"),
        }
    }

    // And the message says what to do about it.
    let text = format!("{}", refusals[0]);
    assert!(
        text.contains("Poll first"),
        "the refusal must say how to resolve it: {text}"
    );
}

/// A fact with no history is refused rather than invented.
#[test]
fn correcting_a_fact_with_no_history_is_refused() {
    let f = fixture();
    // This model has no cache_read rate in the fixture.
    let refusals = refusals(
        &f,
        &[FieldId::Rate {
            class: TokenClass::CacheRead,
        }],
        BAD_FROM,
        FIXED_AT,
    );

    assert!(matches!(
        refusals.as_slice(),
        [CorrectionRefusal::NoSuchFact { .. }]
    ));
    assert!(format!("{}", refusals[0]).contains("check the provider and model ids"));
}

/// Every problem is reported at once.
#[test]
fn all_refusals_are_collected() {
    let f = fixture();
    let refusals = refusals(
        &f,
        &[
            // No history.
            FieldId::Rate {
                class: TokenClass::CacheRead,
            },
            // Also no history.
            FieldId::Capability {
                capability: CapabilityId::Reasoning,
            },
            // Not addressable at all.
            FieldId::TierThreshold,
        ],
        BAD_FROM,
        FIXED_AT,
    );

    assert_eq!(
        refusals.len(),
        3,
        "an operator correcting several facts wants every problem at once, \
         not one per round trip: {refusals:?}"
    );
}

/// A duplicate correction is refused.
#[test]
fn an_identical_correction_is_refused_the_second_time() {
    let f = fixture();
    let plan = plan(&f, &rates(), BAD_FROM, FIXED_AT);
    apply_correction(&f.store, SourceId::ModelsDev, &plan, Timestamp(NOW)).unwrap();

    let refusals = refusals(&f, &rates(), BAD_FROM, FIXED_AT);
    assert_eq!(refusals.len(), 2);
    assert!(refusals
        .iter()
        .all(|r| matches!(r, CorrectionRefusal::AlreadyRecorded { .. })));
}

/// A correction to a non-rate fact works, since a defect is not always money.
#[test]
fn a_capability_or_limit_can_be_corrected() {
    let f = fixture();

    // The fixture seeds limit.context at t=1000 and never moves it, so a
    // correction would be refused for the right reason. Give it the same shape
    // the rates have: a bad value during the window, the right one after the
    // fix.
    let era = |value: &str, at: i64, kind: BoundaryKind| NewEra {
        source: SourceId::ModelsDev,
        provider_id: "anthropic".into(),
        model_id: "claude-sonnet-4-5".into(),
        fact_key: FactKey::limit("context"),
        value_json: value.to_string(),
        boundary_at: Timestamp(at),
        boundary_kind: kind,
        observation_id: None,
    };
    f.store
        .append_eras(&[era("999", BAD_FROM, BoundaryKind::Observed)])
        .unwrap();
    f.store
        .append_eras(&[era("1000000", FIXED_AT, BoundaryKind::Observed)])
        .unwrap();

    let plan = plan(
        &f,
        &[FieldId::Limit {
            limit: LimitId::Context,
        }],
        BAD_FROM,
        FIXED_AT,
    );
    assert_eq!(plan.rows.len(), 1);
    assert_eq!(plan.rows[0].fact_key, FactKey::limit("context"));

    apply_correction(&f.store, SourceId::ModelsDev, &plan, Timestamp(NOW)).unwrap();

    let inside = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::limit("context"),
            Timestamp(5_000),
        )
        .unwrap();
    assert!(
        matches!(inside, PointInTime::Corrected { .. }),
        "a limit correction must refuse inside its window"
    );
}

/// A correction spanning several facts writes one shared extent.
///
/// Each row must name its own fact within that extent, which the store
/// enforces. This checks the writer produces extents that satisfy it, rather
/// than the check passing because the writer only ever corrects one fact.
#[test]
fn a_multi_fact_correction_shares_one_extent() {
    let f = fixture();
    let plan = plan(&f, &rates(), BAD_FROM, FIXED_AT);
    apply_correction(&f.store, SourceId::ModelsDev, &plan, Timestamp(NOW)).unwrap();

    for class in [TokenClass::Input, TokenClass::Output] {
        let corrections = f
            .store
            .corrections_covering(
                SourceId::ModelsDev,
                "anthropic",
                "claude-sonnet-4-5",
                &FactKey::rate(class),
                Timestamp(5_000),
            )
            .unwrap();
        assert_eq!(corrections.len(), 1, "{class:?} must be corrected");
        // The extent names both rates on every row.
        assert!(
            corrections[0].fields_json.contains("input")
                && corrections[0].fields_json.contains("output"),
            "each row carries the whole defect's extent: {}",
            corrections[0].fields_json
        );
    }
}

/// A fact whose value has not moved since before the window is refused.
///
/// The era in force began BEFORE the window and spans it, so the same value was
/// in force during the window and is still being served. Marking that window
/// wrong while serving the identical value now is incoherent: either the fix
/// has not landed, or nothing was ever wrong.
///
/// Found by driving a real correction against a live daemon, where the natural
/// first attempt — correct from the seed instant to the latest poll — hit
/// exactly this on a fact whose value had not changed. The first version of the
/// guard tested only whether the era began INSIDE the window and let this
/// through.
#[test]
fn a_fact_unchanged_since_before_the_window_is_refused() {
    let f = fixture();

    // limit.context was seeded at t=1000 and never moved. A window from 3000 to
    // 8000 sits entirely inside that era.
    let refusals = refusals(
        &f,
        &[FieldId::Limit {
            limit: LimitId::Context,
        }],
        BAD_FROM,
        FIXED_AT,
    );

    assert_eq!(refusals.len(), 1);
    match &refusals[0] {
        CorrectionRefusal::PresentStillAffected { current_since, .. } => {
            assert_eq!(
                *current_since,
                Timestamp(1_000),
                "the era in force predates the window"
            );
        }
        other => panic!("expected PresentStillAffected, got {other:?}"),
    }
}

/// A poll landing between plan and apply is refused, not overwritten.
///
/// `apply_correction` takes a plan so the rows written are the rows the
/// operator was shown. On its own that is dangerous: the poll loop writes to
/// the same store every 30 minutes, and reading a preview before typing
/// `--commit` easily takes longer than nothing.
///
/// Found with a probe rather than by reasoning. A poll landing in that gap left
/// the plan carrying a value that was no longer current, and applying it wrote
/// that stale value at a LATER boundary — silently reverting a real upstream
/// reprice, wearing a `Corrected` boundary that reads as a repair. The one
/// thing this module exists not to do, reached through the mechanism chosen to
/// prevent it.
#[test]
fn a_poll_between_plan_and_apply_is_refused() {
    let f = fixture();
    let plan = plan(&f, &rates(), BAD_FROM, FIXED_AT);

    // A poll lands while the operator reads the preview: the upstream repriced.
    f.store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(NOW - 1),
            outcome: ObservationOutcome::Changed { snapshot_seq: 9 },
            normalized_hash: Some("later".into()),
            raw_hash: None,
            etag: None,
            duration_ms: Some(10),
            detail: None,
        })
        .unwrap();
    f.store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: FactKey::rate(TokenClass::Input),
            value_json: r#"{"units":2500000000}"#.into(),
            boundary_at: Timestamp(NOW - 1),
            boundary_kind: BoundaryKind::Observed,
            observation_id: None,
        }])
        .unwrap();

    let err = apply_correction(&f.store, SourceId::ModelsDev, &plan, Timestamp(NOW))
        .expect_err("a stale plan must be refused");
    let text = format!("{err}");
    assert!(
        text.contains("changed after this correction was planned"),
        "the refusal must say what happened: {text}"
    );
    assert!(text.contains("revert"), "and why it matters: {text}");

    // The upstream's value survives untouched.
    let current = f
        .store
        .recorded_value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
            Timestamp(NOW + 1),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        current.value_json, r#"{"units":2500000000}"#,
        "the poll's value must survive a refused correction"
    );
}

/// A plan applied against an unchanged store still works.
///
/// The compare-and-swap above is worthless if it also refuses the ordinary
/// case, and a guard that refuses everything passes any test written only for
/// the failure.
#[test]
fn an_unchanged_store_still_accepts_its_plan() {
    let f = fixture();
    let plan = plan(&f, &rates(), BAD_FROM, FIXED_AT);
    let written = apply_correction(&f.store, SourceId::ModelsDev, &plan, Timestamp(NOW))
        .expect("an unchanged store must accept the plan");
    assert_eq!(written, 2);
}

/// A second correction landing between plan and apply is also refused.
///
/// This is what makes the compare-and-swap compare BOUNDARIES rather than
/// values, and mutation is what showed the difference mattered. A poll cannot
/// write an unchanged value — `plan_ingest` skips facts whose value did not
/// move — so the only way the same value appears at a new boundary is another
/// correction landing on the same fact.
///
/// Refusing is right: the second operator's diagnosis was made against a
/// history that no longer exists, and their correction would now stack on top
/// of one they never saw. A value comparison would accept it silently.
#[test]
fn a_second_correction_between_plan_and_apply_is_refused() {
    let f = fixture();

    // Two operators plan the same correction from the same state.
    let first = plan(&f, &rates(), BAD_FROM, FIXED_AT);
    let second = plan(&f, &rates(), BAD_FROM, FIXED_AT);

    apply_correction(&f.store, SourceId::ModelsDev, &first, Timestamp(NOW)).unwrap();

    // The second plan carries the same VALUES — a correction writes the value
    // already in force — but the era in force has moved.
    let err = apply_correction(&f.store, SourceId::ModelsDev, &second, Timestamp(NOW + 1))
        .expect_err("a plan made against superseded history must be refused");
    assert!(
        format!("{err}").contains("changed after this correction was planned"),
        "got {err}"
    );
}

/// A correction with nothing to repair is refused.
///
/// # The misuse this closes
///
/// A correction WITHHOLDS: reads inside its interval refuse rather than serve
/// the recorded value. That is right when the value was wrong — the commons
/// tier rows recorded a threshold of 0 and a consumer must not use them.
///
/// It is wrong when the value is fine and only the BOUNDARY is a fiction. On
/// 2026-08-16 fusiform wrote 17,455 eras carrying identical values on both
/// sides, because a serialization change was compared as a value change.
/// Correcting one would replace a spurious change event with a refusal to
/// answer — strictly worse, since the value was serving correctly.
///
/// §3.1 of the design note invites exactly this mistake: it says a correction
/// exists to state that "the upstream did not move", which describes both
/// defects while the remedy fits only one. The note now warns, and prose is
/// what this repo keeps proving unreliable, so the store refuses.
#[test]
fn a_correction_with_nothing_to_repair_is_refused() {
    let f = fixture();
    let key = FactKey::rate(TokenClass::Input);

    // Two eras, same value, different instants: the shape the incident left
    // behind.
    for (at, kind) in [
        (1_000i64, BoundaryKind::Seed),
        (5_000i64, BoundaryKind::Observed),
    ] {
        f.store
            .append_eras(&[NewEra {
                source: SourceId::ModelsDev,
                provider_id: "anthropic".into(),
                model_id: "probe-model-boundary-only".into(),
                fact_key: key.clone(),
                value_json:
                    r#"{"state":"priced","units":3000000000,"exponent":9,"currency":"USD"}"#
                        .to_string(),
                boundary_at: Timestamp(at),
                boundary_kind: kind,
                observation_id: None,
            }])
            .expect("the era must store");
    }

    let outcome = plan_correction(
        &f.store,
        SourceId::ModelsDev,
        "anthropic",
        "probe-model-boundary-only",
        &[FieldId::Rate {
            class: TokenClass::Input,
        }],
        Timestamp(2_000),
        Timestamp(5_000),
        "docs/findings/2026-08-16-provenance-rewrote-the-rate-plane.md",
        Timestamp(9_000),
    )
    .expect("planning must not error");

    let refusals = outcome.expect_err("a correction with nothing to repair must be refused");
    let text = refusals
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        text.contains("withhold a value that is right"),
        "the refusal must say why, and point at what the command cannot do: {text}"
    );

    // CONTROL: a genuine repair — the value DID change — must still plan.
    f.store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "probe-model-boundary-only".into(),
            fact_key: key.clone(),
            value_json: r#"{"state":"priced","units":9000000000,"exponent":9,"currency":"USD"}"#
                .to_string(),
            boundary_at: Timestamp(6_000),
            boundary_kind: BoundaryKind::Observed,
            observation_id: None,
        }])
        .expect("the repaired era must store");

    let outcome = plan_correction(
        &f.store,
        SourceId::ModelsDev,
        "anthropic",
        "probe-model-boundary-only",
        &[FieldId::Rate {
            class: TokenClass::Input,
        }],
        Timestamp(2_000),
        Timestamp(6_000),
        "docs/findings/2026-08-11-commons-tier-threshold.md",
        Timestamp(9_000),
    )
    .expect("planning must not error");
    assert!(
        outcome.is_ok(),
        "control: a correction sitting behind a genuinely repaired value must \
         still plan, or this guard has blocked the mechanism's real use: {:?}",
        outcome.err()
    );
}
