//! Read surfaces preserve recorded values, bounded corrections and poll provenance.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::{serve_tool_call, ToolResponse};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};

/// Two historical upstream rows used to exercise bounded store corrections.
/// The values are a fixed old cut, not claims about today's catalog.
const UPSTREAM: &str = r#"{
  "anthropic": {
    "id": "anthropic",
    "name": "Anthropic",
    "models": {
      "claude-sonnet-4-5": {
        "id": "claude-sonnet-4-5",
        "name": "Claude Sonnet 4.5",
        "limit": { "context": 1000000, "output": 64000 },
        "reasoning": true,
        "tool_call": true,
        "attachment": true,
        "modalities": { "input": ["text"], "output": ["text"] },
        "cost": { "input": 3, "output": 15 }
      },
      "claude-opus-5": {
        "id": "claude-opus-5",
        "name": "Claude Opus 5",
        "limit": { "context": 1000000, "output": 128000 },
        "reasoning": true,
        "tool_call": true,
        "attachment": true,
        "modalities": { "input": ["text"], "output": ["text"] },
        "cost": { "input": 5, "output": 25 }
      }
    }
  }
}"#;

fn store() -> (CatalogStore, tempfile::TempDir) {
    store_from(UPSTREAM)
}

fn store_from(upstream: &str) -> (CatalogStore, tempfile::TempDir) {
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

    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1_000),
            outcome: ObservationOutcome::Seeded,
            normalized_hash: Some("h".into()),
            raw_hash: Some("r".into()),
            etag: None,
            duration_ms: Some(10),
            detail: None,
        })
        .unwrap();

    let catalog = normalize_models_dev(upstream.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();

    (store, dir)
}

fn get(store: &CatalogStore, body: &str) -> serde_json::Value {
    match serve_tool_call(store, body.as_bytes()).expect("must serve") {
        ToolResponse::Catalog(c) => serde_json::to_value(c).unwrap(),
        other => panic!("expected a catalog, got {other:?}"),
    }
}

/// An ordinary read must not invent rates absent from the upstream row.
#[test]
fn an_ordinary_read_preserves_unpublished_rate_absence() {
    let (store, _dir) = store();
    let response = get(&store, r#"{"name":"catalog.get","arguments":{}}"#);
    let sonnet = &response["models"]["anthropic/claude-sonnet-4-5"];

    // Verify this is a served row, rather than an empty result whose missing
    // keys would satisfy the absence check for the wrong reason.
    assert_eq!(sonnet["limit.context"], 1_000_000);
    assert!(sonnet.get("rate.reasoning").is_none());
    for key in sonnet.as_object().unwrap().keys() {
        assert!(
            !key.starts_with("rate.") || key == "rate.input" || key == "rate.output",
            "served rate {key} was not published by this fixture"
        );
    }
}

/// A point-in-time read and a current read are distinguishable on the wire.
///
/// # The property BROCA's completeness guard needs and nothing asserted
///
/// A response whose identity set is smaller than the one a consumer holds has
/// two causes needing opposite responses: the consumer asked for the past
/// (correct, models arrive daily), or fusiform's store went backwards (an
/// engram restore, and the missing models are missing from the CURRENT
/// catalog).
///
/// `catalog_version` cannot tell them apart — it is `max(now_ms, current + 1)`,
/// so a restore raises it while the content goes back, and a historical read
/// carries the current high version with an older identity set. Both present as
/// "version rose, identities shrank".
///
/// `resolved_at_ms` CAN: it is the requested instant for a point-in-time read
/// and approximately now for a current one. I told two seats these responses
/// were byte-identical, having compared the versions and not the rest of the
/// response — the discriminator has been on the wire since the first response
/// and nothing asserted it, which is exactly how a field a consumer needs stops
/// being one a consumer can rely on.
#[test]
fn a_historical_read_is_distinguishable_from_a_current_one() {
    let (store, _dir) = store();

    let current = get(&store, r#"{"name":"catalog.get","arguments":{}}"#);
    let past = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"at_ms":1500}}"#,
    );

    // The control: the versions really are the same, or there is nothing to
    // discriminate and this test proves nothing.
    assert_eq!(
        current["catalog_version"], past["catalog_version"],
        "control: both reads must carry the SAME version, which is why the \
         version cannot be the discriminator"
    );

    let current_at = current["resolved_at_ms"].as_i64().unwrap();
    let past_at = past["resolved_at_ms"].as_i64().unwrap();

    assert_eq!(
        past_at, 1500,
        "a point-in-time read must resolve to the REQUESTED instant. If it \
         reports now, a consumer cannot tell its own historical query from a \
         store that went backwards, and a completeness guard has nothing to \
         branch on."
    );
    assert!(
        current_at > past_at,
        "a current read must resolve to now, not to a stored instant: {current_at} \
         is not later than {past_at}"
    );

    // THE SINGLE-MODEL PATH TOO, because it computes `resolved_at` separately.
    //
    // A mutation neutering only that path survived this test until this block
    // existed: the bulk read and the per-model read are two surfaces answering
    // one question, and this file has already found `read_model` silently
    // lacking correction handling that `read_catalog` had. A consumer asking
    // for one model must get the same discriminator as one asking for the
    // catalog, or the guard works until someone narrows their query.
    let one = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","at_ms":1500}}"#,
    );
    assert_eq!(
        one["resolved_at_ms"].as_i64().unwrap(),
        1500,
        "the single-model path must resolve to the requested instant as well: \
         a consumer that narrows its query must not lose the field its \
         completeness check branches on"
    );
}

