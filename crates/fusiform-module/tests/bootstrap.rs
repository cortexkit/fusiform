//! The carriage bar: a fresh install with no network comes up able to answer.
//!
//! "Able to answer" rather than "reports healthy". A module that starts, says
//! Ok, and serves an empty catalog has met the letter of the bar and failed
//! it — a consumer receiving zero models cannot distinguish that from "the
//! upstream describes nothing", and will cache it.
//!
//! Nothing here touches the network. That is the point.

use std::sync::Arc;

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp, TokenClass};
use fusiform_module::route::{serve_tool_call, ToolResponse};
use fusiform_module::seed::{self, SeedOutcome};
use fusiform_module::signals::Signals;
use fusiform_module::{health, seed as seed_mod};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, FactKey, NewObservation};
use subc_protocol::session::HealthStatus;

fn fresh_store(dir: &tempfile::TempDir) -> CatalogStore {
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

/// A fresh install serves a real catalog with no network.
#[test]
fn a_fresh_install_with_no_network_answers_a_read() {
    let dir = tempfile::tempdir().unwrap();
    let store = fresh_store(&dir);

    match seed::seed_if_empty(&store).expect("seeding must succeed") {
        SeedOutcome::Seeded {
            eras_written,
            model_count,
            ..
        } => {
            assert!(
                model_count > 1_000,
                "the snapshot must describe a real catalog, got {model_count} models"
            );
            assert!(eras_written > 10_000, "got {eras_written} eras");
        }
        other => panic!("a fresh store must seed, got {other:?}"),
    }

    // The read a consumer actually makes.
    let response = match serve_tool_call(&store, br#"{"name": "catalog.get", "arguments": {}}"#)
        .expect("the catalog must be readable")
    {
        ToolResponse::Catalog(c) => c,
        other => panic!("expected a catalog, got {other:?}"),
    };

    assert!(
        response.model_count() > 1_000,
        "a seeded install must serve models, got {}",
        response.model_count()
    );

    // And the facts are real, not placeholders. A seed that populated identity
    // rows and nothing else would pass every count assertion above.
    let sonnet = response
        .models
        .iter()
        .find(|(k, _)| k.contains("claude-sonnet"))
        .map(|(_, v)| v)
        .expect("a well-known model must be present");
    assert!(
        sonnet.contains_key("rate.input"),
        "a seeded model must carry its rates"
    );
    assert!(sonnet.contains_key("limit.context"));
}

/// A fresh install reports Ok, and its health says why.
#[test]
fn a_fresh_install_reports_ok_with_an_honest_reason() {
    let dir = tempfile::tempdir().unwrap();
    let store = fresh_store(&dir);
    seed::seed_if_empty(&store).unwrap();

    let signals = Arc::new(Signals::new());
    signals.store_opened();
    // The startup path adopts the seeded observation.
    if let Ok(Some(at)) = store.last_confirming_observation(SourceId::ModelsDev) {
        signals.adopt_last_observation(at.0);
    }

    // Immediately after install, with the snapshot's own fetch instant as now:
    // the catalog is as fresh as it will ever be from a seed.
    let meta = seed_mod::meta().unwrap();
    let report = health::report(&signals, meta.fetched_at_ms + 60_000);
    assert_eq!(
        report.status,
        HealthStatus::Ok,
        "a just-installed module is not unhealthy: {:?}",
        report.detail
    );

    // But an OLD snapshot on a machine that has never reached the network is
    // degraded, and this is the case the seed makes possible to detect at all.
    // Without a recorded seeded observation the age would be unknown, and
    // "unknown" reads as Ok.
    let a_week_later = meta.fetched_at_ms + 7 * 24 * 60 * 60 * 1_000;
    let stale = health::report(&signals, a_week_later);
    assert_eq!(
        stale.status,
        HealthStatus::Degraded,
        "a week-old snapshot with no successful poll is stale: {:?}",
        stale.detail
    );
}

/// Seeding is idempotent: a restart does not re-apply the snapshot.
#[test]
fn seeding_twice_does_not_rewrite_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = fresh_store(&dir);

    let first = seed::seed_if_empty(&store).unwrap();
    let SeedOutcome::Seeded { eras_written, .. } = first else {
        panic!("the first seed must apply");
    };

    let second = seed::seed_if_empty(&store).unwrap();
    match second {
        SeedOutcome::AlreadyPopulated { existing_eras } => {
            assert_eq!(existing_eras as usize, eras_written);
        }
        other => panic!("a populated store must not re-seed, got {other:?}"),
    }

    // Exactly one seeded observation, not two.
    let polls = store.recent_observations(SourceId::ModelsDev, 10).unwrap();
    assert_eq!(polls.len(), 1, "seeding twice must record one observation");
    assert_eq!(polls[0].outcome, "seeded");
}

/// The seeded observation carries the SNAPSHOT's fetch instant, not now.
///
/// The distinction the whole seed design rests on. Recording the install
/// instant would claim fusiform looked at the upstream when the operator ran
/// the installer, and would make a year-old snapshot report as fresh.
#[test]
fn the_seeded_observation_is_stamped_when_the_snapshot_was_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let store = fresh_store(&dir);
    seed::seed_if_empty(&store).unwrap();

    let meta = seed_mod::meta().unwrap();
    let recorded = store
        .last_confirming_observation(SourceId::ModelsDev)
        .unwrap()
        .expect("the seed records an observation");

    assert_eq!(
        recorded.0, meta.fetched_at_ms,
        "the observation must be stamped at the snapshot's fetch instant"
    );

    // And that instant is in the past relative to running this test, so a
    // module installed today does not claim a fresh observation.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(
        recorded.0 < now,
        "a compiled-in snapshot cannot have been fetched in the future"
    );
}

