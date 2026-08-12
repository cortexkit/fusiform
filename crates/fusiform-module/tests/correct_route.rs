//! The `catalog.correct` wire contract.
//!
//! This is the only route that writes, so the tests are about what it REFUSES
//! at least as much as what it does.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp, TokenClass};
use fusiform_module::route::{serve_tool_call, CorrectResponse, RouteError};
use fusiform_protocol::ToolResponse;
use fusiform_store::{CatalogStore, FactKey, NewEra, NewObservation, PointInTime};

/// The window: bad from t=3000, fixed at t=8000.
const BAD_FROM: i64 = 3_000;
const FIXED_AT: i64 = 8_000;

struct Fixture {
    store: CatalogStore,
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

    for at in [1_000, BAD_FROM, FIXED_AT] {
        store
            .record_observation(&NewObservation {
                source: SourceId::ModelsDev,
                observed_at: Timestamp(at),
                outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
                normalized_hash: Some(format!("h{at}")),
                raw_hash: None,
                etag: None,
                duration_ms: Some(10),
                detail: None,
            })
            .unwrap();
    }

    let era = |value: &str, at: i64, kind: BoundaryKind| NewEra {
        source: SourceId::ModelsDev,
        provider_id: "anthropic".into(),
        model_id: "claude-sonnet-4-5".into(),
        fact_key: FactKey::rate(TokenClass::Input),
        value_json: value.to_string(),
        boundary_at: Timestamp(at),
        boundary_kind: kind,
        observation_id: None,
    };