/// A failed poll outside the status window is still reported.
///
/// # The window hides exactly what an operator came to find
///
/// `recent_polls` is windowed — ten by default. The live store's only failed
/// poll sits about forty polls back, so an operator asking "what has fusiform
/// been doing" sees ten clean rows and nothing suggesting there is more. It is
/// reachable with `--polls 60` and nothing tells them to pass it.
///
/// This is the rare-event argument applied to a WINDOW rather than to a gauge:
/// a failure that self-clears leaves no trace in the current state, and a
/// windowed history only shows it if someone looks soon enough. Both conditions
/// have to hold, and neither is under the operator's control.
#[test]
fn a_failure_older_than_the_poll_window_is_still_reported() {
    let (store, _dir) = store();

    // A failure old enough to fall outside any reasonable window, then enough
    // successes after it that the recent-polls list cannot contain it.
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(100),
            outcome: ObservationOutcome::Failed {
                class: fusiform_core::FailureClass::Network,
            },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: Some(50),
            detail: Some("upstream unreachable".into()),
        })
        .unwrap();
    // A SECOND failure, because one cannot distinguish a count from a constant.
    //
    // With a single failure in the fixture, `ever: 1` hardcoded in the producer
    // passes every assertion here. Found by mutation, and it is the third time
    // in this repo that a fixture holding one of a thing hid exactly that: the
    // count and the constant are the same value.
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(200),
            outcome: ObservationOutcome::Failed {
                class: fusiform_core::FailureClass::Parse,
            },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: Some(60),
            detail: Some("malformed document".into()),
        })
        .unwrap();
    for i in 0..15 {
        store
            .record_observation(&NewObservation {
                source: SourceId::ModelsDev,
                observed_at: Timestamp(1_000 + i * 100),
                outcome: ObservationOutcome::NotModified,
                normalized_hash: None,
                raw_hash: None,
                etag: None,
                duration_ms: Some(10),
                detail: None,
            })
            .unwrap();
    }

    let status = match serve_tool_call(
        &store,
        br#"{"name":"catalog.status","arguments":{"polls":5}}"#,
    )
    .expect("must serve")
    {
        ToolResponse::Status(s) => serde_json::to_value(s).unwrap(),
        other => panic!("expected status, got {other:?}"),
    };

    // The control: the window really does exclude the failure, or this test is
    // asserting against a list that happens to contain it anyway.
    let polls = status["recent_polls"].as_array().unwrap();
    assert!(
        polls.iter().all(|p| p["outcome"] != "failed"),
        "control: the poll window must NOT contain the failure, or the total \
         below proves nothing about reaching past the window"
    );

    let failures = &status["failures"];
    assert_eq!(
        failures["ever"], 2,
        "a failed poll outside the window must still be counted: an operator \
         reading a clean ten-row list has no reason to pass --polls 60, so the \
         windowed history is the only surface and it is silent. Got {status}"
    );
    assert_eq!(
        failures["last_at_ms"], 200,
        "and the instant must be the failure's, so an operator knows how far \
         back to look"
    );
}

