//! The read surface, against a real store.
//!
//! The cases here are the ones where a plausible implementation gives a
//! confidently wrong answer rather than an error: a point-in-time read that
//! silently returns current values, a retired model that keeps its last price
//! forever, a filtered read that makes every model look retired.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::serve::{CatalogQuery, POINT_IN_TIME_SQL};
use fusiform_store::{CatalogStore, FactKey, NewObservation};

const FIXTURE: &str = include_str!("../../fusiform-core/fixtures/models-dev-excerpt.json");

struct Fixture {
    store: CatalogStore,
    dir: tempfile::TempDir,
}

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
    Fixture { store, dir }
}

fn observe(f: &Fixture, at: i64) -> i64 {
    f.store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(at),
            outcome: ObservationOutcome::Unchanged,
            normalized_hash: Some("h".to_string()),
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap()
}

/// Seed the store from the fixture, then apply a mutated document as a change.
fn seed(f: &Fixture, at: i64) {
    let catalog = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(&f.store, &catalog, Timestamp(at), BoundaryKind::Seed, None).unwrap();
    f.store.append_eras(&plan.eras).unwrap();
}

fn apply(f: &Fixture, doc: &serde_json::Value, at: i64) {
    let bytes = serde_json::to_vec(doc).unwrap();
    let catalog = normalize_models_dev(&bytes).unwrap().catalog;
    let obs = observe(f, at - 500);
    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(at),
        BoundaryKind::Observed,
        Some(obs),
    )
    .unwrap();
    f.store.append_eras(&plan.eras).unwrap();
}

fn doc() -> serde_json::Value {
    serde_json::from_str(FIXTURE).unwrap()
}

fn set_rate(doc: &mut serde_json::Value, provider: &str, model: &str, key: &str, value: f64) {
    doc.get_mut(provider)
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.get_mut(model))
        .and_then(|m| m.get_mut("cost"))
        .and_then(|c| c.as_object_mut())
        .expect("the fixture must carry this model's cost block")
        .insert(key.to_string(), serde_json::json!(value));
}

/// A point-in-time read returns what the catalog said THEN, not now.
///
/// The failure this guards is a read that resolves each fact's latest boundary
/// overall and then discards anything after the instant — which returns nothing
/// for every fact that has since moved, while looking correct for facts that
/// have not.
#[test]
fn a_point_in_time_read_returns_the_value_in_force_then() {
    let f = fixture();
    seed(&f, 1_000);
    observe(&f, 2_000);

    let mut d = doc();
    set_rate(&mut d, "anthropic", "claude-sonnet-4-5", "input", 9.0);
    apply(&f, &d, 3_000);

    let key = FactKey::rate(fusiform_core::TokenClass::Input);

    // Before the change: the seeded rate.
    let then = f
        .store
        .read_model(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            Some(Timestamp(2_500)),
        )
        .unwrap()
        .expect("the model existed then");
    let value = then.facts.get(&key).expect("the rate was in force then");
    assert!(
        value.contains("3000000000"),
        "expected the seeded $3.00 rate, got {value}"
    );

    // After: the new one.
    let now = f
        .store
        .read_model(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            Some(Timestamp(9_000)),
        )
        .unwrap()
        .unwrap();
    let value = now.facts.get(&key).unwrap();
    assert!(
        value.contains("9000000000"),
        "expected the repriced $9.00 rate, got {value}"
    );

    // And every OTHER fact still resolves at the earlier instant, rather than
    // vanishing because its own latest boundary is now later.
    assert!(
        then.facts.len() > 5,
        "a point-in-time read must return the model's whole fact set, got {}",
        then.facts.len()
    );
    assert_eq!(
        then.facts.len(),
        now.facts.len(),
        "the same facts exist at both instants; only a value moved"
    );
}

/// Facts that moved at different times resolve independently.
///
/// The case a single "snapshot id" design gets wrong: a model repriced at one
/// instant and re-limited at another has two different answers at any instant
/// between, and both must be correct in one read.
#[test]
fn facts_that_moved_at_different_times_resolve_independently() {
    let f = fixture();
    seed(&f, 1_000);
    observe(&f, 2_000);

    let mut d = doc();
    set_rate(&mut d, "anthropic", "claude-sonnet-4-5", "input", 9.0);
    apply(&f, &d, 3_000);

    // A second change, later, to a different fact.
    d.get_mut("anthropic")
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.get_mut("claude-sonnet-4-5"))
        .and_then(|m| m.get_mut("limit"))
        .and_then(|l| l.as_object_mut())
        .unwrap()
        .insert("output".to_string(), serde_json::json!(128_000));
    apply(&f, &d, 5_000);

    let between = f
        .store
        .read_model(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            Some(Timestamp(4_000)),
        )
        .unwrap()
        .unwrap();

    // The rate has moved by now; the limit has not.
    assert!(between
        .facts
        .get(&FactKey::rate(fusiform_core::TokenClass::Input))
        .unwrap()
        .contains("9000000000"));
    assert_eq!(
        between.facts.get(&FactKey::limit("output")).unwrap(),
        "64000",
        "the limit had not moved yet at this instant"
    );
}

