//! PROBE: what does a restore do to HISTORY, not just to current values?
//!
//! My restore sweep (§9) says era rows are "self-healing: the next poll diffs
//! against the restored state and rewrites what moved". ASTRO's sweep found a
//! category mine did not list — state that cannot be re-derived because the
//! upstream does not serve it — and their instance is historical rate rows.
//!
//! Fusiform's entire purpose is that history. models.dev serves today's
//! document and nothing else, so if a restore loses eras, re-polling cannot
//! bring them back. The question this prints: what does a point-in-time read
//! inside the rewound interval actually return?
//!
//! The probe found the answer was `Known($3)` with no qualification, and that
//! the uncertainty was computable from rows the store already held. These are
//! the guards for the fix.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_store::{CatalogStore, FactKey, NewEra, NewObservation, PointInTime};

fn open(dir: &std::path::Path) -> CatalogStore {
    CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.join("store.db").to_string_lossy().to_string(),
        },
    })
    .unwrap()
}

const DAY: i64 = 86_400_000;

fn observe(store: &CatalogStore, at: i64) -> i64 {
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(at),
            outcome: ObservationOutcome::Changed { snapshot_seq: at },
            normalized_hash: Some(format!("h{at}")),
            raw_hash: None,
            etag: None,
            duration_ms: Some(10),
            detail: None,
        })
        .unwrap()
}

fn write_rate(store: &CatalogStore, at: i64, units: i64, kind: BoundaryKind, obs: Option<i64>) {
    store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: FactKey::rate(fusiform_core::TokenClass::Input),
            value_json: format!(r#"{{"state":"priced","units":{units}}}"#),
            boundary_at: Timestamp(at),
            boundary_kind: kind,
            observation_id: obs,
        }])
        .unwrap();
}

fn read(store: &CatalogStore, at: i64) -> PointInTime {
    store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(fusiform_core::TokenClass::Input),
            Timestamp(at),
        )
        .unwrap()
}

#[test]
fn a_read_inside_a_later_eras_window_is_marked_stale() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());

    // Day 1: $3, seeded. Day 13: $4, observed, with the previous confirming
    // observation on day 1 -- the shape a poll gap produces, and exactly what a
    // restore that loses history leaves behind.
    observe(&store, DAY);
    write_rate(&store, DAY, 3_000_000_000, BoundaryKind::Seed, None);
    let o13 = observe(&store, 13 * DAY);
    write_rate(
        &store,
        13 * DAY,
        4_000_000_000,
        BoundaryKind::Observed,
        Some(o13),
    );

    // Day 9 is inside (day 1, day 13]. The store holds $3 for it and cannot
    // support that as a confident answer: the change happened somewhere in
    // that interval and fusiform was not looking.
    match read(&store, 9 * DAY) {
        PointInTime::KnownStale {
            row,
            superseded_after,
            superseded_by,
        } => {
            assert!(row.value_json.contains("3000000000"));
            assert_eq!(
                superseded_after.0, DAY,
                "the last confirmation before the change"
            );
            assert_eq!(
                superseded_by.0,
                13 * DAY,
                "the observation that detected it"
            );
        }
        other => panic!("a read inside a later era's window must carry the bracket, got {other:?}"),
    }

    // After the change: no later window, so no uncertainty.
    assert!(
        matches!(read(&store, 14 * DAY), PointInTime::Known(_)),
        "a read after the newest era is not uncertain"
    );

    // AT the boundary instant the new era starts: the new value, confidently.
    // The interval is open at the top -- `superseded_by` is when fusiform SAW
    // the change, so the new value is definitely in force from there.
    assert!(
        matches!(read(&store, 13 * DAY), PointInTime::Known(_)),
        "the boundary instant itself is certain"
    );
}

/// A current-catalog read is never marked stale.
///
/// The newest era has no later window by construction, so the common path is
/// unaffected. This matters because the marking would be noise on every read if
/// it fired there, and a qualification that fires everywhere is one nobody
/// reads.
#[test]
fn the_current_value_is_never_stale() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());

    observe(&store, DAY);
    write_rate(&store, DAY, 3_000_000_000, BoundaryKind::Seed, None);
    let o13 = observe(&store, 13 * DAY);
    write_rate(
        &store,
        13 * DAY,
        4_000_000_000,
        BoundaryKind::Observed,
        Some(o13),
    );

    for at in [13 * DAY, 20 * DAY, 999 * DAY] {
        assert!(
            matches!(read(&store, at), PointInTime::Known(_)),
            "a read at or after the newest era must be Known, at day {}",
            at / DAY
        );
    }
}

