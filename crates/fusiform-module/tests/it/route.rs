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

const FIXTURE: &str = include_str!("../../../fusiform-core/fixtures/models-dev-excerpt.json");

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
    // So must a mode rate.
    let moded: usize = rates
        .models
        .values()
        .flat_map(|f| f.keys())
        .filter(|k| k.contains(".mode."))
        .count();
    assert!(moded > 0, "mode rates belong to the rate plane");
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
            // Same reason as the correction above: this one requires a named
            // observation, so a bare call is correctly refused rather than
            // unserved. The id is deliberately one this fixture does not have,
            // which reaches the handler and gets a no_coverage answer — proving
            // the route is wired without depending on the fixture's ids.
            "catalog.mark_artifact" => serde_json::json!({
                "observation_id": 999_999,
                "reason": "reachability check"
                // dry_run defaults to true, so this cannot write either.
            }),
            "catalog.retract_artifact" => serde_json::json!({
                "observation_id": 999_999,
                "reason": "reachability check"
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
            // A coverage answer also means the tool was REACHED: it read the
            // store, found no such row, and said so. Distinguishing this from
            // `bad_request` is exactly what the refusal contract added, and
            // this loop is the first consumer of that distinction inside the
            // repository.
            Err(e) if e.code == fusiform_protocol::CODE_NO_COVERAGE => {}
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
        // One representative: mode names are the upstream's and cannot be
        // enumerated, so the list names the variant rather than every key.
        FieldId::ModeRate {
            class: TokenClass::Input,
            mode: "fast".to_string(),
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
            capability: CapabilityId::ReasoningOptions,
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
        FieldId::Model {
            attribute: fusiform_core::ModelAttributeId::Family,
        },
        FieldId::Model {
            attribute: fusiform_core::ModelAttributeId::OpenWeights,
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

/// An unknown provider is refused with `no_coverage` on the single-model path.
///
/// Naming a model as well as a provider sends the read through the one-model
/// lookup instead of the provider filter tested above, and that lookup builds
/// its own unknown-provider refusal. This is the only test that reaches that
/// refusal directly; a mutation turning its code into `bad_request` is caught
/// here and nowhere else.
#[test]
fn an_unknown_provider_is_refused_on_the_single_model_path_too() {
    let f = fixture();

    match serve_catalog_get(
        &f.store,
        br#"{"provider_id": "notaprovider", "model_id": "claude-sonnet-4-5"}"#,
    ) {
        Err(e) => {
            assert_eq!(e.code, fusiform_protocol::CODE_NO_COVERAGE);
            assert!(
                e.message.contains("unknown provider") && e.message.contains("notaprovider"),
                "the refusal must say it is the provider that is unknown, got {:?}",
                e.message
            );
        }
        Ok(r) => panic!(
            "an unknown provider must be refused on the single-model path, got {} models",
            r.model_count()
        ),
    }

    // Control: the same model under a provider the store knows is answered,
    // so the refusal above is about the provider and not the model id.
    let ok = get(
        &f,
        r#"{"provider_id": "anthropic", "model_id": "claude-sonnet-4-5"}"#,
    );
    assert_eq!(
        ok.model_count(),
        1,
        "a known provider and model must answer"
    );
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

/// A preview reports the evidence and writes nothing; a commit writes.
///
/// # Why the preview matters more here than on a read
///
/// A mark excludes eras from every derivation that honours it and leaves
/// nothing behind to argue with — unlike a correction, which keeps the era it
/// partitions. So the operator's check is whether the id names the poll they
/// mean, and the preview exists to answer exactly that: the observed instant,
/// how many eras the poll wrote, and how many of those restate the claim
/// already in force.
///
/// The counts are served as two numbers rather than a ratio because the
/// DIFFERENCE is the finding. On the 2026-08-16 poll it is 17,454 of 17,455,
/// and the one era that is not a restatement is a genuine upstream change that
/// survives the mark.
#[test]
fn marking_previews_before_it_writes() {
    let f = fixture();

    let obs = f
        .store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(4_000),
            outcome: ObservationOutcome::Unchanged,
            normalized_hash: Some("h".into()),
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();

    let call = |dry: bool| {
        let body = serde_json::to_vec(&serde_json::json!({
            "name": "catalog.mark_artifact",
            "arguments": {
                "observation_id": obs,
                "reason": "docs/findings/2026-08-16-provenance-rewrote-the-rate-plane.md",
                "dry_run": dry
            }
        }))
        .unwrap();
        match fusiform_module::route::serve_tool_call(&f.store, &body) {
            Ok(fusiform_module::route::ToolResponse::MarkArtifact(m)) => m,
            other => panic!("expected a mark response, got {other:?}"),
        }
    };

    let preview = call(true);
    assert!(!preview.committed, "a preview must not write");
    assert_eq!(
        preview.observed_at_ms, 4_000,
        "the preview must name WHEN the poll was, since an id alone is not \
         something an operator can verify by reading it"
    );

    // CONTROL: the store really is unmarked after the preview. Without this the
    // `committed: false` assertion only proves what the response SAYS, not what
    // the store did — which is the gap between a report and a fact.
    assert_eq!(
        f.store.observation_artifact_reason(obs).unwrap(),
        None,
        "a preview must leave no mark in the store"
    );

    let committed = call(false);
    assert!(committed.committed, "an explicit dry_run: false must write");
    assert!(
        f.store
            .observation_artifact_reason(obs)
            .unwrap()
            .is_some_and(|r| r.contains("provenance")),
        "the commit must record the reason it was given, so a later reader \
         meets the evidence rather than a bare id"
    );
}

/// An id this store does not have is a coverage answer, not a caller defect.
#[test]
fn marking_an_unknown_observation_is_not_a_bad_request() {
    let f = fixture();
    let body = serde_json::to_vec(&serde_json::json!({
        "name": "catalog.mark_artifact",
        "arguments": {"observation_id": 999_999, "reason": "x"}
    }))
    .unwrap();

    let err = fusiform_module::route::serve_tool_call(&f.store, &body)
        .expect_err("an unknown observation must refuse");
    assert_eq!(
        err.code,
        fusiform_protocol::CODE_NO_COVERAGE,
        "the request was well-formed and this store has no such poll, which is \
         an answer rather than the operator's defect: reporting it as a client \
         error sends someone to debug a correct request"
    );

    // CONTROL: a genuinely malformed request still reads as one, or the
    // assertion above would pass against a route that never says bad_request.
    let malformed = serde_json::to_vec(&serde_json::json!({
        "name": "catalog.mark_artifact",
        "arguments": {"reason": "no id at all"}
    }))
    .unwrap();
    assert_eq!(
        fusiform_module::route::serve_tool_call(&f.store, &malformed)
            .expect_err("a missing id must refuse")
            .code,
        fusiform_protocol::CODE_BAD_REQUEST,
    );
}

/// The manifest declares what is TRUE of this module, in both directions.
///
/// # Why this test exists, measured rather than assumed
///
/// Under subc-protocol 0.18 `trust_tier` and `bindings` were positional
/// arguments, so the COMPILER kept them present. 0.19 made them optional
/// setters — correctly, because the daemon reads neither on any production path
/// and a required-but-unread field forces producers to invent values.
///
/// The consequence does not appear as a compile error: deleting
/// `.trust_tier(...)` built clean and passed the entire binary. The declaration
/// survived only as long as nobody tidied the builder chain, and the tidy is
/// INVITED, since the protocol itself says the field goes unread.
///
/// So both claims are pinned here, and the pinning is what makes them
/// declarations rather than leftovers:
///
/// - `trust_tier` PRESENT, because fusiform is first-party and that is true.
/// - `bindings` ABSENT, because the only storage scope the enum can express
///   reads as a per-project partition, and this catalog describes the world.
///   Two projects asking what models exist must get the same answer.
///
/// A manifest describes the module, not what the supervisor currently bothers
/// to read. The reader changes; the module does not.
#[test]
fn the_manifest_declares_what_is_true_and_omits_what_is_not() {
    let manifest = fusiform_module::manifest();
    let wire = serde_json::to_value(&manifest).expect("the manifest must serialize");

    assert_eq!(
        manifest.trust_tier,
        Some(subc_protocol::manifest::TrustTier::FirstParty),
        "fusiform is first-party; deleting the setter must redden here rather \
         than pass silently"
    );
    assert_eq!(
        wire.get("trust_tier").and_then(|v| v.as_str()),
        Some("first_party"),
        "and it must reach the wire, since the struct field alone does not \
         prove what a daemon receives"
    );

    assert!(
        manifest.bindings.is_none(),
        "bindings must stay absent: the only expressible storage scope reads as \
         a per-project partition, which is false for a catalog of the world"
    );
    assert!(
        wire.get("bindings").is_none() || wire["bindings"].is_null(),
        "absent on the wire too, so a reader sees no claim rather than a claim \
         this module would have to footnote"
    );

    // identity_scope EMPTY, which is a claim and not an omission: the set of
    // bind keys this provider PARTITIONS its answers by. Fusiform partitions by
    // none — two projects asking what models exist must get the same answer,
    // because the catalog describes the world rather than a workspace.
    //
    // Asserted on the wire as well as the struct, because an empty vec and an
    // absent field are different statements to a reader and only one of them is
    // what this module means.
    let scopes = wire["provides"][0]["identity_scope"]
        .as_array()
        .expect("the tool provider must declare an identity_scope array");
    assert!(
        scopes.is_empty(),
        "identity_scope must stay empty: any key here claims the answer depends \
         on the caller, which is false for a catalog of the world (got {scopes:?})"
    );
}

/// `catalog.status` names every key this producer can serve, INCLUDING keys no
/// model in the store carries.
///
/// # The defect this closes
///
/// A consumer discovering the vocabulary by reading a model and collecting its
/// keys is answering a different question: what did the upstream publish for
/// that model. A store whose models carry no cache pricing makes
/// `rate.cache_read` look unserved.
///
/// That happened. A routing consumer priced on `rate.output` alone, believing
/// it was the only rate served, while four other rate keys had been served
/// since the first ingest — ranking on roughly 5% of the real cost. The
/// inference was sound on the evidence they had, because absence on a model
/// means the UPSTREAM published nothing, which is the distinction this catalog
/// exists to keep. Turned against vocabulary discovery, it misleads.
///
/// So the list is read from the contract table rather than collected from the
/// snapshot, and this test drives a store holding exactly ONE fact to prove it:
/// if the answer came from the data, it would name one key.
#[test]
fn status_names_served_keys_no_model_in_this_store_carries() {
    let f = fixture();

    let status = match fusiform_module::route::serve_tool_call(
        &f.store,
        br#"{"name": "catalog.status", "arguments": {}}"#,
    )
    .expect("status must be served")
    {
        fusiform_module::route::ToolResponse::Status(s) => s,
        other => panic!("expected a status response, got {other:?}"),
    };

    // The keys the consumer was missing, none of which this store holds.
    for key in [
        "rate.input",
        "rate.output",
        "rate.cache_read",
        "rate.cache_write",
        "rate.reasoning",
    ] {
        assert!(
            status.served_facts.iter().any(|k| k == key),
            "{key} must be named as servable even though no model here carries \
             it — a vocabulary answer derived from today's data reports the \
             upstream's silence as this producer's limit. Got: {:?}",
            status.served_facts
        );
    }

    // Control: the list is the CONTRACT, so it must not be the store's keys.
    // Without this, the assertions above pass against a build that collects
    // from a fixture that happens to be rich.
    assert!(
        status.served_facts.len() >= 10,
        "the list must be the full served vocabulary rather than whatever this \
         store holds: {:?}",
        status.served_facts
    );

    // And tiered keys are NOT enumerated: the threshold is the upstream's, not
    // fusiform's to invent. A consumer matches them by prefix.
    assert!(
        !status
            .served_facts
            .iter()
            .any(|k| k.contains("above_context")),
        "tiered keys must not be listed — their thresholds come from the \
         upstream and cannot be enumerated: {:?}",
        status.served_facts
    );
    // Nor are mode rate keys: the mode names are the upstream's labels.
    assert!(
        !status.served_facts.iter().any(|k| k.contains(".mode.")),
        "mode rate keys must not be listed — their names come from the \
         upstream and cannot be enumerated: {:?}",
        status.served_facts
    );
}

/// An unstamped build SAYS it is unstamped, rather than saying nothing.
///
/// # What changed, and why it is not cosmetic
///
/// Until subc-protocol 0.18 a binary built by an ordinary `cargo build` carried
/// `provenance: None`. That conflates two different states: "this module states
/// nothing about its build" and "this build carries no commit". The first is a
/// silence and the second is a fact, and an auditor asking which one they are
/// looking at had no way to tell.
///
/// 0.18 added an absence REASON, so the absence became attestable and the
/// honest form changed. This is the same distinction the catalog keeps between
/// an absent rate and an unpriced one, applied to fusiform's own manifest.
///
/// # Why `NeverDerived` is the true reason here
///
/// `release-build.sh` refuses a dirty tree outright rather than building an
/// unstamped binary, so `DeclinedDirty` is structurally unreachable: a dirty
/// tree produces no fusiform artifact at all. The sentinel therefore means the
/// packaging path never ran.
///
/// This test runs under `cargo test`, which does not set `CK_BUILD_REV` — so it
/// drives exactly the unstamped case and cannot pass by accident on a stamped
/// one.
#[test]
fn an_unstamped_build_names_why_its_commit_is_absent() {
    let manifest = fusiform_module::manifest();

    let provenance = manifest
        .provenance
        .as_ref()
        .expect("an unstamped build must still DECLARE provenance: absent-with-a-reason and absent-entirely are different claims");

    assert!(
        provenance.build_git_sha.is_none(),
        "cargo test does not set CK_BUILD_REV, so no commit can be attested here: {:?}",
        provenance.build_git_sha
    );

    assert!(
        matches!(
            provenance.build_git_sha_absence_reason,
            Some(subc_protocol::manifest::BuildGitShaAbsenceReason::NeverDerived)
        ),
        "the reason must be NeverDerived — release-build.sh refuses a dirty tree \
         rather than building unstamped, so DeclinedDirty cannot describe a \
         fusiform binary: {:?}",
        provenance.build_git_sha_absence_reason
    );

    // Control, and it corrected me rather than confirming me.
    //
    // I asserted this field must stay ABSENT, carrying forward the reasoning
    // from when fusiform hand-built the struct: a crate can only see its own
    // CARGO_PKG_VERSION, so naming that as the WIRE crate's version is the
    // `version_line` defect. True of a literal built here. False now, because
    // the owner's constructor fills it from `SUBC_PROTOCOL_CRATE_VERSION`,
    // evaluated INSIDE subc-protocol — which is the definition-site rule
    // applied correctly rather than violated. Moving to the helper fixed the
    // defect my comment was working around.
    //
    // Asserted against the constant rather than a literal: the two crates are
    // BOTH at 0.21.0 today, so a literal would pass for the wrong reason and
    // keep passing after one of them moves. That coincidence is also what made
    // the first failure read as fusiform-protocol's version leaking.
    assert_eq!(
        provenance.wire_crate_version.as_deref(),
        Some(subc_protocol::SUBC_PROTOCOL_CRATE_VERSION),
        "the wire crate version must be the PROTOCOL crate's, filled where that \
         constant is true"
    );
}

/// A NAMED read of a retired model says it is retired, rather than answering
/// with silence.
///
/// # The defect this closes
///
/// `include_retired` defaults false, so a named read of a withdrawn model
/// returned `"models": {}` with 200 OK — the same shape a caller gets when
/// their fact filter matches nothing. A consumer polling presence therefore
/// held a withdrawn model on its roster indefinitely, while another seat
/// honouring the tombstone refused every request against it. Neither could see
/// the disagreement, because the absence carried no reason and none could be
/// inferred from the response.
///
/// Unknown models already refuse with `no_coverage`, so the gap was precisely
/// between "retired and filtered out" and "nothing to say".
#[test]
fn a_named_read_of_a_retired_model_says_so() {
    let f = fixture();

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
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: Some(5),
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

    let named = get(
        &f,
        r#"{"provider_id": "anthropic", "model_id": "claude-sonnet-4-5"}"#,
    );

    assert!(
        named.models.is_empty(),
        "the model stays out of `models` — the default still means present"
    );
    assert_eq!(
        named.retired.len(),
        1,
        "and the response must SAY it was excluded rather than leaving the \
         caller to infer it from an empty map: {:?}",
        named.retired
    );
    assert_eq!(named.retired[0].model, "anthropic/claude-sonnet-4-5");
    assert!(
        named.retired[0].retired_at_ms > 0,
        "carrying when fusiform noticed, so a consumer can tell a retirement \
         from last week apart from one this morning: {:?}",
        named.retired[0]
    );

    // CONTROL: a present model must not be reported as retired, or the
    // assertions above pass against a build that reports every named read.
    let present = get(
        &f,
        r#"{"provider_id": "google", "model_id": "gemini-flash-latest"}"#,
    );
    assert!(
        !present.models.is_empty(),
        "control: this model is present and must be served"
    );
    assert!(
        present.retired.is_empty(),
        "control: a present model is not a retirement: {:?}",
        present.retired
    );

    // CONTROL: asking FOR retirements excludes nothing, so there is nothing to
    // report. An entry here would double-report the same model.
    let audit = get(
        &f,
        r#"{"provider_id": "anthropic", "model_id": "claude-sonnet-4-5", "include_retired": true}"#,
    );
    assert!(
        !audit.models.is_empty(),
        "control: an audit read serves the retired model"
    );
    assert!(
        audit.retired.is_empty(),
        "control: nothing was excluded, so nothing is reported excluded: {:?}",
        audit.retired
    );
}

/// `plan.prices` serves a curated row, its refusal twin, and the unit policy.
///
/// Driven through `serve_tool_call` — the real envelope, the real dispatch —
/// rather than by calling the handler. A handler tested through its own front
/// door is tested in a world where its caller is correct, and this tool's
/// caller is the thing that was just added.
#[test]
fn plan_prices_serves_rows_with_their_provenance() {
    let f = fixture();

    f.store
        .append_plan_prices(&[
            fusiform_store::NewPlanPrice {
                provider_id: "anthropic".to_string(),
                tier: "max_20x".to_string(),
                minor_units: Some(20000),
                exponent: Some(2),
                currency: Some("USD".to_string()),
                period: Some("month".to_string()),
                boundary_at_ms: 1_000,
                established_by: "fusi".to_string(),
                established_at_ms: 2_000,
                review_by_ms: 9_000_000_000_000,
                source_ref: "https://example/max".to_string(),
                refusal_reason: None,
            },
            fusiform_store::NewPlanPrice {
                provider_id: "openai".to_string(),
                tier: "pro".to_string(),
                minor_units: None,
                exponent: None,
                currency: None,
                period: None,
                boundary_at_ms: 1_000,
                established_by: "fusi".to_string(),
                established_at_ms: 2_000,
                review_by_ms: 9_000_000_000_000,
                source_ref: "https://example/pro".to_string(),
                refusal_reason: Some("tier string does not resolve".to_string()),
            },
        ])
        .expect("curated rows");

    let call = serde_json::json!({"name": "plan.prices", "arguments": {}});
    let out = serve_tool_call(&f.store, &serde_json::to_vec(&call).unwrap())
        .expect("plan.prices is served");
    let ToolResponse::PlanPrices(r) = out else {
        panic!("wrong response arm");
    };

    assert_eq!(r.prices.len(), 2);

    let priced = r
        .prices
        .iter()
        .find(|p| p.provider_id == "anthropic")
        .expect("the priced row");
    let amount = priced.price.as_ref().expect("carries a price");
    assert_eq!(amount.minor_units, 20000);
    assert_eq!(amount.currency, "USD");
    assert_eq!(amount.period, "month");
    assert!(
        priced.refusal_reason.is_none(),
        "a priced row must not also explain why it has none"
    );
    assert_eq!(priced.source_ref, "https://example/max");

    // The refusal: no price, and a REASON rather than a silence. Collapsing
    // this into absence is the whole thing the plane refuses.
    let refused = r
        .prices
        .iter()
        .find(|p| p.provider_id == "openai")
        .expect("the refusal row");
    assert!(refused.price.is_none());
    assert_eq!(
        refused.refusal_reason.as_deref(),
        Some("tier string does not resolve")
    );
    assert!(
        !refused.source_ref.is_empty(),
        "a refusal must name the page that does NOT publish the price, so a \
         reviewer checks the same source rather than guessing at one"
    );

    // Case-insensitive, and that is not laziness: this assertion failed when
    // the policy was split from its reasoning and the wording relaxed from
    // shouty prose. A test pinned to CASING fails on an edit that changes
    // nothing it cares about, which teaches its reader to edit the test.
    let policy = r.unit_policy.to_lowercase();
    for term in ["us list", "monthly-billed", "web subscription"] {
        assert!(
            policy.contains(term),
            "the basis travels with the prices, or a consumer assumes one — \
             missing {term:?}: {}",
            r.unit_policy
        );
    }
    // The TIER VOCABULARY reaches the wire, asserted at the ROUTE rather than
    // at the file. A test that reads the curated file proves the file carries a
    // statement; only driving serve_tool_call proves a consumer receives it,
    // and emptying this at the serve path survived every other test here.
    let vocab = r.tier_vocabulary.to_lowercase();
    for term in ["tier name", "api plan string"] {
        assert!(
            vocab.contains(term),
            "a consumer cannot tell a vendor tier name from their own API's \
             plan string without this — missing {term:?}: {}",
            r.tier_vocabulary
        );
    }

    assert!(
        r.unit_policy.len() < 400,
        "the served policy is the STATEMENT; its argument stays in the file: {}",
        r.unit_policy.len()
    );

    // Narrowing works, and the filter is not decorative.
    let call =
        serde_json::json!({"name": "plan.prices", "arguments": {"provider_id": "anthropic"}});
    let out = serve_tool_call(&f.store, &serde_json::to_vec(&call).unwrap()).expect("narrowed");
    let ToolResponse::PlanPrices(r) = out else {
        panic!("wrong arm");
    };
    assert_eq!(r.prices.len(), 1, "the filter must actually narrow");
    assert_eq!(r.prices[0].provider_id, "anthropic");
}

/// A provider nobody has curated is REFUSED, not answered with an empty list.
///
/// An empty answer reads as "this provider has no subscription pricing", which
/// is a claim about the world. The truth is that nobody has sourced one, and
/// only one of those two is someone's job.
#[test]
fn an_uncurated_provider_is_refused_rather_than_empty() {
    let f = fixture();

    let call = serde_json::json!({"name": "plan.prices", "arguments": {"provider_id": "nobody"}});
    let err = serve_tool_call(&f.store, &serde_json::to_vec(&call).unwrap())
        .expect_err("an uncurated provider must refuse");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("absence of curation"),
        "the refusal must say WHICH absence this is: {msg}"
    );

    // CONTROL: an unfiltered read of an empty plane is a legitimate empty
    // answer, not a refusal. Without this arm the assertion above passes
    // against a handler that refuses every empty result.
    let call = serde_json::json!({"name": "plan.prices", "arguments": {}});
    let out = serve_tool_call(&f.store, &serde_json::to_vec(&call).unwrap())
        .expect("an unfiltered read of an empty plane is not an error");
    let ToolResponse::PlanPrices(r) = out else {
        panic!("wrong arm");
    };
    assert!(r.prices.is_empty());
}