/// History preserves the upstream's values across observations.
#[test]
fn history_preserves_the_recorded_values() {
    let (store, _dir) = store();

    // Change only one model so the other remains an independent control.
    let moved = {
        let mut doc: serde_json::Value = serde_json::from_str(UPSTREAM).unwrap();
        let limit = &mut doc["anthropic"]["models"]["claude-sonnet-4-5"]["limit"];
        assert_eq!(limit["context"], 1_000_000, "the fixture must start here");
        limit["context"] = serde_json::json!(900_000);
        serde_json::to_string(&doc).unwrap()
    };
    let catalog = normalize_models_dev(moved.as_bytes()).unwrap().catalog;
    let obs = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(2_000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some("h2".into()),
            raw_hash: Some("r2".into()),
            etag: None,
            duration_ms: Some(10),
            detail: None,
        })
        .unwrap();
    let plan = plan_ingest(
        &store,
        &catalog,
        Timestamp(2_000),
        BoundaryKind::Observed,
        Some(obs),
    )
    .unwrap();
    store.append_eras(&plan.eras).unwrap();

    let history = |body: &str| -> serde_json::Value {
        match serve_tool_call(&store, body.as_bytes()).expect("must serve") {
            ToolResponse::History(h) => serde_json::to_value(h).unwrap(),
            other => panic!("expected history, got {other:?}"),
        }
    };

    let recorded = history(
        r#"{"name":"catalog.history","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","fact_key":"limit.context"}}"#,
    );

    // Serving must not rewrite either observation.
    let eras = recorded["eras"].as_array().expect("eras is an array");
    assert_eq!(eras.len(), 2);
    assert!(
        eras.iter().any(|e| e["value"] == 1_000_000) && eras.iter().any(|e| e["value"] == 900_000),
        "the recorded eras must still say what the upstream published, both \
         before and after it moved"
    );
    let current = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5"}}"#,
    );
    assert_eq!(
        current["models"]["anthropic/claude-sonnet-4-5"]["limit.context"],
        900_000
    );
    let past = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","at_ms":1000}}"#,
    );
    assert_eq!(
        past["models"]["anthropic/claude-sonnet-4-5"]["limit.context"],
        1_000_000
    );
    let agreeing = history(
        r#"{"name":"catalog.history","arguments":{"provider_id":"anthropic","model_id":"claude-opus-5","fact_key":"limit.context"}}"#,
    );
    assert_eq!(agreeing["eras"][0]["value"], 1_000_000);

    // Changing context must not change a different fact.
    let untouched = history(
        r#"{"name":"catalog.history","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","fact_key":"limit.output"}}"#,
    );
    assert_eq!(untouched["eras"][0]["value"], 64_000);
}