/// A tight poll cadence produces tight brackets, not blanket uncertainty.
///
/// The marking is proportional to the real gap: with a 30-minute cadence the
/// bracket is 30 minutes wide, which is a small and honest qualification. It
/// only becomes dramatic when the gap is dramatic -- which is the case worth
/// reporting.
#[test]
fn the_bracket_is_as_wide_as_the_real_gap() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let half_hour = 1_800_000i64;

    observe(&store, DAY);
    write_rate(&store, DAY, 3_000_000_000, BoundaryKind::Seed, None);
    let next = observe(&store, DAY + half_hour);
    write_rate(
        &store,
        DAY + half_hour,
        4_000_000_000,
        BoundaryKind::Observed,
        Some(next),
    );

    match read(&store, DAY + half_hour / 2) {
        PointInTime::KnownStale {
            superseded_after,
            superseded_by,
            ..
        } => assert_eq!(
            superseded_by.0 - superseded_after.0,
            half_hour,
            "the bracket must be the actual observation gap"
        ),
        other => panic!("expected a bracketed read, got {other:?}"),
    }
}

/// Three eras, so "the next era" and "the newest era" are different rows.
///
/// Every earlier test here has two eras, which makes three distinct mutations
/// undetectable: with one era after the read, `ORDER BY boundary ASC` and
/// `DESC` select the same row, and both interval endpoints coincide with era
/// edges so an off-by-one at either end is invisible. All three survived the
/// suite until this existed.
///
/// The shape: seed at day 1, a change observed at day 13 (prior day 1), and
/// another observed at day 20 (prior day 19). A read on day 9 must bracket
/// against the DAY 13 era, not the day 20 one.
#[test]
fn the_bracket_comes_from_the_next_era_and_excludes_both_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());

    observe(&store, DAY);
    write_rate(&store, DAY, 3_000_000_000, BoundaryKind::Seed, None);
    let o13 = observe(&store, 13 * DAY);
    write_rate(
        &store,
        13 * DAY,
        4_000_000_000,
        BoundaryKind::Observed,
        Some(o13),
    );
    observe(&store, 19 * DAY);
    let o20 = observe(&store, 20 * DAY);
    write_rate(
        &store,
        20 * DAY,
        5_000_000_000,
        BoundaryKind::Observed,
        Some(o20),
    );

    // Day 9 brackets against the NEXT era (day 13), not the newest (day 20).
    match read(&store, 9 * DAY) {
        PointInTime::KnownStale {
            superseded_after,
            superseded_by,
            ..
        } => {
            assert_eq!(superseded_after.0, DAY);
            assert_eq!(
                superseded_by.0,
                13 * DAY,
                "the bracket must come from the next era, not the newest"
            );
        }
        other => panic!("day 9 must be bracketed, got {other:?}"),
    }

    // The interval excludes BOTH endpoints, and with three eras each endpoint
    // is now testable independently.
    //
    // Day 1 is the instant the value was confirmed, so it is certain — even
    // though it is the lower edge of the next era's window.
    assert!(
        matches!(read(&store, DAY), PointInTime::Known(_)),
        "the prior-confirmation instant is certain, got {:?}",
        read(&store, DAY)
    );

    // Day 13 is the instant the change was observed. The day-13 era is in
    // force from there, and the day-20 era's window is (19, 20] which does not
    // contain it, so this must be Known rather than bracketed.
    assert!(
        matches!(read(&store, 13 * DAY), PointInTime::Known(_)),
        "the boundary instant is certain, got {:?}",
        read(&store, 13 * DAY)
    );

    // And day 19.5 brackets against the day-20 era, proving the lookup follows
    // the read instant rather than always finding the same row.
    match read(&store, 19 * DAY + DAY / 2) {
        PointInTime::KnownStale {
            superseded_after,
            superseded_by,
            ..
        } => {
            assert_eq!(superseded_after.0, 19 * DAY);
            assert_eq!(superseded_by.0, 20 * DAY);
        }
        other => panic!("day 19.5 must bracket against the day-20 era, got {other:?}"),
    }
}
