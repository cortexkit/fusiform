//! The `catalog.get` contract, exercised without a socket.
//!
//! What is being tested is the request/response contract a consumer depends on:
//! that a malformed request is refused rather than answered with a default,
//! that a point-in-time request is honoured rather than silently served as
//! current, and that fact values arrive as values rather than as strings
//! containing values.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::{serve_catalog_get, CatalogGetResponse};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};

const FIXTURE: &str = include_str!("../../fusiform-core/fixtures/models-dev-excerpt.json");

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

    let catalog = normalize_models_dev(FIXTURE.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();

    Fixture { store, _dir: dir }
}

fn get(f: &Fixture, body: &str) -> CatalogGetResponse {
    serve_catalog_get(&f.store, body.as_bytes()).expect("the request must be served")
}

/// An empty body is a request for the whole current catalog.
#[test]
fn an_empty_request_returns_the_current_catalog() {
    let f = fixture();

    let from_empty = get(&f, "");
    let from_object = get(&f, "{}");

    assert!(from_empty.model_count() > 10);
    assert_eq!(
        from_empty.models.keys().collect::<Vec<_>>(),
        from_object.models.keys().collect::<Vec<_>>()
    );
    assert_eq!(from_empty.source, "models.dev");

    // A model's identity is the provider_id/model_id pair, never the bare
    // model id: 6,253 model rows carry only 2,957 distinct ids.
    assert!(
        from_empty.models.keys().all(|k| k.contains('/')),
        "every key must be provider/model"
    );
}

/// Fact values arrive as JSON values, not as strings containing JSON.
///
/// The failure this guards is subtle at the wire: a consumer receiving
/// `"{\"units\":3000000000}"` can still parse it, so nothing breaks
/// immediately — it breaks when a second consumer reads the field as a number
/// and gets a string.
#[test]
fn fact_values_are_values_not_strings() {
    let f = fixture();
    let response = get(&f, "{}");

    let sonnet = response
        .models
        .get("anthropic/claude-sonnet-4-5")
        .expect("the fixture carries this model");

    let rate = sonnet.get("rate.input").expect("an input rate");
    assert!(
        rate.is_object(),
        "a rate must arrive as an object, got {rate:?}"
    );
    assert_eq!(
        rate.get("units").and_then(|u| u.as_i64()),
        Some(3_000_000_000)
    );

    let limit = sonnet.get("limit.context").expect("a context limit");
    assert_eq!(
        limit.as_u64(),
        Some(1_000_000),
        "a limit must arrive as a number, got {limit:?}"
    );
}

/// A point-in-time request is honoured rather than served as current.
#[test]
fn a_point_in_time_request_is_honoured() {
    let f = fixture();

    // A change after the seed.
    let mut doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    doc.get_mut("anthropic")
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.get_mut("claude-sonnet-4-5"))
        .and_then(|m| m.get_mut("cost"))
        .and_then(|c| c.as_object_mut())
        .unwrap()
        .insert("input".to_string(), serde_json::json!(11.0));

    let obs = f
        .store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(2_000),
            outcome: ObservationOutcome::Unchanged,
            normalized_hash: Some("h".to_string()),
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let catalog = normalize_models_dev(&serde_json::to_vec(&doc).unwrap())
        .unwrap()
        .catalog;
    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(3_000),
        BoundaryKind::Observed,
        Some(obs),
    )
    .unwrap();
    f.store.append_eras(&plan.eras).unwrap();

    let then = get(&f, r#"{"at_ms": 2500}"#);
    let now = get(&f, "{}");

    let rate_at = |r: &CatalogGetResponse| {
        r.models
            .get("anthropic/claude-sonnet-4-5")
            .and_then(|m| m.get("rate.input"))
            .and_then(|v| v.get("units"))
            .and_then(|u| u.as_i64())
            .unwrap()
    };

    assert_eq!(rate_at(&then), 3_000_000_000, "the rate in force then");
    assert_eq!(rate_at(&now), 11_000_000_000, "the rate in force now");
    assert_eq!(then.resolved_at_ms, 2_500);
    assert!(now.resolved_at_ms > 1_700_000_000_000);
}

