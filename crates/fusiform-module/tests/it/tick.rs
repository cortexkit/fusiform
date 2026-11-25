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

fn body_with_sonnet_input_price(price: u64) -> Vec<u8> {
    let mut document: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    document["anthropic"]["models"]["claude-sonnet-4-5"]["cost"]["text"]["input"]["price"] =
        serde_json::json!(price);
    serde_json::to_vec(&document).unwrap()
}

fn run(f: &Fixture, outcome: FetchOutcome, at: i64) -> fusiform_module::loop_::TickReport {
    apply(&f.store, &f.signals, SourceId::ModelsDev, outcome, at).unwrap()
}

#[test]
fn a_refused_schedule_keeps_its_eras_and_a_true_omission_withdraws_it() {
    use fusiform_store::FactKey;
    let f = fixture();
    let good = br#"{"p":{"models":{"m":{"type":"chat","capabilities":{},"api":{},"cost":{"text":{
        "input":{"price":1,"overrides":[{"when":{"context":{"gt":200000}},"price":3}]},
        "output":{"price":2}
    }}}}}}"#;
    let refused =
        br#"{"p":{"models":{"m":{"type":"chat","capabilities":{},"api":{},"cost":{"text":{
        "input":{"price":"unreadable"},"output":{"price":40}
    }}}}}}"#;
    let omitted = br#"{"p":{"models":{"m":{"type":"chat","capabilities":{},"api":{}}}}}"#;
    let histories = || {
        [
            FactKey::rate(fusiform_core::TokenClass::Input),
            FactKey::rate(fusiform_core::TokenClass::Output),
            FactKey::rate_above_context(fusiform_core::TokenClass::Input, 200_000),
            // V2 projects the unchanged output price into the input override's context band.
            FactKey::rate_above_context(fusiform_core::TokenClass::Output, 200_000),
        ]
        .map(|key| {
            f.store
                .fact_history(SourceId::ModelsDev, "p", "m", &key)
                .unwrap()
        })
    };

    run(&f, body(good, None), 1_000);
    let initial = histories();
    assert!(initial.iter().all(|history| history.len() == 1));
    let partial = run(&f, body(refused, None), 2_000);
    assert_eq!(
        histories(),
        initial,
        "an unreadable schedule must not withdraw or restate the held rates"
    );
    assert_eq!(partial.eras_written, 0);
    let refused_hash = f
        .store
        .last_normalized_hash(SourceId::ModelsDev)
        .unwrap()
        .unwrap();

    let missing = run(&f, body(omitted, None), 3_000);
    assert_eq!(missing.eras_written, 4);
    for (new, old) in histories().iter().zip(initial.iter()) {
        assert_eq!(new.len(), 2);
        assert_eq!(&new[0], &old[0]);
        assert_eq!(new[1].boundary_at, Timestamp(3_000));
        assert!(new[1].value_json.contains("missing_rate"), "{:?}", new[1]);
    }
    assert_ne!(
        f.store
            .last_normalized_hash(SourceId::ModelsDev)
            .unwrap()
            .unwrap(),
        refused_hash,
        "a genuine withdrawal cannot reuse the digest of an unreadable schedule"
    );
}

