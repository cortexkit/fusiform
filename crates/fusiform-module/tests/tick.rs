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
use fusiform_store::CatalogStore;

use fusiform_module::fetch::FetchOutcome;
use fusiform_module::loop_::{apply, TickOutcome};
use fusiform_module::signals::Signals;

const FIXTURE: &[u8] = include_bytes!("../../fusiform-core/fixtures/models-dev-excerpt.json");

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

fn run(f: &Fixture, outcome: FetchOutcome, at: i64) -> fusiform_module::loop_::TickReport {
    apply(&f.store, &f.signals, SourceId::ModelsDev, outcome, at).unwrap()
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
    assert_eq!(new_version, 1);
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
        .unwrap();
    assert_eq!(row.boundary_kind, fusiform_core::BoundaryKind::Seed);
    assert_eq!(row.observation_window(), None);
}

/// A second identical body writes an observation and no eras.
#[test]
fn an_unchanged_body_records_the_look_and_nothing_else() {
    let f = fixture();
    run(&f, body(FIXTURE, Some("\"v1\"")), 1_000);

    let report = run(&f, body(FIXTURE, Some("\"v1\"")), 2_000);
    assert_eq!(report.outcome, TickOutcome::Unchanged);
    assert_eq!(report.eras_written, 0);
    // The version does not move for a non-change: a consumer that refuses a
    // version it already holds must not be handed a new number for old content.
    assert_eq!(f.store.catalog_version().unwrap(), 1);
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
    assert_eq!(f.store.catalog_version().unwrap(), 1);

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
        1,
        "a version bump with no new content would have consumers hold a \
         high-water mark for a catalog they never received"
    );
}

/// The version advances once per change, monotonically.
#[test]
fn the_version_advances_once_per_change() {
    let f = fixture();
    run(&f, body(FIXTURE, None), 1_000);
    assert_eq!(f.store.catalog_version().unwrap(), 1);

    let text = String::from_utf8(FIXTURE.to_vec()).unwrap();
    for (i, (from, to)) in [
        ("\"input\": 3,", "\"input\": 4,"),
        ("\"input\": 4,", "\"input\": 5,"),
    ]
    .into_iter()
    .enumerate()
    {
        let mutated = text.replace(from, to);
        let at = 2_000 + i as i64 * 1_000;
        let report = run(&f, body(mutated.as_bytes(), None), at);
        assert!(
            matches!(report.outcome, TickOutcome::Changed { .. }),
            "round {i} should be a change"
        );
        assert_eq!(f.store.catalog_version().unwrap(), 2 + i as i64);
    }
}

/// Signals track what actually happened, so health cannot be fooled.
#[test]
fn signals_reflect_observations_rather_than_attempts() {
    let f = fixture();
    run(&f, body(FIXTURE, None), 1_000);
    assert_eq!(f.signals.poll_attempts(), 1);
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
    assert_eq!(f.signals.poll_attempts(), 4);
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