/// A plane filter narrows the response.
#[test]
fn a_fact_prefix_filter_narrows_the_response() {
    let f = fixture();

    let all = get(&f, "{}");
    let rates = get(&f, r#"{"fact_prefixes": ["rate."]}"#);

    assert!(rates.fact_count() < all.fact_count());
    assert!(rates.model_count() > 0);
    for facts in rates.models.values() {
        for key in facts.keys() {
            assert!(key.starts_with("rate."), "the filter must exclude {key}");
        }
    }

    // A tiered rate key must survive the `rate.` prefix, since it is part of
    // the same plane.
    let tiered: usize = rates
        .models
        .values()
        .flat_map(|f| f.keys())
        .filter(|k| k.contains(".above_context."))
        .count();
    assert!(tiered > 0, "tiered rates belong to the rate plane");
}

/// A single-model request returns one model.
#[test]
fn a_single_model_request_returns_one_model() {
    let f = fixture();

    let one = get(
        &f,
        r#"{"provider_id": "anthropic", "model_id": "claude-sonnet-4-5"}"#,
    );
    assert_eq!(one.model_count(), 1);
    assert!(one.models.contains_key("anthropic/claude-sonnet-4-5"));

    // A model that does not exist is an empty result, not an error: "no such
    // model" is a legitimate answer to a question about a catalog.
    let none = get(
        &f,
        r#"{"provider_id": "anthropic", "model_id": "no-such-model"}"#,
    );
    assert_eq!(none.model_count(), 0);
}

/// A bare model id is refused, because it is not unique.
#[test]
fn a_model_id_without_a_provider_is_refused() {
    let f = fixture();

    let err = serve_catalog_get(&f.store, br#"{"model_id": "claude-sonnet-4-5"}"#)
        .expect_err("a bare model id must be refused");
    assert_eq!(err.code, "bad_request");
    assert!(
        err.message.contains("provider_id"),
        "the error must name the fix: {}",
        err.message
    );
}

/// A malformed request is refused rather than answered with a default.
///
/// The failure this guards: a consumer misspells `fact_prefixes`, receives the
/// full 3.0 MB catalog, and never learns its filter did nothing.
#[test]
fn a_malformed_request_is_refused_rather_than_defaulted() {
    let f = fixture();

    for body in [
        r#"{"fact_prefix": ["rate."]}"#, // misspelled field
        r#"{"at_ms": "yesterday"}"#,     // wrong type
        r#"{"source": "modelsdev"}"#,    // unknown source
        r#"not json at all"#,
    ] {
        match serve_catalog_get(&f.store, body.as_bytes()) {
            Err(e) => assert_eq!(
                e.code, "bad_request",
                "{body} must be refused as a bad request, got {e:?}"
            ),
            Ok(response) => panic!(
                "{body} was answered with {} models instead of being refused",
                response.model_count()
            ),
        }
    }

    // And a well-formed request with the CORRECT spelling still works, so the
    // refusals above are the malformation's doing rather than everything being
    // rejected.
    let ok = get(&f, r#"{"fact_prefixes": ["rate."]}"#);
    assert!(ok.model_count() > 0);
}

/// A retired model is excluded by default and included on request.
#[test]
fn retired_models_are_excluded_by_default() {
    let f = fixture();

    // Withdraw a model.
    let mut doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    doc.get_mut("anthropic")
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.as_object_mut())
        .unwrap()
        .remove("claude-sonnet-4-5");

    let obs = f
        .store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(2_000),
            outcome: ObservationOutcome::Unchanged,
            normalized_hash: Some("h".to_string()),
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let catalog = normalize_models_dev(&serde_json::to_vec(&doc).unwrap())
        .unwrap()
        .catalog;
    let plan = plan_ingest(
        &f.store,
        &catalog,
        Timestamp(3_000),
        BoundaryKind::Observed,
        Some(obs),
    )
    .unwrap();
    f.store.append_eras(&plan.eras).unwrap();

    let default = get(&f, "{}");
    assert!(!default.models.contains_key("anthropic/claude-sonnet-4-5"));

    let audit = get(&f, r#"{"include_retired": true}"#);
    let retired = audit
        .models
        .get("anthropic/claude-sonnet-4-5")
        .expect("an audit read must find it");
    assert_eq!(
        retired.get("existence").and_then(|v| v.as_str()),
        Some("absent")
    );
}

/// The response carries the catalog version.
#[test]
fn the_response_carries_the_catalog_version() {
    let f = fixture();

    let before = get(&f, "{}").catalog_version;
    let obs = f
        .store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(2_000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some("h2".to_string()),
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    f.store
        .advance_catalog_version(before + 1, obs, Timestamp(2_000))
        .unwrap();

    assert_eq!(get(&f, "{}").catalog_version, before + 1);
}

/// The wire envelope is unwrapped, and a wrong tool name is refused.
///
/// A tool call arrives as `{"name": ..., "arguments": {...}}` and the module
/// receives it intact — subc does not unwrap it. Passing that straight to the
/// request parser is refused with `unknown field "name"`, which is how this was
/// found: the handler was wired to the argument parser and would have refused
/// every real call.
#[test]
fn the_tool_call_envelope_is_unwrapped() {
    let f = fixture();

    let wrapped = fusiform_module::route::serve_tool_call(
        &f.store,
        br#"{"name": "catalog.get", "arguments": {}}"#,
    )
    .expect("a well-formed tool call must be served");
    assert!(wrapped.model_count() > 10);

    // Arguments inside the envelope are honoured, so the unwrap is real rather
    // than the envelope being discarded.
    let filtered = fusiform_module::route::serve_tool_call(
        &f.store,
        br#"{"name": "catalog.get", "arguments": {"fact_prefixes": ["rate."]}}"#,
    )
    .expect("arguments must pass through");
    assert!(filtered.fact_count() < wrapped.fact_count());
    for facts in filtered.models.values() {
        for key in facts.keys() {
            assert!(key.starts_with("rate."), "arguments were ignored: {key}");
        }
    }

    // A bare call with no arguments key at all.
    let bare = fusiform_module::route::serve_tool_call(&f.store, br#"{"name": "catalog.get"}"#)
        .expect("a call with no arguments is a call for everything");
    assert_eq!(bare.model_count(), wrapped.model_count());

    // A different tool name is refused rather than served anyway: a module that
    // answers to any name keeps answering after a consumer's typo.
    let err = fusiform_module::route::serve_tool_call(
        &f.store,
        br#"{"name": "catalog.list", "arguments": {}}"#,
    )
    .expect_err("an unknown tool must be refused");
    assert_eq!(err.code, "bad_request");
    assert!(err.message.contains("catalog.list"), "{}", err.message);

    // And a malformed argument inside a well-formed envelope still fails.
    let err = fusiform_module::route::serve_tool_call(
        &f.store,
        br#"{"name": "catalog.get", "arguments": {"fact_prefix": ["rate."]}}"#,
    )
    .expect_err("a misspelled argument must still be refused");
    assert_eq!(err.code, "bad_request");
}
