//! One poll cycle's decisions, against a real store and no network.
//!
//! The fetch half classifies an HTTP response and returns; every decision in a
//! tick lives in `apply`, which takes the classified outcome. Testing it
//! directly is what makes the parse-failure and unchanged-body branches
//! reachable at all — a test that has to stand up an HTTP server to reach them
//! does not get written, and those branches are where the interesting mistakes
//! are.

use std::sync::Arc;
use std::time::Duration;

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{FailureClass, SourceId, Timestamp};
use fusiform_store::{CatalogStore, QualityGrade};

use fusiform_module::fetch::FetchOutcome;
use fusiform_module::health;
use fusiform_module::loop_::{apply, TickOutcome};
use fusiform_module::signals::Signals;
use fusiform_testkit::mutate;

const FIXTURE: &[u8] = include_bytes!("../../../fusiform-core/fixtures/models-dev-excerpt.json");

struct Fixture {
    store: Arc<CatalogStore>,
    signals: Arc<Signals>,
    _dir: tempfile::TempDir,
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
    Fixture {
        store: Arc::new(store),
        signals: Arc::new(Signals::new()),
        _dir: dir,
    }
}

fn body(bytes: &[u8], etag: Option<&str>) -> FetchOutcome {
    FetchOutcome::Body {
        bytes: bytes.to_vec(),
        etag: etag.map(str::to_string),
        duration: Duration::from_millis(10),
    }
}

fn body_with_extra_model(priced: bool) -> Vec<u8> {
    let mut document: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    let mut model = document["anthropic"]["models"]["claude-sonnet-4-5"].clone();
    model["id"] = serde_json::json!("health-count-model");
    if !priced {
        model.as_object_mut().unwrap().remove("cost");
    }
    document["anthropic"]["models"]["health-count-model"] = model;
    serde_json::to_vec(&document).unwrap()
}

fn run(f: &Fixture, outcome: FetchOutcome, at: i64) -> fusiform_module::loop_::TickReport {
    apply(&f.store, &f.signals, SourceId::ModelsDev, outcome, at).unwrap()
}

#[tokio::test]
async fn status_counts_normalization_findings_for_the_latest_parsed_body() {
    use serde_json::{json, Value};
    use subc_client_rs::HandlerOutcome;

    async fn status(f: &Fixture, signals: Arc<Signals>) -> Value {
        match fusiform_module::serve_catalog_request(
            f.store.clone(),
            signals,
            br#"{"name":"catalog.status","arguments":{"polls":0}}"#.to_vec(),
            false,
        )
        .await
        {
            HandlerOutcome::Response(bytes) => serde_json::from_slice(&bytes).unwrap(),
            other => panic!("expected status, got {other:?}"),
        }
    }

    let f = fixture();
    assert!(status(&f, f.signals.clone())
        .await
        .get("normalization_findings")
        .is_none());
    let bad = br#"{"p":{"models":{
        "bad":{"cost":{"input":"1.25","output":"2.5"}},
        "good":{"cost":{"input":1,"output":2,"input_audio":3}}
    }}}"#;
    let first = run(&f, body(bad, None), 1_000);
    assert!(matches!(first.outcome, TickOutcome::Changed { .. }));
    let expected = json!({"observation_id":first.observation_id,
                         "counts":{"refused_base_schedule":2,"unattributable_charge_unit":1}});
    let first_status = status(&f, f.signals.clone()).await;
    assert!(first_status.get("failures").is_none());
    assert_eq!(
        first_status["normalization_findings"], expected,
        "a successful poll must disclose its normalization findings"
    );

    run(
        &f,
        FetchOutcome::NotModified {
            etag: None,
            duration: Duration::ZERO,
        },
        2_000,
    );
    assert_eq!(
        status(&f, f.signals.clone()).await["normalization_findings"],
        expected
    );
    assert!(status(&f, Arc::new(Signals::new()))
        .await
        .get("normalization_findings")
        .is_none());

    let good = br#"{"p":{"models":{"bad":{"cost":{"input":1,"output":2}},"good":{"cost":{"input":1,"output":2}}}}}"#;
    let last = run(&f, body(good, None), 3_000);
    assert_eq!(
        status(&f, f.signals.clone()).await["normalization_findings"],
        json!({"observation_id":last.observation_id,"counts":{}})
    );
}