#[test]
fn unreadable_facts_keep_their_boundaries_while_readable_siblings_land() {
    use fusiform_store::FactKey;
    let f = fixture();
    let document = |input, output, context, temperature, fast| {
        format!(
            r#"
        {{"p":{{"models":{{"m":{{"type":"chat","api":{{}},
            "limit":{{"context":{context},"output":{output}}},
            "capabilities":{{"temperature":{temperature}}},"cost":{{"text":{{"input":{{"price":{input}}}}}}},
            "experimental":{{"modes":{{"fast":{{"cost":{fast}}}}}}}
        }}}}}}}}
    "#
        )
    };
    let history = |key| {
        f.store
            .fact_history(SourceId::ModelsDev, "p", "m", &key)
            .unwrap()
    };

    let fast_cost = r#"{"text":{"input":{"price":3}}}"#;
    let first = document("1", 100, "200000", "true", fast_cost);
    run(&f, body(first.as_bytes(), None), 1_000);
    let rate = FactKey::rate(fusiform_core::TokenClass::Input);
    let mode = FactKey::rate_in_mode(fusiform_core::TokenClass::Input, "fast");
    let held = [
        rate.clone(),
        mode,
        FactKey::limit("context"),
        FactKey::capability("temperature"),
    ];
    let initial = held.clone().map(&history);

    let bad = document("-1", 200, "200000.0", r#""yes""#, r#""unreadable""#);
    let partial = run(&f, body(bad.as_bytes(), None), 2_000);
    assert!(
        matches!(partial.outcome, TickOutcome::Changed { .. }),
        "healthy limit.output still changes; got {:?}",
        partial.outcome
    );
    assert_eq!(partial.eras_written, 1);
    assert_eq!(
        held.clone().map(&history),
        initial,
        "no refused fact gets a new era"
    );
    assert_eq!(history(FactKey::limit("output"))[1].value_json, "200");

    let same = document("1", 200, "200000", "true", fast_cost);
    assert_eq!(run(&f, body(same.as_bytes(), None), 3_000).eras_written, 0);
    assert_eq!(held.clone().map(&history), initial);
    let changed = document("2", 200, "200000", "true", fast_cost);
    assert_eq!(
        run(&f, body(changed.as_bytes(), None), 4_000).eras_written,
        1
    );
    assert_eq!(history(rate)[1].boundary_at, Timestamp(4_000));
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
    let document = |input, output, audio| {
        format!(
            r#"{{"p":{{"models":{{
                "bad":{{"type":"chat","capabilities":{{}},"api":{{}},"cost":{{"text":{{
                    "input":{{"price":{input}}},"output":{{"price":{output}}}
                }}}}}},
                "good":{{"type":"chat","capabilities":{{}},"api":{{}},"cost":{{
                    "text":{{"input":{{"price":1}},"output":{{"price":2}}}}{audio}
                }}}}
            }}}}}}"#
        )
    };
    let bad = document(
        r#""1.25""#,
        r#""2.5""#,
        r#", "audio":{"input":{"unit":"token","price":3}}"#,
    );
    let first = run(&f, body(bad.as_bytes(), None), 1_000);
    assert!(matches!(first.outcome, TickOutcome::Changed { .. }));
    let expected = json!({"observation_id":first.observation_id,
                          "counts":{"refused_price":2,"unattributable_charge_unit":1}});
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

    let good = document("1", "2", "");
    let last = run(&f, body(good.as_bytes(), None), 3_000);
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
fn a_rewound_clock_does_not_hide_the_last_committed_digest() {
    use fusiform_core::ObservationOutcome;
    use fusiform_store::NewObservation;

    let f = fixture();
    let source = SourceId::ModelsDev;
    for (at, digest) in [(5_000, "before-rewind"), (3_000, "after-rewind")] {
        f.store
            .record_observation(&NewObservation {
                source,
                observed_at: Timestamp(at),
                outcome: ObservationOutcome::Unchanged,
                normalized_hash: Some(digest.into()),
                raw_hash: None,
                etag: None,
                duration_ms: None,
                detail: None,
            })
            .unwrap();
    }
    assert_eq!(
        f.store.last_normalized_hash(source).unwrap().as_deref(),
        Some("after-rewind"),
        "the last committed body, not the latest clock reading, supplies the digest"
    );
}

#[test]
fn a_changed_poll_cannot_write_behind_a_later_observation() {
    let f = fixture();
    let source = SourceId::ModelsDev;
    run(&f, body(FIXTURE, Some("seed")), 1_000);
    let version = f.store.catalog_version().unwrap();
    let second = body_with_extra_model(true);
    for (outcome, at) in [
        (body(FIXTURE, Some("unchanged")), 5_000),
        (
            FetchOutcome::NotModified {
                etag: Some("not-modified".into()),
                duration: Duration::from_millis(10),
            },
            6_000,
        ),
    ] {
        let confirmed = run(&f, outcome, at);
        assert!(matches!(
            confirmed.outcome,
            TickOutcome::Unchanged | TickOutcome::NotModified
        ));
        let etag = f.store.last_etag(source).unwrap();
        for rewound in [at - 500, at] {
            let refused = run(&f, body(&second, Some("refused")), rewound);
            assert_eq!(
                refused.outcome,
                TickOutcome::Failed {
                    class: FailureClass::Implausible
                },
                "the changing poll at {rewound} must not write behind the observation at {at}"
            );
            assert_eq!(refused.eras_written, 0);
            assert_eq!(f.store.catalog_version().unwrap(), version);
            assert_eq!(f.store.last_etag(source).unwrap(), etag);
        }
    }
    assert!(matches!(
        run(&f, body(&second, Some("retry")), 6_001).outcome,
        TickOutcome::Changed { .. }
    ));
}

