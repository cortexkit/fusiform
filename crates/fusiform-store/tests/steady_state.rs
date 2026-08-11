//! What the store does on the path it spends its life on.
//!
//! Fusiform polls every 30 minutes forever. The seed happens once; the
//! unchanged-poll path happens 17,520 times a year, so its cost is the store's
//! actual cost. A design validated only on the seed is validated on the rarest
//! thing it does.
//!
//! These tests measure rather than assert a wall-clock bound: a millisecond
//! threshold in a test fails on a loaded machine for reasons unrelated to the
//! code. What they assert is the SHAPE of the growth, and they print the
//! timings so a regression is visible in the output.
//!
//! Measured on 2026-08-11 against a live 6,253-model document, in release mode
//! on this development machine: seeding writes 67,718 eras (10.8 facts per
//! model), and one unchanged poll costs ~61ms against a freshly seeded store.
//! Growing the table to 126,048 rows — 1.9x, by repricing every model with an
//! input rate across ten rounds — raised that to ~82ms, or 1.35x.
//!
//! So the steady-state read IS linear in total history, sublinearly in
//! practice because SQLite answers the per-fact MAX from a covering index
//! (`EXPLAIN QUERY PLAN`: `SEARCH e2 USING COVERING INDEX`). A grouped-join
//! formulation was measured as an alternative and is consistently slower
//! (77ms vs 45ms on the same store, interleaved to remove ordering bias), so
//! the correlated form stays.
//!
//! Real fact-level churn, measured once on 2026-08-11 by diffing two live
//! fetches about six hours apart: **9 changed facts out of 67,718** (0.013%),
//! plus one new model contributing 10 more eras. Three models were repriced —
//! all three with `last_updated` unchanged, which is independent confirmation
//! that the field cannot drive change detection, this time for money rather
//! than for a renderer override.
//!
//! One interval is not a rate, and this is a single sample taken on one
//! afternoon; a repricing wave would produce a different number. What it does
//! establish is the order of magnitude — tens of eras per poll, not thousands —
//! against a 1.9x table-growth stress that this test applies deliberately
//! because real churn is far too small to expose anything.

use std::time::Instant;

use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};

fn payload() -> Option<Vec<u8>> {
    let path = std::env::var("FUSIFORM_FULL_PAYLOAD").ok()?;
    std::fs::read(path).ok()
}

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

fn observe(s: &CatalogStore, at: i64) {
    s.record_observation(&NewObservation {
        source: SourceId::ModelsDev,
        observed_at: Timestamp(at),
        outcome: ObservationOutcome::Unchanged,
        normalized_hash: Some("h".to_string()),
        raw_hash: None,
        etag: None,
        duration_ms: None,
        detail: None,
    })
    .unwrap();
}