#[test]
fn changed_poll_rolls_back_observation_and_retries() {
    let f = fixture();
    let source = SourceId::ModelsDev;
    let first = run(&f, body(FIXTURE, Some("first")), 1_000);
    let version = f.store.catalog_version().unwrap();
    let second = body_with_extra_model(true);
    let db = rusqlite::Connection::open(f.store.path()).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_era BEFORE INSERT ON era BEGIN SELECT RAISE(ABORT,'era write failed'); END",
    )
    .unwrap();
    let error = apply(
        &f.store,
        &f.signals,
        source,
        body(&second, Some("second")),
        2_000,
    )
    .unwrap_err();
    assert!(error.to_string().contains("era write failed"), "{error}");
    db.execute_batch("DROP TRIGGER fail_era").unwrap();
    assert_eq!(
        f.store.latest_observation_id().unwrap(),
        first.observation_id
    );
    assert_eq!(f.store.last_etag(source).unwrap().as_deref(), Some("first"));
    assert_eq!(f.store.catalog_version().unwrap(), version);

    let retry = run(&f, body(&second, Some("second")), 3_000);
    assert!(matches!(retry.outcome, TickOutcome::Changed { .. }));
    assert!(retry.eras_written > 0);
    let era = f
        .store
        .recorded_value_at(
            source,
            "anthropic",
            "health-count-model",
            &fusiform_store::FactKey::existence(),
            Timestamp(3_000),
        )
        .unwrap()
        .unwrap();
    assert_eq!(era.prior_observation_at, Some(Timestamp(1_000)));
    assert_eq!(era.observation_id, Some(retry.observation_id));
}