/// A bounded correction removes the bad fact on both named and bulk reads,
/// while leaving the current observation available.
#[test]
fn a_corrected_fact_is_withheld_from_named_and_bulk_reads() {
    use fusiform_core::{FieldId, LimitId};
    use fusiform_store::correct::{apply_correction, plan_correction};

    let (store, _dir) = store();

    // Move the fact so there is a history to correct within.
    let moved = {
        let mut doc: serde_json::Value = serde_json::from_str(UPSTREAM).unwrap();
        doc["anthropic"]["models"]["claude-sonnet-4-5"]["limit"]["context"] =
            serde_json::json!(900_000);
        serde_json::to_string(&doc).unwrap()
    };
    let catalog = normalize_models_dev(moved.as_bytes()).unwrap().catalog;
    let obs = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(2_000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some("h2".into()),
            raw_hash: Some("r2".into()),
            etag: None,
            duration_ms: Some(10),
            detail: None,
        })
        .unwrap();
    let plan = plan_ingest(
        &store,
        &catalog,
        Timestamp(2_000),
        BoundaryKind::Observed,
        Some(obs),
    )
    .unwrap();
    store.append_eras(&plan.eras).unwrap();

    // An operator marks the first era's window as wrong.
    let now = Timestamp(3_000);
    let planned = plan_correction(
        &store,
        SourceId::ModelsDev,
        "anthropic",
        "claude-sonnet-4-5",
        &[FieldId::Limit {
            limit: LimitId::Context,
        }],
        Timestamp(1_000),
        Timestamp(1_500),
        "fusiform misread the window during this interval",
        now,
    )
    .expect("planning must not error");

    let planned = match planned {
        Ok(p) => p,
        Err(refusals) => panic!("the correction must be plannable: {refusals:?}"),
    };
    apply_correction(&store, SourceId::ModelsDev, &planned, now).expect("the correction applies");

    // The correction's bounded window must not reach the current observation.
    let current = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5"}}"#,
    );
    assert_eq!(
        current["models"]["anthropic/claude-sonnet-4-5"]["limit.context"],
        900_000
    );
    let withheld_now: Vec<&serde_json::Value> = current["withheld"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    assert!(
        !withheld_now
            .iter()
            .any(|w| w["fact_key"] == "limit.context"),
        "a correction bounded in the past must not withhold a CURRENT read: {:?}",
        current["withheld"]
    );

    // A point-in-time read inside the corrected window withholds the fact.
    let past = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","at_ms":1200}}"#,
    );
    let withheld_then: Vec<&serde_json::Value> = past["withheld"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    assert!(
        withheld_then
            .iter()
            .any(|w| w["fact_key"] == "limit.context"),
        "the corrected interval must withhold the fact: {:?}",
        past["withheld"]
    );

    // Listing a withheld fact is not enough: it must also be absent from the
    // values a consumer reads, or the known-bad value still reaches callers.
    let facts = &past["models"]["anthropic/claude-sonnet-4-5"];
    assert!(
        facts.get("limit.context").is_none(),
        "a withheld fact must not also be SERVED: the correction exists to          stop this value reaching a consumer, and a response carrying it in          `models` delivers it to anyone who does not read `withheld`. Got: {facts}"
    );

    // THE SAME INVARIANT ON THE BULK PATH, which is a different function.
    //
    // `read_model` and `read_catalog` each withhold separately, and the leak
    // above was proven uncovered on the BULK one — a narrowed read and a whole
    // catalog read are the two-surface split this repo keeps finding. A test
    // driving only the narrowed read certifies the half that was already fine.
    let bulk = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"at_ms":1200}}"#,
    );
    let bulk_facts = &bulk["models"]["anthropic/claude-sonnet-4-5"];
    assert!(
        bulk_facts.get("limit.context").is_none(),
        "a withheld fact must not be served by the BULK read either: {bulk_facts}"
    );
    assert!(
        bulk["withheld"]
            .as_array()
            .map(|a| a.iter().any(|w| w["fact_key"] == "limit.context"))
            .unwrap_or(false),
        "control: the bulk read must withhold it, or the assertion above holds \
         for the wrong reason — a fact absent because the read found nothing \
         proves nothing about withholding"
    );

    // And the withheld entry must carry the operator's reason, or an operator
    // meeting a refusal cannot tell a correction from a defect.
    let entry = withheld_then
        .iter()
        .find(|w| w["fact_key"] == "limit.context")
        .unwrap();
    assert!(
        !entry["corrections"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true),
        "a withheld fact must name the correction that withheld it: {entry}"
    );
}