/// A first fetch that AGREES with the seed writes nothing.
#[test]
fn a_first_fetch_agreeing_with_the_snapshot_opens_no_era() {
    let dir = tempfile::tempdir().unwrap();
    let store = fresh_store(&dir);
    seed::seed_if_empty(&store).unwrap();

    let before = store.era_count(SourceId::ModelsDev).unwrap();
    let catalog = seed_mod::catalog().unwrap();

    // The poll loop's own comparison: the seeded observation's normalized hash
    // against this document's digest.
    let held = store
        .last_normalized_hash(SourceId::ModelsDev)
        .unwrap()
        .expect("the seed recorded a hash");
    let fetched = fusiform_store::ingest::catalog_digest(&catalog);
    assert_eq!(
        held, fetched,
        "an unchanged document must hash equal to the seed, or every first \
         poll rewrites the whole catalog"
    );

    assert_eq!(
        store.era_count(SourceId::ModelsDev).unwrap(),
        before,
        "no era may be written"
    );
}

/// A first fetch that DISAGREES gets an observed boundary with a real window.
///
/// This is the defect the seed was built to fix. Before the seeded observation
/// existed, the first disagreeing fetch had no left edge and was written as a
/// second `Seed` boundary — claiming the store came into existence twice,
/// dropping the link to the observation that detected the change, and reporting
/// no window where a real one exists.
#[test]
fn a_disagreeing_first_fetch_gets_an_observed_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let store = fresh_store(&dir);
    seed::seed_if_empty(&store).unwrap();

    let meta = seed_mod::meta().unwrap();
    let poll_at = meta.fetched_at_ms + 30 * 60 * 1_000;

    // The upstream moved between the snapshot being cut and this install
    // polling: the ordinary case, since the seed is compiled in at release.
    let mut doc: serde_json::Value = serde_json::from_slice(
        &std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/data/models-dev-seed.json"
        ))
        .unwrap(),
    )
    .unwrap();
    let target = doc
        .get_mut("anthropic")
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.get_mut("claude-sonnet-4-5"))
        .and_then(|m| m.get_mut("cost"))
        .and_then(|c| c.as_object_mut())
        .expect("the snapshot must carry this model");
    target.insert("input".to_string(), serde_json::json!(2.5));

    let moved = fusiform_core::normalize::normalize_models_dev(&serde_json::to_vec(&doc).unwrap())
        .unwrap()
        .catalog;

    let observation_id = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(poll_at),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some(fusiform_store::ingest::catalog_digest(&moved)),
            raw_hash: Some("raw".to_string()),
            etag: None,
            duration_ms: Some(200),
            detail: None,
        })
        .unwrap();

    // The loop's rule: a prior confirming observation exists, so this is an
    // observed boundary. The seeded observation is what supplies it.
    let prior = store
        .last_confirming_observation_before(SourceId::ModelsDev, Timestamp(poll_at))
        .unwrap();
    assert_eq!(
        prior,
        Some(Timestamp(meta.fetched_at_ms)),
        "the seeded observation must be available as a window edge"
    );

    let plan = plan_ingest(
        &store,
        &moved,
        Timestamp(poll_at),
        BoundaryKind::Observed,
        Some(observation_id),
    )
    .unwrap();
    store.append_eras(&plan.eras).unwrap();

    let history = store
        .fact_history(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &FactKey::rate(TokenClass::Input),
        )
        .unwrap();

    assert_eq!(history.len(), 2, "the seed era and the observed change");
    assert_eq!(history[0].boundary_kind, "seed");
    assert_eq!(
        history[0].prior_observation_at, None,
        "the seed itself has no window"
    );

    assert_eq!(
        history[1].boundary_kind, "observed",
        "a real fetch that detected a real change is an observation, not a second seed"
    );
    assert_eq!(
        history[1].window(),
        Some((Timestamp(meta.fetched_at_ms), Timestamp(poll_at))),
        "the change happened between the snapshot's fetch and this poll"
    );
}

/// The compiled-in snapshot and its metadata describe the same bytes.
///
/// The two files are written together by the refresh script, so they can only
/// disagree if one was edited by hand — which is exactly when a stale
/// `fetched_at_ms` would silently become the left edge of every first-fetch
/// window.
#[test]
fn the_snapshot_and_its_metadata_agree() {
    let meta = seed_mod::meta().unwrap();
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/data/models-dev-seed.json"
    ))
    .unwrap();

    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut hasher, &bytes);
    let actual = format!("{:x}", sha2::Digest::finalize(hasher));
    assert_eq!(
        actual, meta.sha256,
        "the snapshot's hash does not match its metadata; re-run scripts/refresh-seed.sh"
    );

    let catalog = seed_mod::catalog().unwrap();
    assert_eq!(
        catalog.model_count(),
        meta.model_count,
        "the metadata's model count does not match the snapshot"
    );
}
