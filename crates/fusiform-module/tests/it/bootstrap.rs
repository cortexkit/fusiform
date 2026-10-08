//! The carriage bar: a fresh install with no network comes up able to answer.
//!
//! "Able to answer" rather than "reports healthy". A module that starts, says
//! Ok, and serves an empty catalog has met the letter of the bar and failed
//! it — a consumer receiving zero models cannot distinguish that from "the
//! upstream describes nothing", and will cache it.
//!
//! Nothing here touches the network. That is the point.

use std::sync::{Arc, OnceLock};

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

struct SeededStore {
    store: CatalogStore,
    outcome: SeedOutcome,
    dir: tempfile::TempDir,
}

/// Only read-only tests may borrow this store. Keeping its directory and live
/// handle together retains the database and its writer lease for every reader.
/// OnceLock requires the handle to be Send + Sync and seeds an empty store once,
/// even when several test threads request it at the same time.
fn shared_seed() -> &'static SeededStore {
    static SEEDED: OnceLock<SeededStore> = OnceLock::new();
    SEEDED.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let store = fresh_store(&dir);
        let outcome = seed::seed_if_empty(&store).expect("seeding must succeed");
        SeededStore {
            store,
            outcome,
            dir,
        }
    })
}

/// A test that writes must own a separate database and lease. VACUUM INTO reads
/// a consistent SQLite snapshot, including committed WAL frames that a raw file
/// copy would lose. The template is never written after seeding.
fn copied_seeded_store(dir: &tempfile::TempDir) -> CatalogStore {
    let template = shared_seed();
    let source = rusqlite::Connection::open_with_flags(
        template.dir.path().join("store.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    source
        .execute(
            "VACUUM INTO ?1",
            [dir.path().join("store.db").to_string_lossy().as_ref()],
        )
        .unwrap();
    // Use the same migrated, lease-acquiring open path as an empty installation;
    // the source's separate lease file is not part of the SQLite snapshot.
    let store = fresh_store(dir);
    assert_eq!(
        store.era_count(SourceId::ModelsDev).unwrap(),
        template.store.era_count(SourceId::ModelsDev).unwrap(),
        "the copy must retain every seeded era"
    );
    assert_eq!(
        store.catalog_version().unwrap(),
        template.store.catalog_version().unwrap(),
        "the copy must retain the snapshot's version"
    );
    store
}

/// A fresh install serves a real catalog with no network.
#[test]
fn a_fresh_install_with_no_network_answers_a_read() {
    let seeded = shared_seed();
    let store = &seeded.store;

    // This is the actual outcome of seeding the empty shared store, not an
    // outcome reconstructed from a populated copy.
    match seeded.outcome.clone() {
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
    let response = match serve_tool_call(store, br#"{"name": "catalog.get", "arguments": {}}"#)
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
    let store = &shared_seed().store;

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
    let store = &shared_seed().store;

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
    // This test only compares the poll's digests and reads the era count; it
    // does not record a poll or append eras, so it can borrow the template.
    let store = &shared_seed().store;

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
    let store = copied_seeded_store(&dir);

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

/// A seeded store serves a version that states the snapshot's vintage.
///
/// Found by driving the live module after deployment: `ck models status`
/// reported 6,280 models, 68,026 eras, and `catalog version 0`. The version is
/// what a consumer holds as a high-water mark and refuses at or below, so a
/// full catalog was indistinguishable from a store that knows nothing — and it
/// stayed that way until the first CHANGED poll, since an unchanged poll
/// advances nothing.
///
/// The serious case is a reinstall. A fresh install seeded from a NEWER
/// snapshot also served 0, so every consumer holding a real version refused it
/// — correctly by their own rule and wrongly in fact, because the refused
/// catalog was the newer one.
///
/// The version is the snapshot's FETCH INSTANT rather than now, so it states
/// the vintage of the content rather than the age of the install: two machines
/// installed a week apart from one snapshot agree, and an install from a stale
/// snapshot orders correctly below a consumer's newer catalog instead of
/// claiming to supersede it.
#[test]
fn a_seeded_store_serves_the_snapshots_vintage_as_its_version() {
    let dir = tempfile::tempdir().unwrap();
    let store = fresh_store(&dir);

    // Before: nothing, and version 0 is the honest answer for a store that
    // knows nothing.
    assert_eq!(store.catalog_version().unwrap(), 0);

    let outcome = seed::seed_if_empty(&store).unwrap();
    let SeedOutcome::Seeded {
        fetched_at,
        version,
        model_count,
        ..
    } = outcome
    else {
        panic!("an empty store must seed, got {outcome:?}");
    };

    assert!(model_count > 6_000, "the snapshot describes a real catalog");
    assert_eq!(
        version, fetched_at.0,
        "the version must be the snapshot's fetch instant, not the install time"
    );
    assert_eq!(
        store.catalog_version().unwrap(),
        fetched_at.0,
        "and the store must serve it"
    );

    // The distinguishing property, stated directly: a seeded store and an
    // empty one must not report the same version.
    let empty_dir = tempfile::tempdir().unwrap();
    let empty = fresh_store(&empty_dir);
    assert_ne!(
        store.catalog_version().unwrap(),
        empty.catalog_version().unwrap(),
        "a consumer must be able to tell a seeded catalog from an empty store"
    );
}

/// Two installs from the same snapshot agree on their version.
///
/// This is why the version is the fetch instant rather than `now`. Under a
/// now-based version, two machines installed a week apart from one snapshot
/// would claim different versions for identical content, and the later install
/// would appear to supersede the earlier one while holding exactly the same
/// catalog.
#[test]
fn two_installs_from_one_snapshot_report_the_same_version() {
    let a_dir = tempfile::tempdir().unwrap();
    let a = fresh_store(&a_dir);
    seed::seed_if_empty(&a).unwrap();

    let b_dir = tempfile::tempdir().unwrap();
    let b = fresh_store(&b_dir);
    seed::seed_if_empty(&b).unwrap();

    assert_eq!(
        a.catalog_version().unwrap(),
        b.catalog_version().unwrap(),
        "identical content must carry an identical version"
    );
}