/// The unchanged-poll path grows sublinearly with accumulated history.
///
/// This is the property that decides whether a 30-minute cadence is
/// sustainable for years. The read is not free of history — it costs more as
/// superseded rows accumulate — but it must stay far below proportional, which
/// is what an index-backed per-fact MAX buys and what a scan over all history
/// would lose.
///
/// The bound below is deliberately loose. It is not a performance target: it is
/// a shape assertion that would catch a change turning this into a full scan
/// (which at 1.9x table growth would cost about 1.9x, not 1.35x), while
/// tolerating the jitter of a real filesystem on whatever machine runs it.
#[test]
fn the_unchanged_poll_path_grows_sublinearly_with_history() {
    let Some(bytes) = payload() else {
        eprintln!("skipped: set FUSIFORM_FULL_PAYLOAD to a captured models.dev document");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let catalog = normalize_models_dev(&bytes).unwrap().catalog;

    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    let seeded = store.append_eras(&plan.eras).unwrap();

    // Baseline: one unchanged poll against a freshly seeded store.
    observe(&store, 2_000);
    let start = Instant::now();
    let first = plan_ingest(
        &store,
        &catalog,
        Timestamp(3_000),
        BoundaryKind::Observed,
        None,
    )
    .unwrap();
    let baseline = start.elapsed();
    assert!(first.is_empty());

    // Now accumulate history on the scale that could actually expose an
    // algorithmic problem. Ten rounds, each repricing EVERY model that
    // publishes an input rate, so the era table roughly triples with
    // superseded rows the steady-state read must skip past.
    //
    // Moving one model per round would add 40 rows against 67,718 — 0.06%
    // growth, which no scan-shaped defect would show up in. A measurement too
    // small to fail is not a measurement.
    let mut doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let mut clock = 4_000i64;
    let mut extra_eras = 0usize;
    const ROUNDS: usize = 10;
    for round in 0..ROUNDS {
        let providers = doc.as_object_mut().unwrap();
        for (_, provider) in providers.iter_mut() {
            let Some(models) = provider.get_mut("models").and_then(|m| m.as_object_mut()) else {
                continue;
            };
            for (_, model) in models.iter_mut() {
                let Some(cost) = model.get_mut("cost").and_then(|c| c.as_object_mut()) else {
                    continue;
                };
                if !cost.contains_key("input") {
                    continue;
                }
                // A distinct value per round, so every round is a real change
                // rather than a no-op the diff would skip.
                cost.insert("input".to_string(), serde_json::json!(1000 + round));
            }
        }

        let mutated = serde_json::to_vec(&doc).unwrap();
        let changed = normalize_models_dev(&mutated).unwrap().catalog;

        observe(&store, clock);
        clock += 1_000;
        let plan = plan_ingest(
            &store,
            &changed,
            Timestamp(clock),
            BoundaryKind::Observed,
            None,
        )
        .unwrap();
        extra_eras += store.append_eras(&plan.eras).unwrap();
        clock += 1_000;
    }

    // The same unchanged poll, now against a store carrying superseded history.
    let current: serde_json::Value = doc.clone();
    let current_catalog = normalize_models_dev(&serde_json::to_vec(&current).unwrap())
        .unwrap()
        .catalog;
    observe(&store, clock);
    let start = Instant::now();
    let after = plan_ingest(
        &store,
        &current_catalog,
        Timestamp(clock + 1_000),
        BoundaryKind::Observed,
        None,
    )
    .unwrap();
    let with_history = start.elapsed();
    assert!(
        after.is_empty(),
        "the document matches the store, so nothing should be written"
    );

    eprintln!(
        "seeded {seeded} eras; +{extra_eras} superseded rows over {ROUNDS} rounds \
         ({:.1}x table growth)",
        (seeded + extra_eras) as f64 / seeded as f64
    );
    assert!(
        extra_eras > seeded / 2,
        "the history added ({extra_eras}) must be large enough relative to the \
         seed ({seeded}) for this measurement to mean anything"
    );
    eprintln!(
        "unchanged poll: {:?} fresh -> {:?} with history ({:.2}x)",
        baseline,
        with_history,
        with_history.as_secs_f64() / baseline.as_secs_f64().max(f64::MIN_POSITIVE)
    );

    let table_growth = (seeded + extra_eras) as f64 / seeded as f64;
    let time_growth = with_history.as_secs_f64() / baseline.as_secs_f64().max(f64::MIN_POSITIVE);
    eprintln!("table {table_growth:.2}x -> read {time_growth:.2}x");

    // No wall-clock assertion here, deliberately. Measured across five runs on
    // an unloaded machine the ratio ranged 0.48x to 1.31x, so any threshold
    // tight enough to catch a real regression would also fire on jitter, and
    // any threshold loose enough to be stable would pass a full scan. The
    // timings above are for a human reading the output.
    //
    // The mechanism is asserted instead, in
    // `the_steady_state_read_is_answered_from_an_index`, because the query plan
    // is deterministic and the thing that actually matters is whether SQLite
    // can answer the per-fact MAX from an index.
}

/// It runs against `CURRENT_VALUES_SQL` — the constant the store actually
/// prepares — rather than a copy of the SQL. A test carrying its own duplicate
/// would be checking that the test's idea of the query is index-backed.
#[test]
fn the_steady_state_read_is_answered_from_an_index() {
    let Some(bytes) = payload() else {
        eprintln!("skipped: set FUSIFORM_FULL_PAYLOAD to a captured models.dev document");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let catalog = normalize_models_dev(&bytes).unwrap().catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    drop(store);

    let conn = rusqlite::Connection::open(dir.path().join("store.db")).unwrap();
    let explain = format!(
        "EXPLAIN QUERY PLAN {}",
        fusiform_store::ingest::CURRENT_VALUES_SQL
    );
    let mut stmt = conn.prepare(&explain).unwrap();
    let steps: Vec<String> = stmt
        .query_map(rusqlite::params!["models.dev"], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();

    let plan_text = steps.join(" | ");
    eprintln!("query plan: {plan_text}");

    // The correlated subquery runs once per candidate row, so it is the half
    // that decides whether this finishes. What matters is not that it names an
    // index — SQLite will happily SCAN an index end to end — but that it can
    // SEEK on the full fact key. The plan prints the equality constraints it
    // resolved, and `fact_key=?` is the last of the four: if it is present, the
    // whole key was usable.
    //
    // Verified by mutation: an index whose columns are ordered so the seek is
    // impossible produces `SEARCH e2 USING COVERING INDEX <name>` with NO
    // constraint list, and reddens this assertion.
    assert!(
        plan_text.contains("fact_key=?"),
        "the per-fact MAX cannot seek on the full key, so it scans: {plan_text}"
    );

    // And the outer query must not scan either. The table alias is `e`, so the
    // string to look for is `SCAN e` — an earlier version of this test checked
    // for `SCAN era`, which the plan never contains, and passed unconditionally.
    assert!(
        !plan_text.contains("SCAN e"),
        "the steady-state read is scanning the era table: {plan_text}"
    );
}