#[test]
fn a_clock_rewind_must_not_acknowledge_an_unapplied_document() {
    let f = fixture();
    let source = SourceId::ModelsDev;
    run(&f, body(FIXTURE, Some("first")), 1_000);
    let second = body_with_extra_model(true);
    assert!(matches!(
        run(&f, body(&second, Some("second")), 3_000).outcome,
        TickOutcome::Changed { .. }
    ));
    let version = f.store.catalog_version().unwrap();
    let key = fusiform_store::FactKey::existence();

    for at in [2_000, 3_000] {
        let report = run(&f, body(FIXTURE, Some("refused")), at);
        assert!(
            matches!(
                report.outcome,
                TickOutcome::Failed {
                    class: FailureClass::Implausible
                }
            ),
            "unexpected outcome at {at}: {:?}",
            report.outcome
        );
        assert_eq!(report.eras_written, 0);
        assert_eq!(f.store.catalog_version().unwrap(), version);
        assert_eq!(
            f.store.last_etag(source).unwrap().as_deref(),
            Some("second")
        );
        let observation = f
            .store
            .recent_observations(source, 5)
            .unwrap()
            .into_iter()
            .find(|o| o.id == report.observation_id)
            .unwrap();
        assert_eq!(observation.outcome, "failed");
        let detail = observation.detail.unwrap();
        assert!(
            detail.contains("clock") && detail.contains(&at.to_string()) && detail.contains("3000"),
            "{detail}"
        );
    }
    assert_eq!(
        health::report(&f.signals, 3_000).metrics.unwrap()["last_failure_class"],
        "implausible"
    );
    let current = f
        .store
        .recorded_value_at(
            source,
            "anthropic",
            "health-count-model",
            &key,
            Timestamp(3_000),
        )
        .unwrap()
        .unwrap();
    assert_eq!(current.value_json, r#""present""#);

    let retry = run(&f, body(FIXTURE, Some("retry")), 4_000);
    assert!(matches!(retry.outcome, TickOutcome::Changed { .. }));
}

#[test]
fn a_different_saved_digest_with_no_era_changes_does_not_advance_version() {
    use fusiform_core::ObservationOutcome;
    use fusiform_store::NewObservation;

    let f = fixture();
    let source = SourceId::ModelsDev;
    run(&f, body(FIXTURE, Some("first")), 1_000);
    let version = f.store.catalog_version().unwrap();
    let digest = f.store.last_normalized_hash(source).unwrap().unwrap();
    f.store
        .record_observation(&NewObservation {
            source,
            observed_at: Timestamp(1_500),
            outcome: ObservationOutcome::Unchanged,
            normalized_hash: Some("previous-projection-digest".into()),
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    assert_ne!(
        f.store.last_normalized_hash(source).unwrap().as_deref(),
        Some(digest.as_str())
    );

    let report = run(&f, body(FIXTURE, Some("refreshed")), 2_000);
    assert!(
        matches!(report.outcome, TickOutcome::Unchanged),
        "{:?}",
        report.outcome
    );
    assert_eq!(report.eras_written, 0);
    assert_eq!(f.store.catalog_version().unwrap(), version);
    assert_eq!(f.store.last_normalized_hash(source).unwrap(), Some(digest));
    assert_eq!(
        f.store.last_etag(source).unwrap().as_deref(),
        Some("refreshed")
    );
}

/// The first tick against an empty store writes a SEED, not an observed
/// boundary.
///
/// There is no prior observation to bound the first era against, so an observed
/// boundary would be claiming a window that does not exist.
#[test]
fn the_first_tick_seeds() {
    let f = fixture();
    let report = run(&f, body(FIXTURE, Some("\"v1\"")), 1_000);

    let TickOutcome::Changed { new_version } = report.outcome else {
        panic!("the first tick must be a change, got {:?}", report.outcome);
    };
    // The version is derived as max(now_ms, current + 1), so a first tick at
    // instant 1000 issues 1000 rather than 1. Asserted as a property because
    // the derivation is the store's, not this test's: what a consumer needs is
    // that it advanced from nothing, not that it equals any particular number.
    assert!(new_version > 0);
    assert_eq!(f.store.catalog_version().unwrap(), new_version);
    assert!(report.eras_written > 13);

    // Every era from a first tick is a seed, and carries no window.
    let row = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &fusiform_store::FactKey::rate(fusiform_core::TokenClass::Input),
            Timestamp(2_000),
        )
        .unwrap()
        .known()
        .cloned()
        .unwrap();
    assert_eq!(row.boundary_kind, fusiform_core::BoundaryKind::Seed);
    assert_eq!(row.observation_window(), None);
}

/// A poll normalizes with the shipped input-audio evidence: the fixture's
/// google row stores `rate.input_audio` through the real tick path. Without
/// the evidence at this call site, the rate never reaches the store.
#[test]
fn a_poll_stores_input_audio_for_an_evidenced_provider() {
    let f = fixture();
    run(&f, body(FIXTURE, None), 1_000);
    let audio = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "google",
            "gemini-flash-latest",
            &fusiform_store::FactKey::rate(fusiform_core::TokenClass::InputAudio),
            Timestamp(2_000),
        )
        .unwrap()
        .known()
        .cloned()
        .expect("the polled google row must store rate.input_audio");
    let value: serde_json::Value = serde_json::from_str(&audio.value_json).unwrap();
    assert_eq!(value["units"], 1_500_000_000, "{value}");
}

#[test]
fn a_changed_poll_updates_model_count() {
    let f = fixture();
    run(&f, body(FIXTURE, None), 1_000);
    let before = health::report(&f.signals, 1_000).metrics.unwrap()["model_count"]
        .as_u64()
        .unwrap();

    let report = run(&f, body(&body_with_extra_model(false), None), 2_000);

    assert!(matches!(report.outcome, TickOutcome::Changed { .. }));
    assert_eq!(
        health::report(&f.signals, 2_000).metrics.unwrap()["model_count"],
        before + 1
    );
}

#[test]
fn a_changed_poll_updates_priced_model_count() {
    let f = fixture();
    run(&f, body(FIXTURE, None), 1_000);
    let before = health::report(&f.signals, 1_000).metrics.unwrap()["models_priced"]
        .as_u64()
        .unwrap();

    let report = run(&f, body(&body_with_extra_model(true), None), 2_000);

    assert!(matches!(report.outcome, TickOutcome::Changed { .. }));
    assert_eq!(
        health::report(&f.signals, 2_000).metrics.unwrap()["models_priced"],
        before + 1
    );
}

#[test]
fn cached_model_counts_match_catalog_status_after_a_changed_poll() {
    let f = fixture();
    run(&f, body(FIXTURE, None), 1_000);
    run(&f, body(&body_with_extra_model(true), None), 2_000);

    let metrics = health::report(&f.signals, 2_000).metrics.unwrap();
    let status = fusiform_module::route::serve_status(&f.store, b"").unwrap();
    assert_eq!(metrics["model_count"], status.model_count);
    assert_eq!(metrics["models_priced"], status.models_priced.unwrap());
}

#[test]
fn unchanged_and_not_modified_polls_keep_cached_model_counts() {
    let f = fixture();
    run(&f, body(FIXTURE, None), 1_000);
    let counted = health::report(&f.signals, 1_000).metrics.unwrap();

    let not_modified = run(
        &f,
        FetchOutcome::NotModified {
            etag: Some("v1".into()),
            duration: Duration::from_millis(10),
        },
        2_000,
    );
    assert_eq!(not_modified.outcome, TickOutcome::NotModified);
    let after_not_modified = health::report(&f.signals, 2_000).metrics.unwrap();
    assert_eq!(after_not_modified["model_count"], counted["model_count"]);
    assert_eq!(
        after_not_modified["models_priced"],
        counted["models_priced"]
    );

    let unchanged = run(&f, body(FIXTURE, Some("v1")), 3_000);
    assert_eq!(unchanged.outcome, TickOutcome::Unchanged);
    let after_unchanged = health::report(&f.signals, 3_000).metrics.unwrap();
    assert_eq!(after_unchanged["model_count"], counted["model_count"]);
    assert_eq!(after_unchanged["models_priced"], counted["models_priced"]);
}

/// A second identical body writes an observation and no eras.
#[test]
fn an_unchanged_body_records_the_look_and_nothing_else() {
    let f = fixture();
    run(&f, body(FIXTURE, Some("\"v1\"")), 1_000);
    let after_first = f.store.catalog_version().unwrap();

    let report = run(&f, body(FIXTURE, Some("\"v1\"")), 2_000);
    assert_eq!(report.outcome, TickOutcome::Unchanged);
    assert_eq!(report.eras_written, 0);
    // The version does not move for a non-change: a consumer that refuses a
    // version it already holds must not be handed a new number for old content.
    assert_eq!(f.store.catalog_version().unwrap(), after_first);
}

/// Reformatted bytes are not a change.
///
/// The digest is computed over the facts, so a document that round-trips
/// through a JSON library — different bytes, different key order — produces an
/// Unchanged tick.
#[test]
fn reformatted_bytes_do_not_produce_a_change() {
    let f = fixture();
    run(&f, body(FIXTURE, None), 1_000);

    let doc: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    let reserialized = serde_json::to_vec(&doc).unwrap();
    assert_ne!(
        reserialized, FIXTURE,
        "the round trip must change the bytes"
    );

    let report = run(&f, body(&reserialized, None), 2_000);
    assert_eq!(
        report.outcome,
        TickOutcome::Unchanged,
        "a reformat must not wake consumers"
    );
    assert_eq!(report.eras_written, 0);
}

/// A 304 is a real observation and narrows the next window.
#[test]
fn a_not_modified_response_is_an_observation() {
    let f = fixture();
    run(&f, body(FIXTURE, Some("\"v1\"")), 1_000);

    let report = run(
        &f,
        FetchOutcome::NotModified {
            etag: Some("\"v1\"".to_string()),
            duration: Duration::from_millis(5),
        },
        2_000,
    );
    assert_eq!(report.outcome, TickOutcome::NotModified);
    assert_eq!(report.eras_written, 0);

    // A subsequent change opens its window against the 304, not against the
    // earlier full fetch.
    let mutated = String::from_utf8(FIXTURE.to_vec())
        .unwrap()
        .replace("\"input\": 3,", "\"input\": 4,");
    let report = run(&f, body(mutated.as_bytes(), Some("\"v2\"")), 3_000);
    assert!(matches!(report.outcome, TickOutcome::Changed { .. }));

    let row = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &fusiform_store::FactKey::rate(fusiform_core::TokenClass::Input),
            Timestamp(4_000),
        )
        .unwrap()
        .known()
        .cloned()
        .unwrap();
    assert_eq!(
        row.observation_window(),
        Some((Timestamp(2_000), Timestamp(3_000))),
        "the 304 is the near edge; it observed that the content was unchanged"
    );
}