/// A past read can be REPLAYED: `resolved_at_ms` out, `at_ms` back in.
///
/// # The contract this pins, and the field that does not serve it
///
/// A consumer reproducing a past decision — "which model did I route to, and
/// what did the catalog say then" — needs to identify the catalog it read.
/// The obvious candidate is `catalog_version`, and it is the wrong one: that
/// field describes the store at READ time, so a point-in-time read stamps
/// TODAY's watermark on a response carrying values from days earlier. Two
/// replays of different instants are indistinguishable by it.
///
/// `resolved_at_ms` is the snapshot identifier. This drives the full round
/// trip and asserts the values come back, which is the only claim worth
/// making: the doc on `catalog_version` describes this behaviour in prose, and
/// prose has no fence.
#[test]
fn a_read_can_be_replayed_by_the_instant_it_resolved() {
    let (store, _dir) = store();

    // A later poll moves the fact, so "then" and "now" differ.
    let moved = {
        let mut doc: serde_json::Value = serde_json::from_str(UPSTREAM).unwrap();
        doc["anthropic"]["models"]["claude-sonnet-4-5"]["limit"]["output"] =
            serde_json::json!(32_000);
        serde_json::to_string(&doc).unwrap()
    };
    let catalog = normalize_models_dev(moved.as_bytes()).unwrap().catalog;
    let obs = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(5_000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some("h5".into()),
            raw_hash: Some("r5".into()),
            etag: None,
            duration_ms: Some(10),
            detail: None,
        })
        .unwrap();
    let plan = plan_ingest(
        &store,
        &catalog,
        Timestamp(5_000),
        BoundaryKind::Observed,
        Some(obs),
    )
    .unwrap();
    store.append_eras(&plan.eras).unwrap();

    // The read a decision would have made, at an instant before the change.
    let then = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","at_ms":2000}}"#,
    );
    let recorded = then["resolved_at_ms"]
        .as_i64()
        .expect("a response must say which instant it resolved");
    assert_eq!(
        recorded, 2_000,
        "resolved_at_ms must echo the instant asked for, or a decision record \
         cannot name the catalog it read"
    );
    let value_then = then["models"]["anthropic/claude-sonnet-4-5"]["limit.output"].clone();

    // Now: the fact has moved.
    let now = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5"}}"#,
    );
    let value_now = now["models"]["anthropic/claude-sonnet-4-5"]["limit.output"].clone();
    assert_ne!(
        value_then, value_now,
        "control: the fact must differ between the two instants, or the replay \
         below proves nothing"
    );

    // THE REPLAY: feed the recorded instant back in.
    let replayed = get(
        &store,
        &format!(
            r#"{{"name":"catalog.get","arguments":{{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","at_ms":{recorded}}}}}"#
        ),
    );
    assert_eq!(
        replayed["models"]["anthropic/claude-sonnet-4-5"]["limit.output"], value_then,
        "replaying a recorded resolved_at_ms must return the values that were \
         in force then, not the values in force now"
    );

    // And the field that CANNOT serve this purpose: both reads carry the same
    // catalog_version despite resolving different catalogs.
    assert_eq!(
        then["catalog_version"], now["catalog_version"],
        "catalog_version must be identical across a historical and a current \
         read — it describes the store at read time. This is why a decision \
         record cannot use it to identify a snapshot, and asserting it here \
         means the doc saying so cannot quietly stop being true."
    );
}

