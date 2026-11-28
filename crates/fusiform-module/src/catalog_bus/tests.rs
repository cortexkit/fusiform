use super::*;
use std::sync::{atomic::AtomicUsize, atomic::Ordering, Mutex};

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, Timestamp};
use fusiform_store::{FactKey, NewEra, QualityGrade};
use serde_json::{json, Value};

#[derive(Default)]
struct Fake {
    attempts: Mutex<Vec<(String, Value)>>,
    failures_left: AtomicUsize,
    refusal: Option<(&'static str, Option<bool>)>,
    blocked: bool,
}

#[async_trait]
impl EventPublisher for Arc<Fake> {
    async fn publish(&mut self, msg_id: &str, body: Vec<u8>) -> Result<(), PublishError> {
        self.attempts
            .lock()
            .unwrap()
            .push((msg_id.into(), serde_json::from_slice(&body).unwrap()));
        if self.blocked {
            std::future::pending::<()>().await;
        }
        if self.failures_left.load(Ordering::Relaxed) > 0 {
            self.failures_left.fetch_sub(1, Ordering::Relaxed);
            if let Some((code, expected_back)) = self.refusal {
                return Err(cortexkit_bus_client::Error::RouteRefused {
                    code: code.into(),
                    expected_back,
                }
                .into());
            }
            return Err(std::io::Error::other("synthetic publish failure").into());
        }
        Ok(())
    }
}

impl Fake {
    fn versions(&self) -> Vec<i64> {
        self.attempts
            .lock()
            .unwrap()
            .iter()
            .map(|(id, body)| {
                let version = body["catalog_version"].as_i64().unwrap();
                assert_eq!(body, &json!({"catalog_version": version}));
                assert_eq!(id, &version.to_string());
                version
            })
            .collect()
    }
}

fn store() -> (Arc<CatalogStore>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    (Arc::new(store), dir)
}

fn grade(store: &CatalogStore, at: i64) {
    store
        .append_quality_grade(
            "synthetic/model",
            None,
            Some(&QualityGrade::Chat {
                elo: serde_json::from_str("1500").unwrap(),
                eq: serde_json::from_str("1600.5").unwrap(),
            }),
            "operator",
            "synthetic evaluation",
            at,
        )
        .unwrap();
}

async fn settle() {
    // The fake never needs I/O. Yield without advancing paused time so tests
    // distinguish an immediate notification from the minute-long backstop.
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn publishes_after_startup_advance_and_grade_commit() {
    let (store, _dir) = store();
    store
        .advance_catalog_version_at_start(Timestamp(100))
        .unwrap();
    let fake = Arc::new(Fake::default());
    let signals = Arc::new(Signals::new());
    let task = tokio::spawn(run_publisher(store.clone(), signals.clone(), fake.clone()));
    settle().await;
    assert_eq!(fake.versions(), [100]);

    grade(&store, 200);
    settle().await;
    assert_eq!(fake.versions(), [100, 200]);
    assert_eq!(signals.bus_published_version(), Some(200));
    assert_eq!(signals.bus_publish_failures(), 0);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn local_media_grade_and_withdrawal_wake_the_publisher() {
    let (store, _dir) = store();
    let fake = Arc::new(Fake::default());
    let task = tokio::spawn(run_publisher(
        store.clone(),
        Arc::new(Signals::new()),
        fake.clone(),
    ));
    settle().await;
    store
        .append_quality_grade(
            "local:apple/phone-transcriber",
            Some("transcription"),
            Some(&QualityGrade::NonChat {
                grade: serde_json::from_str("59").unwrap(),
            }),
            "operator",
            "local evaluation",
            200,
        )
        .unwrap();
    settle().await;
    assert_eq!(store.catalog_version().unwrap(), 200);
    assert_eq!(fake.versions(), [0, 200]);
    store
        .append_quality_grade(
            "local:apple/phone-transcriber",
            Some("transcription"),
            None,
            "operator",
            "withdrawal",
            300,
        )
        .unwrap();
    settle().await;
    assert_eq!(store.catalog_version().unwrap(), 300);
    assert_eq!(fake.versions(), [0, 200, 300]);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn every_version_advancing_write_wakes_the_publisher_after_commit() {
    let (store, _dir) = store();
    let fake = Arc::new(Fake::default());
    let task = tokio::spawn(run_publisher(
        store.clone(),
        Arc::new(Signals::new()),
        fake.clone(),
    ));
    settle().await;
    store
        .advance_catalog_version_at_start(Timestamp(100))
        .unwrap();
    settle().await;
    assert_eq!(fake.versions(), [0, 100]);
    grade(&store, 200);
    settle().await;
    assert_eq!(
        store
            .quality_grade_history("synthetic/model")
            .unwrap()
            .len(),
        1
    );
    store
        .append_quality_grade("synthetic/model", None, None, "operator", "withdrawal", 300)
        .unwrap();
    settle().await;
    assert!(store
        .quality_grades()
        .unwrap()
        .iter()
        .all(|era| era.model != "synthetic/model"));
    store
        .append_model_mark("synthetic/model", true, "operator", "decommission", 400)
        .unwrap();
    settle().await;
    assert_eq!(
        store.model_mark_history("synthetic/model").unwrap().len(),
        1
    );
    store
        .append_model_mark("synthetic/model", false, "operator", "restore", 500)
        .unwrap();
    settle().await;
    assert!(store
        .model_marks()
        .unwrap()
        .iter()
        .all(|era| era.model != "synthetic/model"));
    let observation = store
        .record_observation(&fusiform_store::NewObservation {
            source: fusiform_core::SourceId::ModelsDev,
            observed_at: Timestamp(600),
            outcome: fusiform_core::ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    store
        .advance_catalog_version(observation, Timestamp(600))
        .unwrap();
    settle().await;
    assert_eq!(fake.versions(), [0, 100, 200, 300, 400, 500, 600]);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn coalesces_advances_and_never_republishes_a_high_water_mark() {
    let (store, _dir) = store();
    let fake = Arc::new(Fake::default());
    let task = tokio::spawn(run_publisher(
        store.clone(),
        Arc::new(Signals::new()),
        fake.clone(),
    ));
    settle().await;
    grade(&store, 100);
    grade(&store, 200);
    grade(&store, 300);
    settle().await;
    assert_eq!(fake.versions(), [0, 300]);
    // A stale store read, as after a restore, must not send a lower version.
    let conn = rusqlite::Connection::open(store.path()).unwrap();
    conn.execute("UPDATE catalog_version SET version = 50 WHERE id = 1", [])
        .unwrap();
    tokio::time::advance(RECHECK).await;
    settle().await;
    assert_eq!(fake.versions(), [0, 300]);
    grade(&store, 400);
    settle().await;
    assert_eq!(fake.versions(), [0, 300, 400]);
    tokio::time::advance(RECHECK).await;
    settle().await;
    assert_eq!(fake.versions(), [0, 300, 400]);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn retries_with_capped_backoff_and_reads_the_newest_version() {
    assert_short_retries(None, "synthetic publish failure").await;
    assert_short_retries(
        Some(("target_unavailable", None)),
        "ck-bus route refused: target_unavailable (unknown whether ck-bus is expected back)",
    )
    .await;
}

async fn assert_short_retries(refusal: Option<(&'static str, Option<bool>)>, last_error: &str) {
    let (store, _dir) = store();
    let fake = Arc::new(Fake {
        refusal,
        ..Fake::default()
    });
    fake.failures_left.store(8, Ordering::Relaxed);
    let signals = Arc::new(Signals::new());
    let task = tokio::spawn(run_publisher(store.clone(), signals.clone(), fake.clone()));
    settle().await;
    assert_eq!(signals.bus_published_version(), None);
    grade(&store, 100);
    grade(&store, 200);
    settle().await;
    assert_eq!(
        fake.versions(),
        [0],
        "a write must not bypass retry backoff"
    );

    for (index, seconds) in [1, 2, 4, 8, 16, 32, 60, 60].into_iter().enumerate() {
        tokio::time::advance(Duration::from_millis(seconds * 1000 - 1)).await;
        settle().await;
        assert_eq!(fake.versions().len(), index + 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
        assert_eq!(fake.versions().len(), index + 2);
    }
    assert_eq!(fake.versions(), [0, 200, 200, 200, 200, 200, 200, 200, 200]);
    assert_eq!(signals.bus_published_version(), Some(200));
    assert_eq!(signals.bus_publish_failures(), 8);
    assert_eq!(signals.bus_last_error().as_deref(), Some(last_error));
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn absent_bus_uses_long_backoff_and_success_resets_it() {
    let (store, _dir) = store();
    let fake = Arc::new(Fake {
        refusal: Some(("target_unavailable", Some(false))),
        ..Fake::default()
    });
    fake.failures_left.store(7, Ordering::Relaxed);
    let signals = Arc::new(Signals::new());
    let task = tokio::spawn(run_publisher(store.clone(), signals.clone(), fake.clone()));
    settle().await;
    assert_eq!(fake.versions(), [0]);
    grade(&store, 100);

    for (index, seconds) in [30, 60, 120, 240, 480, 900, 900].into_iter().enumerate() {
        tokio::time::advance(Duration::from_millis(seconds * 1000 - 1)).await;
        settle().await;
        assert_eq!(
            fake.versions().len(),
            index + 1,
            "the retry must wait the full {seconds} s"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
        assert_eq!(fake.versions().len(), index + 2);
    }
    assert_eq!(signals.bus_published_version(), Some(100));
    assert_eq!(signals.bus_publish_failures(), 7);

    fake.failures_left.store(1, Ordering::Relaxed);
    grade(&store, 200);
    settle().await;
    assert_eq!(fake.versions().len(), 9);
    tokio::time::advance(Duration::from_millis(29_999)).await;
    settle().await;
    assert_eq!(fake.versions().len(), 9);
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(signals.bus_published_version(), Some(200));
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn an_unsignalled_advance_is_found_by_the_periodic_check() {
    let (store, _dir) = store();
    let fake = Arc::new(Fake::default());
    let task = tokio::spawn(run_publisher(
        store.clone(),
        Arc::new(Signals::new()),
        fake.clone(),
    ));
    settle().await;
    let conn = rusqlite::Connection::open(store.path()).unwrap();
    conn.execute("UPDATE catalog_version SET version = 900 WHERE id = 1", [])
        .unwrap();
    tokio::time::advance(Duration::from_secs(59)).await;
    settle().await;
    assert_eq!(fake.versions(), [0]);
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(fake.versions(), [0, 900]);
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn grade_commit_is_found_within_sixty_seconds() {
    let (store, _dir) = store();
    let fake = Arc::new(Fake::default());
    let task = tokio::spawn(run_publisher(
        store.clone(),
        Arc::new(Signals::new()),
        fake.clone(),
    ));
    settle().await;
    grade(&store, 1000);
    tokio::time::advance(RECHECK).await;
    settle().await;
    assert_eq!(fake.versions(), [0, 1000]);
    task.abort();
}

#[tokio::test]
async fn tool_reply_does_not_wait_for_the_bus_and_status_exposes_counters() {
    let (store, _dir) = store();
    store
        .append_eras(&[NewEra {
            source: fusiform_core::SourceId::ModelsDev,
            provider_id: "synthetic".into(),
            model_id: "model".into(),
            fact_key: FactKey::existence(),
            value_json: "\"present\"".into(),
            boundary_at: Timestamp(100),
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        }])
        .unwrap();
    let fake = Arc::new(Fake {
        blocked: true,
        ..Fake::default()
    });
    let signals = Arc::new(Signals::new());
    let task = tokio::spawn(run_publisher(store.clone(), signals.clone(), fake.clone()));
    settle().await;
    assert_eq!(fake.versions(), [0], "the bus must actually be waiting");
    let body = br#"{"name":"quality.grade","arguments":{"model":"synthetic/model","elo":1500,"eq":1600.5,"reason":"synthetic evaluation","dry_run":false}}"#.to_vec();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        crate::serve_request(store.clone(), signals.clone(), body),
    )
    .await
    .expect("a tool reply must not wait for publication");
    assert!(
        matches!(response, subc_client_rs::HandlerOutcome::Response(_)),
        "{response:?}"
    );
    assert_eq!(
        store
            .quality_grade_history("synthetic/model")
            .unwrap()
            .len(),
        1
    );
    assert_eq!(signals.bus_published_version(), None);

    signals.store_opened();
    signals.observed(100);
    let before = crate::health::report(&signals, 100).status;
    signals.bus_published(123);
    signals.bus_publish_failed(&std::io::Error::other("synthetic status error"));
    let response = crate::serve_request(
        store,
        signals.clone(),
        br#"{"name":"catalog.status"}"#.to_vec(),
    )
    .await;
    let subc_client_rs::HandlerOutcome::Response(bytes) = response else {
        panic!("{response:?}")
    };
    let status: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status["bus_published_version"], 123);
    assert_eq!(status["bus_publish_failures"], 1);
    assert_eq!(status["bus_last_error"], "synthetic status error");
    let health = crate::health::report(&signals, 100);
    assert_eq!(
        health.status, before,
        "bus failures cannot degrade serving health"
    );
    let metrics = serde_json::to_value(health).unwrap();
    assert_eq!(metrics["metrics"]["process_bus_published_version"], 123);
    assert_eq!(metrics["metrics"]["process_bus_publish_failures"], 1);
    assert_eq!(
        metrics["metrics"]["process_bus_last_error"],
        "synthetic status error"
    );
    task.abort();
}