/// A failed poll writes an observation, no eras, and does not narrow anything.
#[test]
fn a_failed_poll_records_the_attempt_and_nothing_more() {
    let f = fixture();
    run(&f, body(FIXTURE, Some("\"v1\"")), 1_000);

    // Two failures land between the seed and the next real observation.
    for at in [2_000, 3_000] {
        let report = run(
            &f,
            FetchOutcome::Failed {
                class: FailureClass::Network,
                detail: "connection refused".to_string(),
                duration: Duration::from_millis(50),
            },
            at,
        );
        assert_eq!(
            report.outcome,
            TickOutcome::Failed {
                class: FailureClass::Network
            }
        );
        assert_eq!(report.eras_written, 0);
    }

    let mutated = String::from_utf8(FIXTURE.to_vec())
        .unwrap()
        .replace("\"input\": 3,", "\"input\": 4,");
    run(&f, body(mutated.as_bytes(), None), 4_000);

    let row = f
        .store
        .value_at(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            &fusiform_store::FactKey::rate(fusiform_core::TokenClass::Input),
            Timestamp(5_000),
        )
        .unwrap()
        .known()
        .cloned()
        .unwrap();
    // 1000, not 3000: the failures observed nothing.
    assert_eq!(
        row.observation_window(),
        Some((Timestamp(1_000), Timestamp(4_000))),
        "failed polls must not narrow the window"
    );
}