#[test]
fn a_poll_overtaken_by_a_correction_records_a_transient_failure_and_retries() {
    use fusiform_core::{FieldId, LimitId};
    use fusiform_store::correct::{apply_correction, plan_correction};
    use serde_json::{json, Value};
    use subc_protocol::session::HealthStatus;

    let f = fixture();
    f.signals.store_opened();
    let source = SourceId::ModelsDev;
    let provider = "anthropic";
    let model = "claude-sonnet-4-5";
    let document = |context| {
        let mut doc: Value = serde_json::from_slice(FIXTURE).unwrap();
        doc[provider]["models"][model]["limit"]["context"] = json!(context);
        serde_json::to_vec(&doc).unwrap()
    };
    for (at, limit) in [(1_000, 10_000), (2_000, 99_000), (4_000, 10_000)] {
        assert!(matches!(
            run(&f, body(&document(limit), None), at).outcome,
            TickOutcome::Changed { .. }
        ));
    }
    let correction = plan_correction(
        &f.store,
        source,
        provider,
        model,
        &[FieldId::Limit {
            limit: LimitId::Context,
        }],
        Timestamp(2_000),
        Timestamp(3_999),
        "wrong context limit",
        Timestamp(5_000),
    )
    .unwrap()
    .unwrap();
    apply_correction(&f.store, source, &correction, Timestamp(5_000)).unwrap();
    let version = f.store.catalog_version().unwrap();
    let updated = document(20_000);
    let report = run(&f, body(&updated, Some("overtaken")), 4_500);
    assert_eq!(
        report.outcome,
        TickOutcome::Failed {
            class: FailureClass::Transient
        }
    );
    let observation = f
        .store
        .recent_observations(source, 5)
        .unwrap()
        .into_iter()
        .find(|row| row.id == report.observation_id)
        .unwrap();
    assert_eq!(observation.outcome, "failed");
    assert_eq!(
        observation.failure_class.as_deref(),
        Some("transient"),
        "a correction overtaking a poll is not an implausible document or clock fault"
    );
    let detail = observation.detail.unwrap();
    assert!(
        detail.contains("correction") && detail.contains("5000"),
        "{detail}"
    );
    assert_eq!(report.eras_written, 0);
    assert_eq!(f.store.catalog_version().unwrap(), version);
    assert_ne!(
        f.store.last_etag(source).unwrap().as_deref(),
        Some("overtaken")
    );
    let health = health::report(&f.signals, 4_500);
    assert_eq!(health.status, HealthStatus::Ok);
    assert_eq!(health.metrics.unwrap()["last_failure_class"], "transient");
    let status = fusiform_module::route::serve_status(&f.store, b"{}").unwrap();
    assert_eq!(
        status.failures.unwrap().last_class.as_deref(),
        Some("transient")
    );
    let poll = status
        .recent_polls
        .iter()
        .find(|poll| poll.observed_at_ms == 4_500)
        .unwrap();
    assert_eq!(poll.failure_class.as_deref(), Some("transient"));

    // If an ordinary observation also blocks the write, it is not just the
    // correction that overtook the poll.
    assert_eq!(
        run(&f, body(&updated, None), 4_000).outcome,
        TickOutcome::Failed {
            class: FailureClass::Implausible
        }
    );

    let retry = run(&f, body(&updated, Some("retry")), 5_001);
    assert!(matches!(retry.outcome, TickOutcome::Changed { .. }));
    let era = f
        .store
        .recorded_value_at(
            source,
            provider,
            model,
            &fusiform_store::FactKey::limit("context"),
            Timestamp(5_001),
        )
        .unwrap()
        .unwrap();
    assert_eq!(era.value_json, "20000");
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
    let mutated = body_with_sonnet_input_price(4);
    let report = run(&f, body(&mutated, Some("\"v2\"")), 3_000);
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

    let mutated = body_with_sonnet_input_price(4);
    run(&f, body(&mutated, None), 4_000);

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

    for (i, price) in [4, 5].into_iter().enumerate() {
        let mutated = body_with_sonnet_input_price(price);
        let at = 2_000 + i as i64 * 1_000;
        let report = run(&f, body(&mutated, None), at);
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

    let repriced = body_with_sonnet_input_price(4);
    let report = run(&f, body(&repriced, None), 3_000);
    assert_eq!(report.eras_written, 1);
    let after_rate = status_versions(&f.store);
    assert_eq!(after_rate, (3_000, 3_000));
    assert!(after_rate.0 > before.0 && after_rate.1 > before.1);

    let mut changed: serde_json::Value = serde_json::from_slice(&repriced).unwrap();
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

    let old_catalog = std::process::Command::new("gzip")
        .args([
            "-dc",
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../fusiform-core/tests/data/models-dev-v2/api-v1-20261125.json.gz"
            ),
        ])
        .output()
        .unwrap();
    assert!(old_catalog.status.success());
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
        (
            body(&old_catalog.stdout, None),
            FailureClass::Parse,
            "parse",
        ),
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

    let doc = |models: &[(&str, &str, f64)]| {
        let providers: serde_json::Map<String, serde_json::Value> = models
            .iter()
            .map(|(prov, id, rate)| {
                (
                    (*prov).to_owned(),
                    serde_json::json!({"name": prov, "models": {*id: {
                        "type": "chat", "name": id, "capabilities": {}, "api": {},
                        "cost": {"text": {"input": {"price": rate}, "output": {"price": rate}}}
                    }}}),
                )
            })
            .collect();
        serde_json::to_vec(&providers).unwrap()
    };

    // Seed: two models.
    let first = doc(&[("anthropic", "sonnet", 3.0), ("openai", "gpt5", 2.0)]);
    let seeded = run(&f, body(&first, None), 1_000);
    assert!(seeded.eras_written > 0, "the seed must write something");

    // Next poll: one rate moves on a known model, TWO models arrive, one goes.
    //
    // Asymmetric counts on purpose. An earlier version used one arrival and one
    // withdrawal, and a mutation swapping the two fields survived — the test
    // could not tell them apart. Two and one can.
    let second = doc(&[
        ("anthropic", "sonnet", 4.0),
        ("mistral", "large", 1.0),
        ("google", "gemini", 5.0),
    ]);
    let report = run(&f, body(&second, None), 2_000);

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
    let second = mutate(
        &first,
        "\"name\": \"Claude Sonnet 4.5 (latest)\"",
        "\"name\":   \"Claude Sonnet 4.5 (latest)\"",
    );

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