    store
        .append_eras(&[era(r#"{"units":3000000000}"#, 1_000, BoundaryKind::Seed)])
        .unwrap();
    store
        .append_eras(&[era(
            r#"{"units":999000000000}"#,
            BAD_FROM,
            BoundaryKind::Observed,
        )])
        .unwrap();
    store
        .append_eras(&[era(
            r#"{"units":3000000000}"#,
            FIXED_AT,
            BoundaryKind::Observed,
        )])
        .unwrap();

    Fixture { store, _dir: dir }
}

fn correct(f: &Fixture, args: &str) -> Result<CorrectResponse, RouteError> {
    let body = format!(r#"{{"name":"catalog.correct","arguments":{args}}}"#);
    match serve_tool_call(&f.store, body.as_bytes()) {
        Ok(ToolResponse::Correct(r)) => Ok(r),
        Ok(other) => panic!("catalog.correct returned the wrong response: {other:?}"),
        Err(e) => Err(e),
    }
}

const ARGS: &str = r#"{
    "provider_id":"anthropic",
    "model_id":"claude-sonnet-4-5",
    "fields":[{"field":"rate","class":"input"}],
    "affected_from_ms":3000,
    "affected_until_ms":8000,
    "reason":"docs/findings/2026-08-12-example.md"
}"#;

/// A correction defaults to a dry run.
///
/// The default matters more than it looks: this is the only command in the
/// module that changes what the catalog says about the past, and a write that
/// happens because a flag was forgotten is the wrong failure direction.
#[test]
fn a_correction_defaults_to_a_dry_run() {
    let f = fixture();
    let response = correct(&f, ARGS).expect("the correction must plan");

    assert!(!response.written, "the default must not write");
    assert_eq!(response.facts.len(), 1);
    assert_eq!(response.facts[0].fact_key, "rate.input");

    // And nothing was actually written: a read inside the window still answers.
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
    assert!(
        matches!(inside, PointInTime::Known(_)),
        "a dry run must leave the store untouched, got {inside:?}"
    );
}

/// With `dry_run: false` the correction is written and reads refuse.
#[test]
fn a_committed_correction_makes_the_window_refuse() {
    let f = fixture();
    let args = ARGS.replace(
        r#""reason":"docs/findings/2026-08-12-example.md""#,
        r#""reason":"docs/findings/2026-08-12-example.md","dry_run":false"#,
    );
    let response = correct(&f, &args).expect("the correction must apply");
    assert!(response.written);

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
            assert!(corrections[0].reason.contains("2026-08-12-example"));
        }
        other => panic!("the window must refuse after a commit, got {other:?}"),
    }
}

/// The response shows the value that stays in force, read from the store.
#[test]
fn the_response_reports_the_value_the_catalog_keeps() {
    let f = fixture();
    let response = correct(&f, ARGS).unwrap();

    // The corrected value, not the bad one. An operator sees what the catalog
    // will still say before they commit.
    assert_eq!(
        response.facts[0].value,
        serde_json::json!({"units": 3_000_000_000i64})
    );
    assert_eq!(response.facts[0].current_since_ms, FIXED_AT);
}

/// A correction with no reason is refused.
#[test]
fn a_correction_without_a_reason_is_refused() {
    let f = fixture();
    let args = ARGS.replace("docs/findings/2026-08-12-example.md", "   ");
    let err = correct(&f, &args).expect_err("an unauditable correction must be refused");
    assert_eq!(err.code, "bad_request");
    assert!(err.message.contains("reason"));
}

/// An unrecognised field is refused rather than skipped.
///
/// Skipping would write a correction with a NARROWER extent than the operator
/// stated — they would believe five facts were marked when four were, and the
/// fifth would keep serving a value everyone believes was corrected.
#[test]
fn an_unrecognised_field_is_refused_rather_than_dropped() {
    let f = fixture();
    let args = ARGS.replace(
        r#"[{"field":"rate","class":"input"}]"#,
        r#"[{"field":"rate","class":"input"},{"field":"vibes"}]"#,
    );
    let err = correct(&f, &args).expect_err("an unknown field must be refused");
    assert_eq!(err.code, "bad_request");
    assert!(
        err.message.contains("vibes"),
        "the error must name the field: {}",
        err.message
    );
}

/// An empty field list is refused.
#[test]
fn an_empty_field_list_is_refused() {
    let f = fixture();
    let args = ARGS.replace(r#"[{"field":"rate","class":"input"}]"#, "[]");
    let err = correct(&f, &args).expect_err("a correction naming nothing must be refused");
    assert_eq!(err.code, "bad_request");
}

/// There is no wildcard form.
///
/// A correction makes reads inside its window refuse, so a wildcard would be a
/// catalog kill switch. Fusiform cannot tell who is calling — RequestCtx
/// carries no consumer identity — so the blast radius is bounded by what the
/// request can EXPRESS rather than by who may send it.
#[test]
fn a_correction_cannot_name_all_models() {
    let f = fixture();

    for attempt in [
        // No provider.
        r#"{"model_id":"claude-sonnet-4-5","fields":[{"field":"rate","class":"input"}],"affected_from_ms":3000,"affected_until_ms":8000,"reason":"x"}"#,
        // No model.
        r#"{"provider_id":"anthropic","fields":[{"field":"rate","class":"input"}],"affected_from_ms":3000,"affected_until_ms":8000,"reason":"x"}"#,
        // A wildcard someone might reach for.
        r#"{"provider_id":"*","model_id":"*","fields":[{"field":"rate","class":"input"},{"field":"existence"}],"affected_from_ms":0,"affected_until_ms":8000,"reason":"x"}"#,
    ] {
        let result = correct(&f, attempt);
        assert!(
            result.is_err(),
            "a correction without one named model must be refused: {attempt}"
        );
    }
}

/// A window still containing the current value is refused, over the wire.
#[test]
fn a_window_containing_the_present_is_refused_with_instructions() {
    let f = fixture();
    let args = ARGS.replace(r#""affected_until_ms":8000"#, r#""affected_until_ms":9000"#);
    let err = correct(&f, &args).expect_err("this window must be refused");

    assert_eq!(err.code, "refused");
    assert!(
        err.message.contains("Poll first"),
        "the refusal must say how to resolve it: {}",
        err.message
    );
}

/// A dry run and a commit report the same facts.
///
/// Otherwise the preview is not a preview. An operator approves what the dry
/// run showed, so the commit must not resolve a different set.
#[test]
fn the_dry_run_and_the_commit_agree() {
    let f = fixture();
    let preview = correct(&f, ARGS).unwrap();

    let args = ARGS.replace(
        r#""reason":"docs/findings/2026-08-12-example.md""#,
        r#""reason":"docs/findings/2026-08-12-example.md","dry_run":false"#,
    );
    let committed = correct(&f, &args).unwrap();

    assert_eq!(
        preview.facts, committed.facts,
        "the commit must write what the preview showed"
    );
    assert_eq!(preview.affected_from_ms, committed.affected_from_ms);
    assert_eq!(preview.affected_until_ms, committed.affected_until_ms);
}