/// A body that arrives and cannot be parsed is a PARSE failure, not a network
/// one.
///
/// The distinction is what an operator acts on: a network failure will probably
/// fix itself, while a parse failure means the upstream published a shape this
/// version does not understand and will keep failing until someone looks.
#[test]
fn an_unparseable_body_is_a_parse_failure_carrying_its_hash() {
    let f = fixture();

    let report = run(&f, body(b"{ not json at all", None), 1_000);
    assert_eq!(
        report.outcome,
        TickOutcome::Failed {
            class: FailureClass::Parse
        }
    );
    assert_eq!(report.eras_written, 0);

    // The failing document's hash is recorded, so a later fix can be checked
    // against the exact bytes that broke.
    let hash: Option<String> = f
        .store
        .raw_hash_of_observation(report.observation_id)
        .unwrap();
    assert!(
        hash.is_some(),
        "a parse failure must record which document failed"
    );

    // And no ETag is stored for a failure: the next poll must not send a
    // conditional request claiming to hold a document it could not read.
    assert_eq!(f.store.last_etag(SourceId::ModelsDev).unwrap(), None);
}

/// A failed tick must not advance the catalog version.
#[test]
fn a_failure_does_not_advance_the_version() {
    let f = fixture();
    run(&f, body(FIXTURE, None), 1_000);
    let after_change = f.store.catalog_version().unwrap();

    run(
        &f,
        FetchOutcome::Failed {
            class: FailureClass::Network,
            detail: "timeout".to_string(),
            duration: Duration::from_millis(90_000),
        },
        2_000,
    );
    assert_eq!(
        f.store.catalog_version().unwrap(),
        after_change,
        "a version bump with no new content would have consumers hold a \
         high-water mark for a catalog they never received"
    );
}

