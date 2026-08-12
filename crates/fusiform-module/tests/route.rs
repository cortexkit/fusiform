//! The `catalog.get` contract, exercised without a socket.
//!
//! What is being tested is the request/response contract a consumer depends on:
//! that a malformed request is refused rather than answered with a default,
//! that a point-in-time request is honoured rather than silently served as
//! current, and that fact values arrive as values rather than as strings
//! containing values.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp, TokenClass};
use fusiform_module::route::{
    serve_catalog_get, serve_tool_call, CatalogGetResponse, ToolResponse,
};
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

/// Drive a full tool call and unwrap the catalog arm.
///
/// Panics on any other arm rather than returning a default, so a dispatch bug
/// that routes `catalog.get` to the wrong handler fails here instead of
/// producing an empty catalog that reads as a legitimate answer.
fn catalog_from_call(f: &Fixture, body: &[u8]) -> CatalogGetResponse {
    match fusiform_module::route::serve_tool_call(&f.store, body).expect("the call must be served")
    {
        fusiform_module::route::ToolResponse::Catalog(c) => c,
        other => panic!("expected a catalog response, got {other:?}"),
    }
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
    let issued = f
        .store
        .advance_catalog_version(obs, Timestamp(2_000))
        .unwrap();

    // The store derives the version, so the assertion is that the response
    // carries whatever was issued rather than a number computed here.
    assert!(issued > before);
    assert_eq!(get(&f, "{}").catalog_version, issued);
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

    let wrapped = catalog_from_call(&f, br#"{"name": "catalog.get", "arguments": {}}"#);
    assert!(wrapped.model_count() > 10);

    // Arguments inside the envelope are honoured, so the unwrap is real rather
    // than the envelope being discarded.
    let filtered = catalog_from_call(
        &f,
        br#"{"name": "catalog.get", "arguments": {"fact_prefixes": ["rate."]}}"#,
    );
    assert!(filtered.fact_count() < wrapped.fact_count());
    for facts in filtered.models.values() {
        for key in facts.keys() {
            assert!(key.starts_with("rate."), "arguments were ignored: {key}");
        }
    }

    // A bare call with no arguments key at all.
    let bare = catalog_from_call(&f, br#"{"name": "catalog.get"}"#);
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

/// `catalog.history` returns a fact's eras oldest first, with their windows.
#[test]
fn history_returns_eras_with_their_windows() {
    let f = fixture();

    // Reprice twice, so the history has a seed and two observed boundaries.
    for (at, rate) in [(3_000i64, 7.0f64), (5_000, 11.0)] {
        let mut doc: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        doc.get_mut("anthropic")
            .and_then(|p| p.get_mut("models"))
            .and_then(|m| m.get_mut("claude-sonnet-4-5"))
            .and_then(|m| m.get_mut("cost"))
            .and_then(|c| c.as_object_mut())
            .unwrap()
            .insert("input".to_string(), serde_json::json!(rate));

        let obs = f
            .store
            .record_observation(&NewObservation {
                source: SourceId::ModelsDev,
                observed_at: Timestamp(at - 1_000),
                outcome: ObservationOutcome::Unchanged,
                normalized_hash: Some(format!("h{at}")),
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
            Timestamp(at),
            BoundaryKind::Observed,
            Some(obs),
        )
        .unwrap();
        f.store.append_eras(&plan.eras).unwrap();
    }

    let history = match fusiform_module::route::serve_tool_call(
        &f.store,
        br#"{"name": "catalog.history", "arguments": {
            "provider_id": "anthropic",
            "model_id": "claude-sonnet-4-5",
            "fact_key": "rate.input"
        }}"#,
    )
    .expect("history must be served")
    {
        fusiform_module::route::ToolResponse::History(h) => h,
        other => panic!("expected a history response, got {other:?}"),
    };

    assert_eq!(history.eras.len(), 3, "seed plus two reprices");

    // Oldest first: a history is read forwards.
    let boundaries: Vec<i64> = history.eras.iter().map(|e| e.boundary_at_ms).collect();
    assert_eq!(boundaries, vec![1_000, 3_000, 5_000]);

    // The seed carries no window; both observed boundaries do.
    assert_eq!(history.eras[0].boundary_kind, "seed");
    assert_eq!(
        history.eras[0].window_from_ms, None,
        "a seed is not an observation and must claim no window"
    );
    assert_eq!(history.eras[1].boundary_kind, "observed");
    assert_eq!(history.eras[1].window_from_ms, Some(2_000));
    assert_eq!(history.eras[2].window_from_ms, Some(4_000));

    // And the values are the ones that were in force.
    let units = |i: usize| {
        history.eras[i]
            .value
            .get("units")
            .and_then(|u| u.as_i64())
            .unwrap()
    };
    assert_eq!(units(0), 3_000_000_000);
    assert_eq!(units(1), 7_000_000_000);
    assert_eq!(units(2), 11_000_000_000);
}

/// `catalog.status` reports polls that changed nothing, not only changes.
///
/// The distinction an operator needs: a source returning 304 for a week and a
/// source failing for a week look identical from the catalog alone.
#[test]
fn status_reports_quiet_polls_and_failures() {
    let f = fixture();

    for (at, outcome) in [
        (2_000i64, ObservationOutcome::NotModified),
        (
            3_000,
            ObservationOutcome::Failed {
                class: fusiform_core::FailureClass::Network,
            },
        ),
        (4_000, ObservationOutcome::NotModified),
    ] {
        f.store
            .record_observation(&NewObservation {
                source: SourceId::ModelsDev,
                observed_at: Timestamp(at),
                outcome,
                normalized_hash: None,
                raw_hash: None,
                etag: None,
                duration_ms: Some(12),
                detail: Some("probe".to_string()),
            })
            .unwrap();
    }

    let status = match fusiform_module::route::serve_tool_call(
        &f.store,
        br#"{"name": "catalog.status", "arguments": {}}"#,
    )
    .expect("status must be served")
    {
        fusiform_module::route::ToolResponse::Status(s) => s,
        other => panic!("expected a status response, got {other:?}"),
    };

    assert_eq!(status.recent_polls.len(), 3);
    // Newest first.
    assert_eq!(status.recent_polls[0].observed_at_ms, 4_000);
    assert_eq!(status.recent_polls[0].outcome, "not_modified");

    // The failure is present with its class, rather than being filtered out as
    // "not a real poll".
    let failure = status
        .recent_polls
        .iter()
        .find(|p| p.outcome == "failed")
        .expect("a failed poll must be reported");
    assert_eq!(failure.failure_class.as_deref(), Some("network"));

    // Counts come from the same read a consumer gets, so status and catalog.get
    // cannot disagree about how many models exist.
    let catalog = get(&f, "{}");
    assert_eq!(status.model_count, catalog.model_count());
    assert!(status.era_count > status.model_count as i64);
}

/// An unknown tool is refused, and the error names what is served.
#[test]
fn an_unknown_tool_is_refused() {
    let f = fixture();

    let err = fusiform_module::route::serve_tool_call(
        &f.store,
        br#"{"name": "catalog.list", "arguments": {}}"#,
    )
    .expect_err("an unknown tool must be refused");
    assert_eq!(err.code, "bad_request");
    assert!(err.message.contains("catalog.list"), "{}", err.message);
    // The error lists every tool that IS served, so an operator can pick the
    // right one without going to read the module's code.
    for tool in fusiform_module::route::TOOLS {
        assert!(
            err.message.contains(tool),
            "the refusal must name {tool}: {}",
            err.message
        );
    }
}

/// The manifest and the dispatch table describe the same tools.
///
/// Two lists of tool names in different files, with nothing comparing them: a
/// tool declared in the manifest and not dispatched is advertised and broken,
/// and a tool dispatched and not declared is unreachable. Neither shows up as a
/// failure anywhere else, because each file is internally consistent.
#[test]
fn the_manifest_and_the_dispatch_agree_on_which_tools_exist() {
    let manifest = fusiform_module::manifest();

    let declared: Vec<String> = manifest
        .provides
        .iter()
        .flat_map(|role| match role {
            subc_protocol::manifest::ProviderRole::ToolProvider { tools, .. } => {
                tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>()
            }
            _ => Vec::new(),
        })
        .collect();

    let dispatched: Vec<String> = fusiform_module::route::TOOLS
        .iter()
        .map(|t| t.to_string())
        .collect();

    assert!(!declared.is_empty(), "the manifest must declare tools");
    assert_eq!(
        declared, dispatched,
        "the manifest declares {declared:?} but the dispatch handles {dispatched:?}"
    );

    // And every declared tool actually answers rather than falling through to
    // the unknown-tool arm. A name present in both lists still proves nothing
    // if the match arm is missing.
    let f = fixture();
    for tool in &declared {
        let args = match tool.as_str() {
            "catalog.history" => serde_json::json!({
                "provider_id": "anthropic",
                "model_id": "claude-sonnet-4-5",
                "fact_key": "rate.input"
            }),
            _ => serde_json::json!({}),
        };
        let body = serde_json::to_vec(&serde_json::json!({
            "name": tool,
            "arguments": args
        }))
        .unwrap();

        let result = fusiform_module::route::serve_tool_call(&f.store, &body);
        match result {
            Ok(_) => {}
            Err(e) => panic!(
                "declared tool {tool} is not served: {} {}",
                e.code, e.message
            ),
        }
    }
}

/// A withheld fact is NAMED on the wire, not silently missing.
///
/// The bulk read omits a fact whose record is known bad. Over the wire that
/// omission is indistinguishable from a fact the upstream never published — a
/// consumer pricing a model reads "no input rate" and cannot tell it from "the
/// input rate we recorded is wrong". The second demands a different action and
/// is the one that costs money.
#[test]
fn a_corrected_fact_is_named_as_withheld_on_the_wire() {
    use fusiform_core::{Correction, FieldId};

    let f = fixture();

    // A correction covering the instant the read asks about.
    f.store
        .append_eras(&[fusiform_store::NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: fusiform_store::FactKey::rate(TokenClass::Input),
            value_json: r#"{"units":3000000000}"#.into(),
            boundary_at: Timestamp(8_000),
            boundary_kind: BoundaryKind::Corrected(Correction {
                fields: vec![FieldId::Rate {
                    class: TokenClass::Input,
                }],
                affected_from: Timestamp(2_000),
                affected_until: Timestamp(8_000),
                reason: "normalizer scaled the rate wrong".to_string(),
            }),
            observation_id: None,
        }])
        .unwrap();

    let response = get(&f, r#"{"at_ms": 5000}"#);

    // The fact is not in the models map...
    let sonnet = response
        .models
        .iter()
        .find(|(k, _)| k.contains("claude-sonnet-4-5"))
        .map(|(_, v)| v);
    if let Some(facts) = sonnet {
        assert!(
            !facts.contains_key("rate.input"),
            "a corrected fact must not be served as if it were good"
        );
    }

    // ...and the response says so, with the reason.
    assert_eq!(
        response.withheld.len(),
        1,
        "the withholding must be reported, not silent"
    );
    let w = &response.withheld[0];
    assert_eq!(w.model, "anthropic/claude-sonnet-4-5");
    assert_eq!(w.fact_key, "rate.input");
    assert_eq!(w.corrections.len(), 1);
    assert!(
        w.corrections[0].reason.contains("normalizer"),
        "the reason must travel: {:?}",
        w.corrections[0].reason
    );
    assert_eq!(w.corrections[0].affected_from_ms, 2_000);
    assert_eq!(w.corrections[0].affected_until_ms, 8_000);

    // And a read outside the interval is unaffected and reports nothing.
    let clean = get(&f, r#"{"at_ms": 9000}"#);
    assert!(
        clean.withheld.is_empty(),
        "a read outside every corrected interval withholds nothing"
    );
}

/// `catalog.history` carries a correction's extent and reason.
///
/// Reporting `kind: "corrected"` alone records a cause without surfacing it: an
/// operator learns something was wrong and not which interval or why.
#[test]
fn history_carries_the_correction_extent() {
    use fusiform_core::{Correction, FieldId};

    let f = fixture();
    f.store
        .append_eras(&[fusiform_store::NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: fusiform_store::FactKey::rate(TokenClass::Input),
            value_json: r#"{"units":3000000000}"#.into(),
            boundary_at: Timestamp(8_000),
            boundary_kind: BoundaryKind::Corrected(Correction {
                fields: vec![FieldId::Rate {
                    class: TokenClass::Input,
                }],
                affected_from: Timestamp(2_000),
                affected_until: Timestamp(8_000),
                reason: "normalizer scaled the rate wrong".to_string(),
            }),
            observation_id: None,
        }])
        .unwrap();

    let response = match serve_tool_call(
        &f.store,
        br#"{"name":"catalog.history","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","fact_key":"rate.input"}}"#,
    )
    .expect("history must serve")
    {
        ToolResponse::History(h) => h,
        other => panic!("expected history, got {other:?}"),
    };

    let corrected = response
        .eras
        .iter()
        .find(|e| e.boundary_kind == "corrected")
        .expect("the correction must appear in history");
    let detail = corrected
        .correction
        .as_ref()
        .expect("a corrected era must carry its extent");
    assert_eq!(detail.affected_from_ms, 2_000);
    assert_eq!(detail.affected_until_ms, 8_000);
    assert!(detail.reason.contains("normalizer"));

    // A non-corrected era carries no correction block, so its absence means
    // something rather than being the default everywhere.
    let seeded = response
        .eras
        .iter()
        .find(|e| e.boundary_kind != "corrected")
        .expect("the fixture has ordinary eras too");
    assert!(seeded.correction.is_none());
}

/// The SINGLE-MODEL path withholds and reports too.
///
/// `catalog.get` with a provider and model resolves its own rows through a
/// different store method than the bulk read, so it decides about corrections
/// separately. It got the decision wrong until a mutation showed no test
/// covered it — and this is the path the most common operator command uses.
#[test]
fn the_single_model_read_also_names_a_withheld_fact() {
    use fusiform_core::{Correction, FieldId};

    let f = fixture();
    f.store
        .append_eras(&[fusiform_store::NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: fusiform_store::FactKey::rate(TokenClass::Input),
            value_json: r#"{"units":3000000000}"#.into(),
            boundary_at: Timestamp(8_000),
            boundary_kind: BoundaryKind::Corrected(Correction {
                fields: vec![FieldId::Rate {
                    class: TokenClass::Input,
                }],
                affected_from: Timestamp(2_000),
                affected_until: Timestamp(8_000),
                reason: "normalizer scaled the rate wrong".to_string(),
            }),
            observation_id: None,
        }])
        .unwrap();

    let response = get(
        &f,
        r#"{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","at_ms":5000}"#,
    );

    let facts = response
        .models
        .get("anthropic/claude-sonnet-4-5")
        .expect("the model is present");
    assert!(
        !facts.contains_key("rate.input"),
        "the single-model read must not serve a known-bad fact"
    );
    assert!(
        facts.contains_key("rate.output"),
        "other facts are unaffected, so this is not an empty answer"
    );

    assert_eq!(
        response.withheld.len(),
        1,
        "the single-model read must report what it withheld"
    );
    assert_eq!(response.withheld[0].fact_key, "rate.input");
    assert!(response.withheld[0].corrections[0]
        .reason
        .contains("normalizer"));
}

/// A model whose every fact is withheld is still discoverable, through the
/// withheld list rather than through `models`.
///
/// It does NOT appear in `models`: an entry with no facts is noise, and the
/// route drops it. What matters is that the model's identity and the reason
/// survive somewhere in the response, so a consumer can tell "fusiform's record
/// of this model is known bad" from "no such model".
///
/// The distinction was found by a mutation that survived. Making `read_model`
/// return `None` when every fact is withheld changed nothing observable, which
/// is correct — the route already drops a factless model — but this test was
/// named `..._is_not_absent` and asserted no such thing. The name claimed a
/// property the code does not implement and the test never checked.
#[test]
fn a_model_whose_facts_are_all_withheld_is_still_discoverable() {
    use fusiform_core::{Correction, FieldId};

    let f = fixture();

    // Correct every fact this model has.
    let model = f
        .store
        .read_model(
            SourceId::ModelsDev,
            "anthropic",
            "claude-sonnet-4-5",
            Some(Timestamp(5_000)),
        )
        .unwrap()
        .0
        .expect("the model exists");

    // One defect touching every fact, so each row carries the WHOLE extent and
    // each therefore names its own fact.
    //
    // An earlier version declared `Rate{Input}` as the extent on every row,
    // including the capability and limit rows. That is incoherent — a
    // correction on `capability.attachment` telling a consumer to partition on
    // an input rate — and nothing rejected it until the store learned to. The
    // guard caught this test, which is the right way round.
    let all_fields: Vec<FieldId> = every_field_id()
        .into_iter()
        .filter(|f| {
            fusiform_store::FactKey::for_field(f.clone())
                .is_some_and(|k| model.facts.contains_key(&k))
        })
        .collect();

    let eras: Vec<_> = model
        .facts
        .keys()
        .map(|k| fusiform_store::NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: k.clone(),
            value_json: "null".into(),
            boundary_at: Timestamp(8_000),
            boundary_kind: BoundaryKind::Corrected(Correction {
                fields: all_fields.clone(),
                affected_from: Timestamp(2_000),
                affected_until: Timestamp(8_000),
                reason: "a defect affecting every fact of this model".to_string(),
            }),
            observation_id: None,
        })
        .collect();
    let count = eras.len();
    f.store.append_eras(&eras).unwrap();

    let response = get(
        &f,
        r#"{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","at_ms":5000,"include_retired":true}"#,
    );

    assert_eq!(
        response.withheld.len(),
        count,
        "every withheld fact must be named"
    );
    // The model's identity survives in the withheld list, which is the only
    // place it can be found — `models` does not carry a factless entry.
    assert!(response
        .withheld
        .iter()
        .all(|w| w.model == "anthropic/claude-sonnet-4-5"));
    assert!(
        !response.models.contains_key("anthropic/claude-sonnet-4-5"),
        "a model with no readable facts is not carried as an empty entry"
    );

    // And the response is distinguishable from "no such model", which would
    // have an empty withheld list.
    let no_such = get(
        &f,
        r#"{"provider_id":"anthropic","model_id":"claude-imaginary-9","at_ms":5000}"#,
    );
    assert!(no_such.models.is_empty());
    assert!(
        no_such.withheld.is_empty(),
        "a model that never existed withholds nothing; that is what makes the \
         two cases distinguishable"
    );
}