/// A retired model is excluded by default and available on request.
#[test]
fn a_retired_model_is_excluded_by_default() {
    let f = fixture();
    seed(&f, 1_000);
    observe(&f, 2_000);

    // Remove a model from the document: the upstream stopped describing it.
    let mut d = doc();
    d.get_mut("anthropic")
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.as_object_mut())
        .unwrap()
        .remove("claude-sonnet-4-5");
    apply(&f, &d, 3_000);

    let present = f
        .store
        .read_catalog(&CatalogQuery::current(SourceId::ModelsDev))
        .unwrap();
    assert!(
        !present
            .models
            .iter()
            .any(|m| m.model_id == "claude-sonnet-4-5"),
        "a retired model is not part of what models exist"
    );

    let all = f
        .store
        .read_catalog(&CatalogQuery::current(SourceId::ModelsDev).including_retired())
        .unwrap();
    let retired = all
        .models
        .iter()
        .find(|m| m.model_id == "claude-sonnet-4-5")
        .expect("an audit read must still find it");
    assert!(!retired.is_present());
    assert_eq!(
        retired.facts.get(&FactKey::existence()).unwrap(),
        "\"absent\""
    );

    // And it was present before the withdrawal, so the tombstone has a boundary
    // rather than the model having simply never existed.
    let earlier = f
        .store
        .read_catalog(&CatalogQuery::at(SourceId::ModelsDev, Timestamp(2_500)))
        .unwrap();
    assert!(earlier
        .models
        .iter()
        .any(|m| m.model_id == "claude-sonnet-4-5"));
}

/// A model with facts but no existence fact is not present.
///
/// The store cannot produce this state through its own ingest — the planner
/// always writes existence — so the row is inserted directly. That is the
/// point: the read layer must not assume its input came from its own writer,
/// because the state it is being asked about is one where something ELSE went
/// wrong. A partial write, a truncated restore, a future migration that adds a
/// fact kind and backfills incompletely.
///
/// Defaulting to present would make such a model look real, and it would carry
/// whatever rates it happened to have. Defaulting to absent hides it, which is
/// the survivable direction: a consumer that cannot see a model asks about it,
/// while a consumer that prices against a phantom does not.
#[test]
fn a_model_with_no_existence_fact_is_not_present() {
    let f = fixture();
    seed(&f, 1_000);
    let path = f.dir.path().join("store.db");
    drop(f.store);

    // A model carrying a rate and nothing else: no existence era at all.
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO era \
         (source, provider_id, model_id, fact_key, value_json, boundary_at_ms, boundary_kind) \
         VALUES ('models.dev', 'ghost', 'phantom-1', 'rate.input', \
                 '{\"state\":\"priced\",\"units\":1000000000}', 1000, 'seed')",
        [],
    )
    .unwrap();
    drop(conn);

    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_string_lossy().to_string(),
        },
    })
    .unwrap();

    // The row exists — so this test is not passing because the insert failed.
    let all = store
        .read_catalog(&CatalogQuery::current(SourceId::ModelsDev).including_retired())
        .unwrap();
    let ghost = all
        .models
        .iter()
        .find(|m| m.model_id == "phantom-1")
        .expect("the inserted row must be readable");
    assert!(ghost
        .facts
        .contains_key(&FactKey::rate(fusiform_core::TokenClass::Input)));
    assert!(
        !ghost.facts.contains_key(&FactKey::existence()),
        "this test requires a model with no existence fact"
    );

    // And it is not present, so a default read never returns it.
    assert!(!ghost.is_present());
    let present = store
        .read_catalog(&CatalogQuery::current(SourceId::ModelsDev))
        .unwrap();
    assert!(
        !present.models.iter().any(|m| m.model_id == "phantom-1"),
        "a model with no existence fact must not appear in the catalog"
    );
}

/// A withdrawn rate stops being current instead of persisting forever.
#[test]
fn a_withdrawn_rate_does_not_stay_current() {
    let f = fixture();
    seed(&f, 1_000);
    observe(&f, 2_000);

    // The model stays; its cache_read rate stops being published.
    let mut d = doc();
    d.get_mut("anthropic")
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.get_mut("claude-sonnet-4-5"))
        .and_then(|m| m.get_mut("cost"))
        .and_then(|c| c.as_object_mut())
        .unwrap()
        .remove("cache_read");
    apply(&f, &d, 3_000);

    let key = FactKey::rate(fusiform_core::TokenClass::CacheRead);
    let now = f
        .store
        .read_model(SourceId::ModelsDev, "anthropic", "claude-sonnet-4-5", None)
        .unwrap()
        .unwrap();

    let value = now.facts.get(&key).expect("the fact still has an era");
    assert!(
        value.contains("missing_rate"),
        "a withdrawn rate must become explicitly unpriced, got {value}"
    );
    assert!(
        !value.contains("300000000"),
        "the last published rate must not still be current"
    );
}