/// A read before the record begins is REFUSED, not answered with nothing.
///
/// # The defect, measured on the live store
///
/// Fusiform's history begins 2026-08-12 10:07:29Z. A read at 02:00:00Z the
/// same morning returned `0 models` with a success status — well-formed, no
/// signal, and a claim about the world from a store whose only honest claim is
/// about its own coverage.
///
/// This is the case a replaying consumer hits first and notices last. ASTRO's
/// historical charges are priced against observations from 19 July, a month
/// before this store existed; every replay of those instants would have
/// returned an empty catalog that reads as "nothing was published then".
///
/// # The distinction it must NOT break
///
/// A model that exists and was not published at T is a real answer: fusiform
/// watched at T and did not see it. That stays an empty result. The difference
/// is whether fusiform was watching at all, which no per-model check can
/// determine — only the store's coverage bound.
#[test]
fn a_read_before_the_record_is_refused_rather_than_answered_empty() {
    let (store, _dir) = store();

    // The fixture's observation is at 1_000, so anything earlier predates it.
    let before = r#"{"name":"catalog.get","arguments":{"at_ms":500}}"#;
    let err = match serve_tool_call(&store, before.as_bytes()) {
        Err(e) => format!("{e:?}"),
        Ok(other) => panic!(
            "a read before the record must be refused; answering it with an \
             empty catalog claims the catalog was empty when it was unobserved. \
             Got: {other:?}"
        ),
    };
    assert!(
        err.contains("no record at"),
        "the refusal must name the problem and the bound, got: {err}"
    );

    // BOTH paths, because this crate has produced the single-model/bulk split
    // three times. The guard sits above the branch precisely so it cannot.
    let before_one = r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","at_ms":500}}"#;
    assert!(
        serve_tool_call(&store, before_one.as_bytes()).is_err(),
        "the single-model path must refuse it too"
    );

    // CONTROL 1: an instant INSIDE the record is served, or the refusal above
    // is just a broken read path.
    let inside = r#"{"name":"catalog.get","arguments":{"at_ms":1500}}"#;
    let served = match serve_tool_call(&store, inside.as_bytes()) {
        Ok(ToolResponse::Catalog(c)) => serde_json::to_value(c).unwrap(),
        other => panic!("an instant inside the record must be served: {other:?}"),
    };
    assert!(
        !served["models"].as_object().unwrap().is_empty(),
        "control: a covered instant must return models"
    );

    // CONTROL 2: the exact boundary is INSIDE the record, not outside it. The
    // record begins AT that observation, so a read there is answerable — an
    // off-by-one here would refuse the first instant fusiform ever saw.
    let boundary = r#"{"name":"catalog.get","arguments":{"at_ms":1000}}"#;
    assert!(
        serve_tool_call(&store, boundary.as_bytes()).is_ok(),
        "the first observed instant must be answerable: the record begins there"
    );

    // CONTROL 3: a CURRENT read is never affected, since it names no instant.
    let current = r#"{"name":"catalog.get","arguments":{}}"#;
    assert!(
        serve_tool_call(&store, current.as_bytes()).is_ok(),
        "a current read must not be touched by a coverage bound"
    );
}

/// `catalog.status` names the bound the refusal above enforces, as a number.
///
/// A consumer replaying history needs to know where the record starts before
/// it asks. Without this field the only way to learn it was to send a
/// too-old read and parse the number out of the refusal's English message.
/// The test checks both halves of the contract against each other: the bound
/// status reports is answerable, and one millisecond before it is refused.
#[test]
fn status_reports_the_bound_the_pre_history_refusal_enforces() {
    let (store, _dir) = store();

    let status = match serve_tool_call(&store, br#"{"name":"catalog.status","arguments":{}}"#) {
        Ok(ToolResponse::Status(s)) => s,
        other => panic!("catalog.status must be served: {other:?}"),
    };
    let begins = status
        .history_begins_at_ms
        .expect("a store with an observation must report where its history begins");
    // The fixture's first observation is at 1_000.
    assert_eq!(begins, 1_000);

    let at = |ms: i64| format!(r#"{{"name":"catalog.get","arguments":{{"at_ms":{ms}}}}}"#);
    assert!(
        serve_tool_call(&store, at(begins).as_bytes()).is_ok(),
        "the reported bound must itself be answerable"
    );
    assert!(
        serve_tool_call(&store, at(begins - 1).as_bytes()).is_err(),
        "one millisecond before the reported bound must be refused, or the \
         field and the refusal disagree about where the record starts"
    );
}

/// A named model with no facts in the requested plane is still RETURNED.
///
/// # The defect, and the population that makes it matter
///
/// `catalog.get` with `fact_prefixes: ["rate."]` on a model that publishes no
/// rate returned an empty models map — indistinguishable from a typo, from a
/// retired model, and from a provider that does not exist. Measured on the
/// live store 2026-08-15: 419 of 6,583 present models publish no rate at all,
/// so 6.4% of the catalog answered a pricing question with silence.
///
/// For a ledger the distinction is the question itself. A model that is
/// UNPRICED and used means a subscription or a billing gap; a model that is
/// UNKNOWN and used means a bad id. The store holds the difference — it
/// deliberately writes no rate row rather than a fabricated zero — and the
/// serve path was collapsing it.
///
/// # Why the bulk read still drops them
///
/// It answers a different question. "Give me the rate plane" is a request for
/// models that have rates, not a census, and returning 419 empty entries would
/// be noise in a 6,583-model response. Here the caller NAMED the subject, so
/// the answer is about that subject. The two paths differ because the
/// questions differ, which is worth stating since this crate has three
/// separate defects from the two paths differing by accident.
#[test]
fn a_named_model_with_no_facts_in_the_plane_is_still_returned() {
    let (store, _dir) = store();

    // claude-sonnet-4-5 in this fixture publishes limits and capabilities.
    // Ask for a plane it has nothing in.
    let response = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","fact_prefixes":["nonexistent.plane."]}}"#,
    );
    let models = response["models"]
        .as_object()
        .expect("models must be an object");
    assert_eq!(
        models.len(),
        1,
        "a NAMED model must be returned even when the plane filter matches \
         nothing: an empty map answers 'no such model' to a question about a \
         model that exists. Got: {models:?}"
    );
    let facts = &models["anthropic/claude-sonnet-4-5"];
    assert!(
        facts.as_object().unwrap().is_empty(),
        "and it must carry NO facts, since none matched — the emptiness is the \
         answer, not the absence of the model: {facts}"
    );

    // CONTROL 1: an unknown model must still REFUSE rather than return an
    // empty entry, or the fix above has turned a typo into a plausible answer.
    let unknown = serve_tool_call(
        &store,
        br#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-9-9","fact_prefixes":["rate."]}}"#,
    );
    assert!(
        unknown.is_err(),
        "an unknown model must refuse, not return an empty fact map — \
         otherwise this fix makes a typo indistinguishable from an unpriced \
         model, which is the very distinction it exists to draw"
    );

    // CONTROL 2: the unfiltered read is unchanged and carries real facts.
    let full = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5"}}"#,
    );
    assert!(
        !full["models"]["anthropic/claude-sonnet-4-5"]
            .as_object()
            .unwrap()
            .is_empty(),
        "control: the unfiltered read must still carry facts"
    );

    // CONTROL 3: the BULK read keeps dropping them, deliberately. If this ever
    // changes it should be a decision, not a side effect of the line above.
    let bulk = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"fact_prefixes":["nonexistent.plane."]}}"#,
    );
    assert!(
        bulk["models"].as_object().unwrap().is_empty(),
        "the bulk read answers 'give me this plane' and returns models that \
         have it: {}",
        bulk["models"]
    );
}

