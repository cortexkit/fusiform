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

    // The TYPE is what this test is about, so the assertion is on the type.
    //
    // It used to assert the value was 1,000,000, which was incidental to the
    // test's subject and became wrong the day fusiform started correcting this
    // model's context limit to Anthropic's documented 200k. A value pinned in
    // passing inside a test about something else is a claim nobody decided to
    // make, and it fails for a reason unrelated to what the test is named for.
    let limit = sonnet.get("limit.context").expect("a context limit");
    assert!(
        limit.is_number(),
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

    // A name fusiform has never recorded is a refusal.
    //
    // THIS REVERSES AN EARLIER DECISION, recorded here because the old reason
    // was not wrong so much as answering a different question. It said: "no
    // such model is a legitimate answer to a question about a catalog." True
    // of a QUERY — asking whether the catalog contains X, empty is the answer.
    //
    // What changed is noticing fusiform holds a distinction it was discarding.
    // It knows three states, not two: never recorded this name, know it and it
    // is retired, know it and it is present. Collapsing the first two into "0
    // models" is the same defect as collapsing a published zero into absence —
    // an unknown and a real-but-empty answer rendered identically.
    //
    // The terminal is what settled it. `ck models get --provider notaprovider`
    // answered "0 models", and at a terminal a typo is far more likely than a
    // provider whose every model is retired.
    match serve_catalog_get(
        &f.store,
        br#"{"provider_id": "anthropic", "model_id": "no-such-model"}"#,
    ) {
        Err(e) => {
            assert_eq!(e.code, fusiform_protocol::CODE_NO_COVERAGE);
            assert!(
                !fusiform_protocol::RefusalKind::of(e.code).is_retryable(),
                "a coverage refusal must not invite a retry"
            );
        }
        Ok(r) => panic!("an unrecorded model must be refused, got {r:?}"),
    }
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
    //
    // Arguments per tool rather than `{}` for all: `catalog.correct` REQUIRES a
    // named model, deliberately, so a bare call is correctly refused. An
    // earlier version of this loop sent `{}` to everything and would have
    // reported a correctly-refusing write tool as unserved — the test's method
    // assumed every tool is callable with no arguments, which stopped being
    // true the moment one of them could change the store.
    let f = fixture();
    for tool in &declared {
        let args = match tool.as_str() {
            "catalog.history" => serde_json::json!({
                "provider_id": "anthropic",
                "model_id": "claude-sonnet-4-5",
                "fact_key": "rate.input"
            }),
            "catalog.correct" => serde_json::json!({
                "provider_id": "anthropic",
                "model_id": "claude-sonnet-4-5",
                "fields": [{"field": "rate", "class": "input"}],
                "affected_from_ms": 1,
                "affected_until_ms": 2,
                "reason": "reachability check"
                // dry_run defaults to true, so this proves the tool is reached
                // without writing anything.
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
            // A `refused` code means the tool WAS reached and declined on the
            // merits, which is what this loop is checking for. Only a
            // bad_request or an unknown tool means it was not served.
            Err(e) if e.code == "refused" => {}
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

    // And the response is distinguishable from "no such model" — which is now
    // a REFUSAL rather than an empty answer.
    //
    // This assertion used to check that an unknown model returned an empty
    // withheld list, which distinguished the two cases only if a reader
    // compared two fields. Driving the CLI showed the weaker half in practice:
    // an unknown model answered "0 models", identical to a real model with
    // nothing to show. The distinction is now carried by the response KIND.
    match serve_catalog_get(
        &f.store,
        br#"{"provider_id":"anthropic","model_id":"claude-imaginary-9","at_ms":5000}"#,
    ) {
        Err(e) => assert!(
            e.message.contains("claude-imaginary-9"),
            "the refusal must name the model, got {:?}",
            e.message
        ),
        Ok(r) => panic!("a model that never existed must be refused, got {r:?}"),
    }
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

/// `catalog.status` reports what each poll changed, not just that it changed.
///
/// The composition already existed in two places that an operator cannot
/// reach: computed on the ingest plan, and logged to the daemon's shared
/// stderr — which on this machine is a 1.17-million-line file where fusiform
/// holds 31 lines. That is the recording-versus-surfacing defect: a diagnostic
/// is only real if something read routinely can get to it, and `ck models
/// status` is what an operator reads.
///
/// Derived from the eras rather than stored, so it is correct for polls written
/// before this existed.
#[test]
fn status_reports_what_each_poll_changed() {
    let f = fixture();

    // An earlier confirming observation, so the eras below can state a real
    // window. The store refuses an observed boundary without one, which is the
    // guard working: an era claiming a window it cannot bound is exactly what
    // the observation/era split exists to prevent.
    f.store
        .record_observation(&fusiform_store::NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1_000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 0 },
            normalized_hash: Some("h1".into()),
            raw_hash: None,
            etag: None,
            duration_ms: Some(10),
            detail: None,
        })
        .unwrap();

    // A poll that adds a model and changes a fact on one already known.
    let obs = f
        .store
        .record_observation(&fusiform_store::NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(5_000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some("h5".into()),
            raw_hash: None,
            etag: None,
            duration_ms: Some(42),
            detail: None,
        })
        .unwrap();

    let era = |provider: &str, model: &str, fact: &str, value: &str| fusiform_store::NewEra {
        source: SourceId::ModelsDev,
        provider_id: provider.into(),
        model_id: model.into(),
        fact_key: fusiform_store::FactKey::from_stored(fact.to_string()),
        value_json: value.to_string(),
        boundary_at: Timestamp(5_000),
        boundary_kind: BoundaryKind::Observed,
        observation_id: Some(obs),
    };

    f.store
        .append_eras(&[
            // TWO models arriving, each writing its existence plus its facts.
            //
            // Asymmetric against the one withdrawal on purpose. The first
            // version of this test had one of each, and a mutation swapping the
            // two columns survived — exactly the weakness fixed in the tick
            // test ninety minutes earlier, repeated here in a new fixture.
            // Knowing the failure mode did not prevent it; the mutation did.
            era("mistral", "large", "existence", "\"present\""),
            era("mistral", "large", "rate.input", "1"),
            era("mistral", "large", "rate.output", "2"),
            era("google", "gemini", "existence", "\"present\""),
            era("google", "gemini", "rate.input", "5"),
            // A model leaving.
            era("openai", "gpt-5", "existence", "\"absent\""),
            // And a genuine change on a model already known.
            era("anthropic", "claude-sonnet-4-5", "rate.input", "9"),
        ])
        .unwrap();

    let response =
        match serve_tool_call(&f.store, br#"{"name":"catalog.status","arguments":{}}"#).unwrap() {
            ToolResponse::Status(s) => s,
            other => panic!("expected a status response, got {other:?}"),
        };

    let poll = response
        .recent_polls
        .iter()
        .find(|p| p.observed_at_ms == 5_000)
        .expect("the poll must be reported");
    let changes = poll
        .changes
        .as_ref()
        .expect("a poll that wrote eras must report what it changed");

    assert_eq!(changes.eras, 7);
    assert_eq!(changes.models_arrived, 2, "mistral and google arrived");
    assert_eq!(changes.models_withdrawn, 1, "gpt-5 was withdrawn");
    assert_eq!(
        changes.facts_changed, 1,
        "only anthropic's rate moved on a model already known — the arriving \
         model's two rate eras are part of its arrival, not revisions"
    );

    // The distinction is the whole point: the parts must not restate the total.
    assert!(
        changes.facts_changed < changes.eras,
        "an arriving model's facts must not be counted as changes"
    );
}

/// A poll that changed nothing reports no composition at all.
///
/// A 304, an unchanged document and a failure all write no eras, so the
/// absence is the ordinary case. Reporting zeroes would put four fields on
/// every ordinary poll to say nothing happened, which the outcome already says.
#[test]
fn a_poll_that_wrote_nothing_reports_no_changes() {
    let f = fixture();

    f.store
        .record_observation(&fusiform_store::NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(7_000),
            outcome: ObservationOutcome::NotModified,
            normalized_hash: None,
            raw_hash: None,
            etag: Some("etag-x".into()),
            duration_ms: Some(11),
            detail: None,
        })
        .unwrap();

    let response =
        match serve_tool_call(&f.store, br#"{"name":"catalog.status","arguments":{}}"#).unwrap() {
            ToolResponse::Status(s) => s,
            other => panic!("expected a status response, got {other:?}"),
        };

    let poll = response
        .recent_polls
        .iter()
        .find(|p| p.observed_at_ms == 7_000)
        .expect("the 304 must be reported");
    assert!(
        poll.changes.is_none(),
        "a poll that wrote nothing must not carry a composition: {:?}",
        poll.changes
    );
}

/// The model total and the priced total are different numbers, and status
/// reports both.
///
/// Measured on the live store before this existed: 420 of 6,293 present models
/// carry no rate at all, because the upstream publishes no cost object for
/// them. That is 6.7% of the catalog and the model total cannot express it —
/// an operator reading "6,293 models" has no way to know a fifteenth of them
/// cannot be priced.
///
/// The distinction matters because absent and free are different states, and
/// only one of them is safe to bill against.
#[test]
fn status_reports_how_many_models_can_be_priced() {
    let f = fixture();

    // The fixture is cut from real upstream bytes, so it already contains
    // models with no cost object — five of them. Measuring the DELTA rather
    // than an absolute: an absolute would assert a property of the fixture,
    // and the fixture changes whenever the upstream sample is refreshed.
    let before = fusiform_module::route::serve_status(&f.store, b"").unwrap();
    let unpriced_before = before.model_count - before.models_priced.unwrap();
    assert!(
        unpriced_before > 0,
        "the real upstream sample must already contain unpriced models, or this \
         test proves nothing about a state that occurs in production"
    );

    // A model with facts but no rate: exactly the shape the upstream produces
    // when it publishes no cost object.
    f.store
        .append_eras(&[
            fusiform_store::NewEra {
                source: SourceId::ModelsDev,
                provider_id: "someprovider".into(),
                model_id: "unpriced-model".into(),
                fact_key: fusiform_store::FactKey::existence(),
                value_json: "\"present\"".into(),
                boundary_at: Timestamp(1_000),
                boundary_kind: BoundaryKind::Seed,
                observation_id: None,
            },
            fusiform_store::NewEra {
                source: SourceId::ModelsDev,
                provider_id: "someprovider".into(),
                model_id: "unpriced-model".into(),
                fact_key: fusiform_store::FactKey::limit("context"),
                value_json: "200000".into(),
                boundary_at: Timestamp(1_000),
                boundary_kind: BoundaryKind::Seed,
                observation_id: None,
            },
        ])
        .unwrap();

    let after = fusiform_module::route::serve_status(&f.store, b"").unwrap();
    let unpriced_after = after.model_count - after.models_priced.unwrap();

    assert_eq!(
        after.model_count,
        before.model_count + 1,
        "the new model must be counted as a model"
    );
    assert_eq!(
        unpriced_after,
        unpriced_before + 1,
        "and must be counted as unpriced, not as priced"
    );
    assert_eq!(
        after.models_priced.unwrap(),
        before.models_priced.unwrap(),
        "a model with no rate must not change the priced count at all"
    );
}

/// A historical read carries its uncertainty onto the wire.
///
/// The store learned to mark a read inside a later era's observation window,
/// and that distinction is worthless if it stops at the store boundary — which
/// is exactly what happened for hours with corrections, where `read_model`
/// honoured none and the defect was invisible because the bulk path did.
///
/// So this asserts on the served response, and asserts BOTH read paths agree:
/// a consumer asking for one model and a consumer asking for the catalog must
/// get the same qualification on the same fact.
#[test]
fn a_historical_read_reports_uncertainty_on_the_wire() {
    let f = fixture();

    // A fact observed at 1_000, then changed at 9_000 with the previous
    // confirming observation at 2_000 — a poll gap from 2_000 to 9_000.
    f.store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(2_000),
            outcome: ObservationOutcome::Unchanged,
            normalized_hash: Some("h2".into()),
            raw_hash: None,
            etag: None,
            duration_ms: Some(5),
            detail: None,
        })
        .unwrap();
    let obs = f
        .store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(9_000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 9 },
            normalized_hash: Some("h9".into()),
            raw_hash: None,
            etag: None,
            duration_ms: Some(5),
            detail: None,
        })
        .unwrap();
    f.store
        .append_eras(&[fusiform_store::NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: fusiform_store::FactKey::rate(TokenClass::Input),
            value_json: r#"{"state":"priced","units":7000000000}"#.into(),
            boundary_at: Timestamp(9_000),
            boundary_kind: BoundaryKind::Observed,
            observation_id: Some(obs),
        }])
        .unwrap();

    // A read at 5_000 sits inside (2_000, 9_000].
    let bulk = get(&f, r#"{"at_ms": 5000}"#);
    let entry = bulk
        .uncertain
        .iter()
        .find(|u| u.fact_key == "rate.input")
        .expect("the bulk read must report the uncertainty");
    assert_eq!(entry.provider_id, "anthropic");
    assert_eq!(entry.model_id, "claude-sonnet-4-5");
    assert_eq!(entry.superseded_after_ms, 2_000);
    assert_eq!(entry.superseded_by_ms, 9_000);

    // The value is STILL SERVED. This qualifies rather than withholds.
    let facts = bulk
        .models
        .get("anthropic/claude-sonnet-4-5")
        .expect("the model is served");
    assert!(
        facts.contains_key("rate.input"),
        "an uncertain fact is qualified, not omitted"
    );

    // The single-model path must agree, fact for fact and bracket for bracket.
    let single = get(
        &f,
        r#"{"at_ms": 5000, "provider_id": "anthropic", "model_id": "claude-sonnet-4-5"}"#,
    );
    assert_eq!(
        single.uncertain, bulk.uncertain,
        "the two read paths must report the same uncertainty"
    );

    // And a read of the CURRENT catalog reports none, because the newest era
    // has no successor. If this ever fires, every consumer sees the field on
    // every response and stops reading it.
    let now = get(&f, "{}");
    assert!(
        now.uncertain.is_empty(),
        "a current read is never uncertain, got {:?}",
        now.uncertain
    );

    // THE ENDPOINTS, because the predicate exists twice.
    //
    // `value_at` computes it in Rust; the bulk read computes it in SQL. Two
    // implementations of one belief, and nothing forces them to agree -- the
    // shape that has produced a defect in this repository more than once. The
    // interior case above passes with either open or closed ends, so it cannot
    // tell them apart; a mutation widening the SQL to `<=` and `>=` survived
    // until these existed.
    //
    // Both instants are CERTAIN. At 2_000 fusiform confirmed the value; at
    // 9_000 it observed the new one, and the new era is in force from there.
    for at in [2_000i64, 9_000] {
        let edge = get(&f, &format!(r#"{{"at_ms": {at}}}"#));
        assert!(
            edge.uncertain.is_empty(),
            "t={at} is an observation instant and is certain, got {:?}",
            edge.uncertain
        );
    }
}

/// An unknown provider is refused, not answered with zero models.
///
/// Found by driving the CLI along paths I did not design: `ck models get
/// --provider notaprovider` returned "0 models", which is a true statement
/// about the filter and a misleading one about the catalog. A misspelled
/// provider and a real provider whose models are all retired produced the
/// identical answer, and at a terminal the typo is far more likely.
#[test]
fn an_unknown_provider_is_refused_rather_than_answered_with_zero() {
    let f = fixture();

    match serve_catalog_get(&f.store, br#"{"provider_id": "notaprovider"}"#) {
        Err(e) => {
            // `no_coverage`, not `bad_request`: the request was well formed
            // and fusiform has no catalog to answer it. A consumer must act on
            // those differently — one says fix your request, the other is an
            // ANSWER — and while they shared a code no consumer could tell.
            assert_eq!(e.code, fusiform_protocol::CODE_NO_COVERAGE);
            assert_ne!(
                e.code,
                fusiform_protocol::CODE_BAD_REQUEST,
                "a coverage refusal must not read as a caller defect: that \
                 sends someone to debug a request that was correct"
            );
            assert!(
                !fusiform_protocol::RefusalKind::of(e.code).is_retryable(),
                "a coverage refusal must not invite a retry: the catalog will \
                 not have the model next time either"
            );
            assert!(
                e.message.contains("notaprovider"),
                "the refusal must name what was not found, got {:?}",
                e.message
            );
        }
        Ok(r) => panic!(
            "an unknown provider must be refused, got {} models",
            r.model_count()
        ),
    }

    // The control: a REAL provider still answers, so the refusal is the
    // unknown-ness rather than the filter being broken.
    let ok = get(&f, r#"{"provider_id": "anthropic"}"#);
    assert!(ok.model_count() > 0, "a known provider must still answer");
}

/// An unknown model under a KNOWN provider is refused, and says so distinctly.
///
/// Two refusals rather than one because the operator's next action differs: a
/// wrong provider means the whole id is wrong, a wrong model under a real
/// provider usually means a version suffix.
#[test]
fn an_unknown_model_names_the_provider_as_known() {
    let f = fixture();

    match serve_catalog_get(
        &f.store,
        br#"{"provider_id": "anthropic", "model_id": "no-such-model"}"#,
    ) {
        Err(e) => {
            // `no_coverage`, not `bad_request`: the request was well formed
            // and fusiform has no catalog to answer it. A consumer must act on
            // those differently — one says fix your request, the other is an
            // ANSWER — and while they shared a code no consumer could tell.
            assert_eq!(e.code, fusiform_protocol::CODE_NO_COVERAGE);
            assert_ne!(
                e.code,
                fusiform_protocol::CODE_BAD_REQUEST,
                "a coverage refusal must not read as a caller defect: that \
                 sends someone to debug a request that was correct"
            );
            assert!(
                !fusiform_protocol::RefusalKind::of(e.code).is_retryable(),
                "a coverage refusal must not invite a retry: the catalog will \
                 not have the model next time either"
            );
            assert!(
                e.message.contains("no-such-model"),
                "must name the model, got {:?}",
                e.message
            );
            assert!(
                e.message.contains("provider exists"),
                "must distinguish this from an unknown provider, got {:?}",
                e.message
            );
        }
        Ok(r) => panic!("an unknown model must be refused, got {r:?}"),
    }
}

/// A model fusiform KNOWS but that is absent right now is an empty answer, not
/// an error.
///
/// This is the line the two refusals must not cross. A withdrawn model is a
/// real answer about a real model — "the upstream stopped publishing it" — and
/// turning that into an error would make a withdrawal indistinguishable from a
/// typo, which is the defect above with the sign flipped.
#[test]
fn a_known_but_retired_model_answers_rather_than_refusing() {
    let f = fixture();

    // A confirming observation, so the withdrawal below can state a window.
    // The schema requires it: an observed boundary with no prior confirming
    // observation cannot say when the change happened.
    f.store
        .record_observation(&fusiform_store::NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(5_000),
            outcome: fusiform_core::ObservationOutcome::Unchanged,
            normalized_hash: Some("h".into()),
            raw_hash: Some("r".into()),
            etag: None,
            duration_ms: Some(10),
            detail: None,
        })
        .unwrap();

    // Withdraw it: a later era says absent.
    f.store
        .append_eras(&[fusiform_store::NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: fusiform_store::FactKey::existence(),
            value_json: "\"absent\"".into(),
            boundary_at: Timestamp(9_000),
            boundary_kind: BoundaryKind::Observed,
            observation_id: None,
        }])
        .unwrap();

    let r = get(
        &f,
        r#"{"provider_id": "anthropic", "model_id": "claude-sonnet-4-5"}"#,
    );
    assert_eq!(
        r.model_count(),
        0,
        "a retired model is absent from a present-only read"
    );

    // And including retired brings it back, proving the zero above is presence
    // rather than the model being unknown.
    let with_retired = get(
        &f,
        r#"{"provider_id": "anthropic", "model_id": "claude-sonnet-4-5", "include_retired": true}"#,
    );
    assert_eq!(with_retired.model_count(), 1);
}

/// A known model read BEFORE it existed is an empty answer, not "unknown".
///
/// The case a surviving mutation named. `read_model` finds nothing at an
/// instant earlier than the model's first era, which looks identical to a
/// misspelling from inside the function — and the two need opposite answers.
///
/// "This model did not exist on the 3rd" is a real historical fact and one of
/// the questions point-in-time reads exist to answer. Refusing it would make a
/// correct answer about the past indistinguishable from a typo, which is the
/// defect this refusal was added to fix, with the sign flipped.
#[test]
fn a_known_model_before_it_existed_answers_rather_than_refusing() {
    let f = fixture();

    // The fixture seeds at t=1000, so t=500 predates every era.
    let before = get(
        &f,
        r#"{"provider_id": "anthropic", "model_id": "claude-sonnet-4-5", "at_ms": 500}"#,
    );
    assert_eq!(
        before.model_count(),
        0,
        "a model that did not exist yet is absent, and that is an answer"
    );

    // The control: the same model at an instant it DOES exist, so the zero
    // above is the instant rather than the model being unreadable.
    let after = get(
        &f,
        r#"{"provider_id": "anthropic", "model_id": "claude-sonnet-4-5", "at_ms": 2000}"#,
    );
    assert_eq!(after.model_count(), 1);
}

/// Every source the route ACCEPTS must be one a row can carry.
///
/// # The defect this closes
///
/// `parse_source` accepted `"seed"` and returned `SourceId::Seed`. Nothing has
/// ever written a row under that source — the embedded snapshot is models.dev
/// data fetched earlier, so it is stored under `ModelsDev` and marked with
/// `BoundaryKind::Seed`. Every query for `"seed"` therefore matched zero rows
/// and returned an EMPTY CATALOG with a success status.
///
/// A consumer reading that gets "fusiform knows of no models", which is the
/// exact confusion this store is built to prevent: absence must never be
/// indistinguishable from never-published. Verified against the live store on
/// 2026-08-15 — 73,584 eras, not one under `seed`.
///
/// The CLI never sends `--source`, so only a direct wire consumer could reach
/// it. That is BROCA, ASTRO and MC.
///
/// # Why this is a loop over accepted names rather than a test of one string
///
/// A second source will arrive — the charter says so — and the failure is not
/// "seed is wrong", it is "a name is accepted before anything writes it".
/// This asserts the property for every name the route takes.
#[test]
fn every_accepted_source_can_match_a_row() {
    // The names a wire caller can send. Anything not here must be refused.
    let accepted = ["models.dev"];

    let f = fixture();

    for name in accepted {
        let body = format!(r#"{{"name":"catalog.get","arguments":{{"source":"{name}"}}}}"#);
        let response = catalog_from_call(&f, body.as_bytes());
        assert!(
            !response.models.is_empty(),
            "source {name:?} is accepted by the route and matches no rows, so \
             every query for it returns an empty catalog with a success status \
             — indistinguishable from a catalog that has nothing in it"
        );
    }

    // And a source no row carries must be REFUSED, not served empty.
    for rejected in ["seed", "models.dev.old", "openrouter"] {
        let body = format!(r#"{{"name":"catalog.get","arguments":{{"source":"{rejected}"}}}}"#);
        let err = match fusiform_module::route::serve_tool_call(&f.store, body.as_bytes()) {
            Err(e) => e,
            Ok(other) => panic!(
                "source {rejected:?} must be refused; no row carries it, so any \
                 answer is an empty catalog that reads as a real one. Got: {other:?}"
            ),
        };
        assert!(
            format!("{err:?}").contains("unknown source"),
            "the refusal must name the problem, got: {err:?}"
        );
    }
}

/// An empty history says WHICH of its three causes applies.
///
/// # The defect
///
/// `catalog.history` returned an empty era list for a wrong provider, a wrong
/// model, and a real model with nothing recorded for the fact — three
/// situations needing three different actions. The CLI rendered one sentence
/// for all of them, and its comment named a typo in the KEY as the cause, so
/// an operator who mistyped the MODEL was told to check the key.
///
/// `catalog.get` has separated these since the route refusals landed; history
/// did not, and the two verbs sit next to each other in the same help output.
///
/// # Why the CLI was not the place to fix it
///
/// The distinction needs two store lookups the CLI cannot make. Fixing it at
/// the renderer would have given the operator a better sentence and left every
/// other consumer with the same collapsed answer.
#[test]
fn an_empty_history_names_which_cause_applies() {
    let f = fixture();

    let history = |provider: &str, model: &str, fact: &str| {
        let body = format!(
            r#"{{"name":"catalog.history","arguments":{{"provider_id":"{provider}","model_id":"{model}","fact_key":"{fact}"}}}}"#
        );
        fusiform_module::route::serve_tool_call(&f.store, body.as_bytes())
    };

    // A provider fusiform has never recorded.
    let err = format!(
        "{:?}",
        history("notaprovider", "x", "limit.context").expect_err("must refuse")
    );
    assert!(
        err.contains("unknown provider"),
        "a wrong provider must be named as such, not answered with an empty \
         history: {err}"
    );

    // A real provider, a model it does not have.
    let err = format!(
        "{:?}",
        history("anthropic", "claude-sonnet-9-9", "limit.context").expect_err("must refuse")
    );
    assert!(
        err.contains("unknown model") && err.contains("version suffix"),
        "a wrong model under a real provider must say the provider exists and \
         point at the id, which is the actionable half: {err}"
    );

    // A real model, a fact key with nothing recorded. This is a REAL ANSWER
    // and must stay one — refusing here would make a fact that genuinely has
    // no history indistinguishable from a typo.
    let ok = history("anthropic", "claude-sonnet-4-5", "limit.nonsense")
        .expect("a known model with an unrecorded fact must answer, not refuse");
    let value = serde_json::to_value(match ok {
        fusiform_module::route::ToolResponse::History(h) => h,
        other => panic!("expected history, got {other:?}"),
    })
    .unwrap();
    assert!(
        value["eras"].as_array().unwrap().is_empty(),
        "and its era list must be empty: {value}"
    );

    // CONTROL: a real fact still returns its history, or the refusals above
    // could be a broken read path rather than a diagnosis.
    let ok =
        history("anthropic", "claude-sonnet-4-5", "limit.context").expect("the control must serve");
    let value = serde_json::to_value(match ok {
        fusiform_module::route::ToolResponse::History(h) => h,
        other => panic!("expected history, got {other:?}"),
    })
    .unwrap();
    assert!(
        !value["eras"].as_array().unwrap().is_empty(),
        "control: a recorded fact must return eras: {value}"
    );
}

/// The served `last_changed_at_ms` excludes artifact eras.
///
/// # Why the golden fixture cannot cover this
///
/// In that fixture the answer equals the newest boundary, so it passes just as
/// well against a route that ignores artifacts entirely. This drives the case
/// where the two differ: the newest era is fusiform rewriting its own
/// representation, and the true answer is the era before it.
///
/// That is the whole reason the field is served rather than derived by the
/// consumer — which polls were representation changes is not in the era list,
/// and a consumer taking the newest boundary reads an untouched fact as
/// freshly maintained.
#[test]
fn the_served_last_change_ignores_an_artifact_poll() {
    let f = fixture();

    // A genuine reprice, then a poll that rewrites the same value.
    let mut repriced: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    repriced
        .get_mut("anthropic")
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.get_mut("claude-sonnet-4-5"))
        .and_then(|m| m.get_mut("cost"))
        .and_then(|c| c.as_object_mut())
        .unwrap()
        .insert("input".to_string(), serde_json::json!(7.0));

    let mut artifact_obs = None;
    for (at, doc) in [(3_000i64, &repriced), (5_000, &repriced)] {
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
        let catalog = normalize_models_dev(&serde_json::to_vec(doc).unwrap())
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
        // The second pass produces no eras, since the value did not move. Write
        // one by hand, exactly as a representation change would: same claim,
        // new era, new observation.
        if at == 5_000 {
            artifact_obs = Some(obs);
            // The value is READ BACK from the store rather than written by
            // hand, so it is byte-for-byte the claim already in force. A
            // hand-typed literal that differs by a digit is a real change, and
            // the test would then be asserting nothing about artifacts.
            let held = f
                .store
                .fact_history(
                    SourceId::ModelsDev,
                    "anthropic",
                    "claude-sonnet-4-5",
                    &fusiform_store::FactKey::rate(fusiform_core::TokenClass::Input),
                )
                .unwrap()
                .last()
                .expect("the reprice era exists")
                .value_json
                .clone();
            f.store
                .append_eras(&[fusiform_store::NewEra {
                    source: SourceId::ModelsDev,
                    provider_id: "anthropic".into(),
                    model_id: "claude-sonnet-4-5".into(),
                    fact_key: fusiform_store::FactKey::rate(fusiform_core::TokenClass::Input),
                    value_json: held,
                    boundary_at: Timestamp(5_000),
                    boundary_kind: BoundaryKind::Observed,
                    observation_id: Some(obs),
                }])
                .unwrap();
        } else {
            f.store.append_eras(&plan.eras).unwrap();
        }
    }

    let ask = || match fusiform_module::route::serve_tool_call(
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

    // CONTROL: unmarked, the artifact is indistinguishable from a real change.
    // Without this the assertion below passes against a route that always
    // reports the era before the newest.
    assert_eq!(
        ask().last_changed_at_ms,
        Some(5_000),
        "control: an unmarked artifact reads as the last change"
    );

    f.store
        .mark_observation_artifact(
            artifact_obs.expect("the artifact poll was recorded"),
            "a representation change",
            Timestamp(9_000),
        )
        .unwrap();

    assert_eq!(
        ask().last_changed_at_ms,
        Some(3_000),
        "the served answer must skip the artifact era and report the genuine \
         reprice before it"
    );
}