/// A filtered read still decides presence on the full fact set.
///
/// Filtering before the presence check would drop the existence fact for a
/// rates-only read and make every model look retired — returning an empty
/// catalog that looks like a legitimate answer.
#[test]
fn a_filtered_read_does_not_make_every_model_look_retired() {
    let f = fixture();
    seed(&f, 1_000);

    let rates_only = f
        .store
        .read_catalog(&CatalogQuery::current(SourceId::ModelsDev).with_prefixes(&["rate."]))
        .unwrap();

    assert!(
        rates_only.model_count() > 5,
        "a rates-only read must still return models, got {}",
        rates_only.model_count()
    );
    for model in &rates_only.models {
        for key in model.facts.keys() {
            assert!(
                key.as_str().starts_with("rate."),
                "the filter must exclude {}",
                key.as_str()
            );
        }
    }

    // And the filter genuinely narrows: the unfiltered read carries more.
    let all = f
        .store
        .read_catalog(&CatalogQuery::current(SourceId::ModelsDev))
        .unwrap();
    assert!(
        all.fact_count() > rates_only.fact_count(),
        "the filter must remove facts, {} vs {}",
        all.fact_count(),
        rates_only.fact_count()
    );
}

/// A model with no facts in the requested plane is omitted, not returned empty.
#[test]
fn a_model_outside_the_requested_plane_is_omitted() {
    let f = fixture();
    seed(&f, 1_000);

    // The fixture carries a model with no cost object at all.
    let rates_only = f
        .store
        .read_catalog(&CatalogQuery::current(SourceId::ModelsDev).with_prefixes(&["rate."]))
        .unwrap();

    assert!(
        !rates_only
            .models
            .iter()
            .any(|m| m.model_id.contains("command-r-plus")),
        "a model with no rates must not appear in a rates-only read as an empty entry"
    );
    // It does exist in the full read, so its absence above is the filter's
    // doing rather than the model being missing.
    let all = f
        .store
        .read_catalog(&CatalogQuery::current(SourceId::ModelsDev))
        .unwrap();
    assert!(all
        .models
        .iter()
        .any(|m| m.model_id.contains("command-r-plus")));
}

/// A read for "now" records the instant it resolved at.
#[test]
fn a_read_records_the_instant_it_resolved_at() {
    let f = fixture();
    seed(&f, 1_000);

    let snapshot = f
        .store
        .read_catalog(&CatalogQuery::current(SourceId::ModelsDev))
        .unwrap();

    // A real wall-clock instant, so the same snapshot is re-askable.
    assert!(
        snapshot.resolved_at.0 > 1_700_000_000_000,
        "resolved_at must be a real instant, got {}",
        snapshot.resolved_at.0
    );

    let again = f
        .store
        .read_catalog(&CatalogQuery::at(SourceId::ModelsDev, snapshot.resolved_at))
        .unwrap();
    assert_eq!(
        snapshot.models, again.models,
        "re-asking for the recorded instant must return the same snapshot"
    );
}

/// The point-in-time query resolves each fact's maximum from the index.
///
/// Asserted on the plan rather than on wall clock: the timing is dominated by
/// machine noise, while the plan is deterministic. The failure this guards is
/// not a slow query but an unusable one — the same read written so SQLite
/// cannot use the covering index did not finish in 300 seconds on 126k rows.
#[test]
fn the_point_in_time_query_seeks_rather_than_scans() {
    let f = fixture();
    seed(&f, 1_000);
    let path = f.dir.path().join("store.db");
    drop(f.store);

    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {POINT_IN_TIME_SQL}"))
        .unwrap();
    let steps: Vec<String> = stmt
        .query_map(rusqlite::params!["models.dev", 9_999i64], |r| {
            r.get::<_, String>(3)
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let plan_text = steps.join(" | ");
    eprintln!("query plan: {plan_text}");

    // The correlated subquery must SEEK on the full fact key. Naming an index
    // is not enough — SQLite will scan an index end to end — so the assertion
    // is on the constraint list the planner resolved. `fact_key=?` is the last
    // of the four key columns: present means the whole key was usable.
    assert!(
        plan_text.contains("fact_key=?"),
        "the per-fact MAX cannot seek on the full key, so it scans: {plan_text}"
    );

    // The outer query must not scan the table either. The alias is `e`, so the
    // string is `SCAN e` — a sibling test once checked for `SCAN era`, which
    // the plan never contains, and passed unconditionally.
    assert!(
        !plan_text.contains("SCAN e"),
        "the point-in-time read is scanning the era table: {plan_text}"
    );
}