/// A rate written before `unit_provenance` existed still says how its currency
/// was established, and a rate that carries one is left alone.
///
/// # Why a serve-time default rather than absence
///
/// The field began being stored in fusiform-protocol 0.11.0, and the live
/// store holds 19,707 rate eras written before it. An append-only store never
/// rewrites them, so absence would have to mean something on the wire — and it
/// cannot mean nothing, because `UnitProvenance::Unknown` is a real variant
/// meaning "no statement and no policy covers this". That is the state that
/// stops a non-USD provider being silently priced in dollars, so a consumer
/// treating an absent field as unknown would refuse to price rates that are
/// perfectly well established.
///
/// The default is a fact rather than a guess: every pre-0.11.0 rate came
/// through one normalization path whose only currency branch is
/// `AssumedByPolicy(models-dev-usd-v1)`, because models.dev publishes no
/// currency field at all. The population is closed and can only shrink.
#[test]
fn a_rate_stored_before_the_field_existed_still_names_its_policy() {
    let (store, _dir) = store();

    // A rate era in the PRE-0.11.0 shape, written directly so the test drives
    // the real historical payload rather than a reconstruction of it.
    store
        .append_eras(&[fusiform_store::NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: fusiform_store::FactKey::rate(fusiform_core::TokenClass::Output),
            value_json: r#"{"state":"priced","units":15000000000,"exponent":9,"currency":"USD"}"#
                .to_string(),
            boundary_at: Timestamp(1_500),
            boundary_kind: BoundaryKind::Observed,
            observation_id: None,
        }])
        .expect("the legacy-shaped era must store");

    let response = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5"}}"#,
    );
    let rate = &response["models"]["anthropic/claude-sonnet-4-5"]["rate.output"];

    assert_eq!(
        rate["units"], 15_000_000_000i64,
        "control: the legacy row must be the one being served, or the \
         assertion below is about some other era: {rate}"
    );
    assert_eq!(
        rate["unit_provenance"]["kind"], "assumed_by_policy",
        "a rate written before the field existed must still say how its \
         currency was established: an absent field is indistinguishable from \
         UnitProvenance::Unknown, which means something else entirely: {rate}"
    );
    assert_eq!(
        rate["unit_provenance"]["policy"], "models-dev-usd-v1",
        "and it must name the policy that supplied it: {rate}"
    );

    // A NON-PRICED state must not acquire a provenance it has no use for.
    // `stated_zero` and `unpriced` carry no currency, so stamping a policy on
    // them would claim a unit for a value that has none.
    store
        .append_eras(&[fusiform_store::NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-sonnet-4-5".into(),
            fact_key: fusiform_store::FactKey::rate(fusiform_core::TokenClass::CacheWrite),
            value_json: r#"{"state":"stated_zero"}"#.to_string(),
            boundary_at: Timestamp(1_600),
            boundary_kind: BoundaryKind::Observed,
            observation_id: None,
        }])
        .expect("a stated zero must store");

    let response = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5"}}"#,
    );
    let zero = &response["models"]["anthropic/claude-sonnet-4-5"]["rate.cache_write"];
    assert_eq!(zero["state"], "stated_zero", "control: the zero must serve");
    assert!(
        zero.get("unit_provenance").is_none(),
        "a stated zero carries no currency, so it must not acquire a policy \
         that supplied one: {zero}"
    );
}