/// The version advances once per change, monotonically.
#[test]
fn the_version_advances_once_per_change() {
    let f = fixture();
    run(&f, body(FIXTURE, None), 1_000);
    let mut previous = f.store.catalog_version().unwrap();

    let text = String::from_utf8(FIXTURE.to_vec()).unwrap();
    for (i, (from, to)) in [
        ("\"input\": 3,", "\"input\": 4,"),
        ("\"input\": 4,", "\"input\": 5,"),
    ]
    .into_iter()
    .enumerate()
    {
        let mutated = mutate(&text, from, to);
        let at = 2_000 + i as i64 * 1_000;
        let report = run(&f, body(mutated.as_bytes(), None), at);
        assert!(
            matches!(report.outcome, TickOutcome::Changed { .. }),
            "round {i} should be a change"
        );
        // Strictly greater each round. The literal values are the tick
        // instants now, so what is checkable — and what a consumer relies on —
        // is that the sequence advances exactly once per change.
        let issued = f.store.catalog_version().unwrap();
        assert!(
            issued > previous,
            "round {i}: version must advance, {previous} -> {issued}"
        );
        previous = issued;
    }
}

fn status_versions(store: &CatalogStore) -> (u64, u64) {
    let response = fusiform_module::route::serve_status(store, b"").unwrap();
    let wire = serde_json::to_value(response).unwrap();
    (
        wire["catalog_version"].as_u64().unwrap(),
        wire["rates_version"]
            .as_u64()
            .expect("catalog.status must serve an integer rates_version"),
    )
}

#[test]
fn rates_version_tracks_rate_polls_but_not_renames_or_grades() {
    let f = fixture();
    f.store
        .advance_catalog_version_at_start(Timestamp(1_000))
        .unwrap();
    run(&f, body(FIXTURE, None), 2_000);
    let before = status_versions(&f.store);

    let mut changed: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    changed["anthropic"]["models"]["claude-sonnet-4-5"]["cost"]["input"] = serde_json::json!(4);
    let report = run(
        &f,
        body(&serde_json::to_vec(&changed).unwrap(), None),
        3_000,
    );
    assert_eq!(report.eras_written, 1);
    let after_rate = status_versions(&f.store);
    assert_eq!(after_rate, (3_000, 3_000));
    assert!(after_rate.0 > before.0 && after_rate.1 > before.1);

    changed["anthropic"]["models"]["claude-sonnet-4-5"]["name"] =
        serde_json::json!("Renamed Sonnet");
    let report = run(
        &f,
        body(&serde_json::to_vec(&changed).unwrap(), None),
        4_000,
    );
    assert_eq!(report.eras_written, 1);
    assert_eq!(status_versions(&f.store), (4_000, after_rate.1));

    f.store
        .append_quality_grade(
            "anthropic/claude-sonnet-4-5",
            None,
            Some(&QualityGrade::Chat {
                elo: 1100.into(),
                eq: 1000.into(),
            }),
            "user",
            "Update quality judgment",
            5_000,
        )
        .unwrap();
    assert_eq!(status_versions(&f.store), (5_000, after_rate.1));
}

#[test]
fn rates_version_restarts_at_the_new_catalog_version() {
    let f = fixture();
    f.store
        .advance_catalog_version_at_start(Timestamp(1_000))
        .unwrap();
    let original = status_versions(&f.store);
    assert_eq!(original, (1_000, 1_000));
    let Fixture { store, _dir, .. } = f;
    drop(store);

    let reopened = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: _dir.path().join("store.db").to_string_lossy().into_owned(),
        },
    })
    .unwrap();
    reopened
        .advance_catalog_version_at_start(Timestamp(900))
        .unwrap();
    let restarted = status_versions(&reopened);
    assert_eq!(restarted, (1_001, 1_001));
    assert!(restarted.1 > original.1);
}