/// Every `FieldId` a correction can name.
///
/// Written out rather than derived, because deriving it from the same mapping
/// the code under test uses would make any test built on it agree with a broken
/// mapping. `FactKey::for_field` is exhaustive at the type level, so a new
/// variant fails to compile there and this list is what has to follow.
fn every_field_id() -> Vec<fusiform_core::FieldId> {
    use fusiform_core::{CapabilityId, FieldId, LimitId};
    vec![
        FieldId::Existence,
        FieldId::Rate {
            class: TokenClass::Input,
        },
        FieldId::Rate {
            class: TokenClass::Output,
        },
        FieldId::Rate {
            class: TokenClass::CacheRead,
        },
        FieldId::Rate {
            class: TokenClass::CacheWrite,
        },
        FieldId::Rate {
            class: TokenClass::Reasoning,
        },
        FieldId::Limit {
            limit: LimitId::Context,
        },
        FieldId::Limit {
            limit: LimitId::Output,
        },
        FieldId::Capability {
            capability: CapabilityId::Reasoning,
        },
        FieldId::Capability {
            capability: CapabilityId::ToolCall,
        },
        FieldId::Capability {
            capability: CapabilityId::Attachment,
        },
        FieldId::Capability {
            capability: CapabilityId::InputModalities,
        },
        FieldId::Capability {
            capability: CapabilityId::OutputModalities,
        },
    ]
}