/// Exactly one currency policy has ever existed, which is what makes the
/// pre-0.11.0 default a fact rather than a guess.
///
/// The serve-time backfill labels every provenance-less rate
/// `models-dev-usd-v1`. That is true only while there is one policy: a second
/// one means some pre-existing rows were written under it, the backfill would
/// mislabel them, and there is no way to tell which is which after the fact.
///
/// So this fails when a second policy constructor appears, and the decision it
/// forces is what to do about the rows — not whether to keep defaulting.
#[test]
fn only_one_currency_policy_has_ever_existed() {
    // Follows the type: `PolicyId` moved to `fusiform-protocol` when the money
    // vocabulary was lifted onto the crate consumers can depend on. The fence
    // scans SOURCE, so its path is part of its subject — and it failed loudly
    // on the move rather than passing against a file that no longer mints
    // policies, which is the behaviour a source-scanning guard has to have.
    let src = include_str!("../../../fusiform-protocol/src/money.rs");
    let constructors: Vec<&str> = src
        .lines()
        .filter(|l| l.trim_start().starts_with("pub fn "))
        .filter(|l| l.contains("-> Self"))
        .collect();

    // Scoped to the PolicyId impl block, which is the only place a policy is
    // minted.
    let policies = currency_policies(src);
    let violations = extra_currency_policies(src);

    assert!(
        violations.is_empty(),
        "the legacy currency backfill requires one policy; extra policies: {violations:?}"
    );
    assert_eq!(
        policies.len(),
        1,
        "the pre-0.11.0 provenance backfill in route.rs labels every \
         provenance-less rate `models-dev-usd-v1`, which is only true while \
         one policy exists. A second policy means some legacy rows were \
         written under it and the backfill silently mislabels them. Decide \
         what happens to those rows before adding this. Found: {policies:#?}"
    );
    assert!(
        !constructors.is_empty(),
        "the detector found no constructors at all, so it would pass on an \
         empty file"
    );
}

fn currency_policies(src: &str) -> Vec<&str> {
    let impl_block = src
        .split_once("impl PolicyId {")
        .expect("PolicyId must have an impl block")
        .1
        .split_once("\n}")
        .expect("the impl block must close")
        .0;
    impl_block
        .lines()
        .filter(|l| l.trim_start().starts_with("pub fn "))
        .collect()
}

fn extra_currency_policies(src: &str) -> Vec<&str> {
    currency_policies(src).into_iter().skip(1).collect()
}

#[test]
fn planted_second_currency_policy_is_reported() {
    assert_eq!(extra_currency_policies("impl PolicyId {\n pub fn first() -> Self { todo!() }\n pub fn second() -> Self { todo!() }\n}"), [" pub fn second() -> Self { todo!() }"]);
}