/// Signals track what actually happened, so health cannot be fooled.
#[test]
fn signals_reflect_observations_rather_than_attempts() {
    let f = fixture();
    run(&f, body(FIXTURE, None), 1_000);
    assert_eq!(f.signals.polls_recorded(), 1);
    assert_eq!(f.signals.observation_age_ms(2_000), Some(1_000));

    for at in [2_000, 3_000, 4_000] {
        run(
            &f,
            FetchOutcome::Failed {
                class: FailureClass::Network,
                detail: "down".to_string(),
                duration: Duration::from_millis(10),
            },
            at,
        );
    }

    // The loop is demonstrably running.
    assert_eq!(f.signals.polls_recorded(), 4);
    assert_eq!(f.signals.consecutive_failures(), 3);
    // But knowledge is as old as the last real observation, which is what stops
    // a failing fetcher from reporting healthy.
    assert_eq!(f.signals.observation_age_ms(5_000), Some(4_000));
}

/// The failure class a tick REPORTS is the class it STORED.
///
/// Found by mutation: the two failure paths each constructed the class twice —
/// once for the observation row, once for the returned report — and changing
/// one left the other, with every test still passing. Both paths now go through
/// one function, and this test would catch a regression that split them again.
#[test]
fn the_reported_failure_class_matches_the_stored_one() {
    let f = fixture();

    let cases = [
        (
            FetchOutcome::Failed {
                class: FailureClass::Network,
                detail: "connection refused".to_string(),
                duration: Duration::from_millis(10),
            },
            FailureClass::Network,
            "network",
        ),
        (
            FetchOutcome::Failed {
                class: FailureClass::HttpStatus,
                detail: "upstream answered 503".to_string(),
                duration: Duration::from_millis(10),
            },
            FailureClass::HttpStatus,
            "http_status",
        ),
        (body(b"{ not json", None), FailureClass::Parse, "parse"),
    ];

    for (i, (outcome, expected, stored_name)) in cases.into_iter().enumerate() {
        let at = 1_000 + i as i64 * 1_000;
        let report = run(&f, outcome, at);

        assert_eq!(
            report.outcome,
            TickOutcome::Failed { class: expected },
            "the reported class is wrong for case {i}"
        );

        let stored = f
            .store
            .failure_class_of_observation(report.observation_id)
            .unwrap();
        assert_eq!(
            stored.as_deref(),
            Some(stored_name),
            "the class stored in the observation row disagrees with the one \
             reported to the caller, for case {i}"
        );
    }
}

/// A tick reports how its eras divide, not just how many there are.
///
/// Measured over 11 hours of live polling: 72% of era churn was models arriving
/// and leaving rather than facts changing, because an arriving model writes one
/// era per fact it has. Capability "changes" were the extreme case — 230 of 245
/// were arrivals and only 15 were genuine changes on a known model.
///
/// So a row count alone is misleading in a specific direction: an operator
/// reading "45 eras written" takes it as 45 facts moving. The distinction is
/// not cosmetic — a model arriving is the upstream publishing something new, a
/// fact changing on a known model is the upstream revising something, and only
/// the second is the event a consumer's cache or a ledger cares about.
///
/// The counts already existed on the ingest plan and were discarded at the tick
/// boundary. This asserts they survive to the report, which is what an operator
/// reads.
#[test]
fn a_tick_reports_arrivals_and_changes_separately() {
    let f = fixture();

    let doc = |models: &[(&str, &str, &str)]| {
        let entries: Vec<String> = models
            .iter()
            .map(|(prov, id, rate)| {
                format!(
                    r#""{prov}":{{"id":"{prov}","name":"{prov}","models":{{"{id}":{{"id":"{id}","name":"{id}","cost":{{"input":{rate},"output":{rate}}}}}}}}}"#
                )
            })
            .collect();
        format!("{{{}}}", entries.join(","))
    };

    // Seed: two models.
    let first = doc(&[("anthropic", "sonnet", "3.0"), ("openai", "gpt5", "2.0")]);
    let seeded = run(&f, body(first.as_bytes(), None), 1_000);
    assert!(seeded.eras_written > 0, "the seed must write something");

    // Next poll: one rate moves on a known model, TWO models arrive, one goes.
    //
    // Asymmetric counts on purpose. An earlier version used one arrival and one
    // withdrawal, and a mutation swapping the two fields survived — the test
    // could not tell them apart. Two and one can.
    let second = doc(&[
        ("anthropic", "sonnet", "4.0"),
        ("mistral", "large", "1.0"),
        ("google", "gemini", "5.0"),
    ]);
    let report = run(&f, body(second.as_bytes(), None), 2_000);

    let c = report.composition;
    assert_eq!(c.models_arrived, 2, "mistral and google arrived: {c:?}");
    assert_eq!(
        c.models_withdrawn, 1,
        "openai stopped being published: {c:?}"
    );
    assert!(
        c.facts_changed >= 1,
        "anthropic's rate moved on a model already known: {c:?}"
    );

    // The composition must not simply restate the row count, which is the
    // conflation it exists to prevent.
    assert!(
        c.facts_changed < report.eras_written,
        "an arriving model's facts must not be counted as changes: {} changed \
         of {} eras",
        c.facts_changed,
        report.eras_written
    );
}

/// The recorded `raw_hash` is a function of the bytes that arrived.
///
/// # The gap this closes, found by an age-selected sweep
///
/// `fetch.rs` had gone six days untouched, so mutations were written against
/// it from memory. Replacing `blake3::hash(bytes)` with `blake3::hash(&[])`
/// survived the entire module crate: every recorded `raw_hash` could have been
/// one constant and nothing would have failed.
///
/// The reason nothing caught it is visible in the fixtures. Every other test
/// that mentions the field SETS it — `Some("raw")`, `Some("seed-raw")`, `None`
/// — because they need an observation row, not a hash. A field written as a
/// literal in every test is a field no test can discover has become a literal
/// in production.
///
/// What it costs: `raw_hash` is the operator half of the two hashes. Its whole
/// job is answering "did the upstream change its bytes without changing its
/// facts" — reformatting, key reordering, a field fusiform does not read. A
/// constant answers "no" forever, and answers it in the direction nobody
/// investigates.
#[test]
fn the_raw_hash_distinguishes_the_documents_it_summarises() {
    let f = fixture();

    // Two documents that differ in bytes. Whether they differ in FACTS is not
    // the question here — that is `normalized_hash`, the consumer half.
    let first = String::from_utf8(FIXTURE.to_vec()).expect("the fixture is utf8");
    let second = mutate(&first, "\"attachment\": false", "\"attachment\":   false");

    let r1 = run(&f, body(first.as_bytes(), Some("e1")), 1_000);
    let h1 = f
        .store
        .raw_hash_of_observation(r1.observation_id)
        .expect("the hash must be readable");

    let r2 = run(&f, body(second.as_bytes(), Some("e2")), 2_000);
    let h2 = f
        .store
        .raw_hash_of_observation(r2.observation_id)
        .expect("the hash must be readable");

    assert!(
        h1.is_some() && h2.is_some(),
        "both polls must record a hash"
    );
    assert_ne!(
        h1, h2,
        "two documents differing only in whitespace must record different raw \
         hashes. This is the operator's drift signal: equal hashes here mean an \
         upstream that reformatted its document is indistinguishable from one \
         that did not publish at all."
    );

    // And stable for the same bytes, or "unchanged" would mean nothing either.
    let r3 = run(&f, body(first.as_bytes(), Some("e3")), 3_000);
    let h3 = f
        .store
        .raw_hash_of_observation(r3.observation_id)
        .expect("the hash must be readable");
    assert_eq!(
        h1, h3,
        "the same bytes must record the same hash, or a stable upstream would \
         look like it was churning"
    );
}
