//! Corrections replace the upstream's value on the current view, never on
//! history.
//!
//! The overlay records what a source says NOW. A cell carries `observed_at` and
//! no validity interval, so applying it to a past instant would state something
//! about a window nobody observed — a forged inference rather than a forged
//! observation, and just as unfalsifiable a month later.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::{serve_tool_call, ToolResponse};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};

/// Two real rows, cut from the live document rather than invented.
///
/// `claude-sonnet-4-5` carries the 1M/64k self-contradiction this correction
/// exists for; `claude-opus-5` carries the documented 1M/128k pairing and is
/// the control — a bug correcting every context limit would pass the first
/// assertion and fail here.
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

#[test]
fn a_current_read_serves_the_corrected_value_and_names_the_authority() {
    let (store, _dir) = store();
    let response = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5"}}"#,
    );

    assert_eq!(
        response["models"]["anthropic/claude-sonnet-4-5"]["limit.context"], 200_000,
        "the current view must serve Anthropic's number, not the catalog's 1M"
    );

    let entry = response["overridden"]
        .as_array()
        .expect("the override list must be present")
        .iter()
        .find(|o| o["fact_key"] == "limit.context")
        .expect("the override must be REPORTED, not applied silently");

    assert_eq!(entry["upstream_value"], "1000000");
    assert_eq!(entry["served_value"], "200000");
    assert!(
        entry["authority"]
            .as_str()
            .unwrap()
            .contains("docs.claude.com"),
        "the entry must name WHO says so: an operator asking 'says who' has to \
         answer from the line, or distrusting the correction is their cheapest \
         move. Got {:?}",
        entry["authority"]
    );
}

#[test]
fn a_point_in_time_read_serves_what_the_upstream_published() {
    let (store, _dir) = store();
    let response = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","at_ms":1500}}"#,
    );

    // The read INSIDE era coverage returned something, asserted before anything
    // is concluded from what it did not contain.
    //
    // SUBC's rider, from running the acceptance check on the shipped binary: my
    // suggested instant predated era coverage and returned ZERO MODELS, which
    // would have passed the override assertion below vacuously — an empty
    // result contains no override entry either. The value assertion happens to
    // protect this test by ordering, and protection by accident is not a
    // property anyone can rely on when the test is next edited.
    //
    // This is the absent-versus-unknown discipline the whole overlay rests on,
    // applied to a test's own inputs: "no correction was applied" and "there
    // was nothing to correct" are different facts, and only one of them is
    // being asserted.
    assert!(
        response["models"]
            .get("anthropic/claude-sonnet-4-5")
            .is_some(),
        "the point-in-time instant must fall INSIDE era coverage, or every \
         assertion below holds vacuously on an empty result"
    );

    assert_eq!(
        response["models"]["anthropic/claude-sonnet-4-5"]["limit.context"], 1_000_000,
        "history must report what models.dev published. The overlay says what \
         Anthropic's docs say TODAY and carries no validity interval, so \
         applying it to a past instant would claim something about a window \
         nobody observed."
    );

    let overridden = response["overridden"].as_array();
    assert!(
        overridden.is_none() || overridden.unwrap().is_empty(),
        "a point-in-time read overrides nothing, so the list must be absent \
         rather than reporting a correction that was not applied"
    );
}

#[test]
fn a_model_without_a_corrective_cell_is_untouched() {
    // The control. Without it, a bug correcting EVERY context limit to 200000
    // would pass the first test and read as success.
    let (store, _dir) = store();
    let response = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-opus-5"}}"#,
    );

    assert_eq!(
        response["models"]["anthropic/claude-opus-5"]["limit.context"], 1_000_000,
        "opus-5 passes the pairing test and Anthropic's doc names it in the 1M \
         list, so the catalog is right there and nothing may touch it"
    );
    let overridden = response["overridden"].as_array();
    assert!(
        overridden.is_none() || overridden.unwrap().is_empty(),
        "a model with no corrective cell must report no override"
    );
}

/// `catalog.status` reports the overrides in effect, not only `catalog.get`.
///
/// An operator asking "what is fusiform doing" is asking a different question
/// than "what are this model's facts", and the answer includes "deliberately
/// disagreeing with the upstream about these facts". Without this they learn it
/// only by happening to read one of the affected models.
///
/// Found by BROCA describing what they had built on their side and my checking
/// whether both of my surfaces did the same — the renderer went to `get`
/// because `get` is where the gap was found, and nothing asked whether the
/// other verb needed it.
#[test]
fn the_status_surface_reports_the_overrides_in_effect() {
    let (store, _dir) = store();
    let response = match serve_tool_call(
        &store,
        br#"{"name":"catalog.status","arguments":{"polls":3}}"#,
    )
    .expect("must serve")
    {
        ToolResponse::Status(s) => serde_json::to_value(s).unwrap(),
        other => panic!("expected a status, got {other:?}"),
    };

    let entry = response["overridden"]
        .as_array()
        .expect("status must carry the override list")
        .iter()
        .find(|o| o["model_id"] == "claude-sonnet-4-5")
        .expect("the sonnet-4.5 override must be reported on the status surface");

    assert_eq!(entry["upstream_value"], "1000000");
    assert_eq!(entry["served_value"], "200000");

    // And it must agree with what `catalog.get` says, because two surfaces
    // reporting one fact differently is worse than one surface reporting it.
    let from_get = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5"}}"#,
    );
    assert_eq!(
        from_get["overridden"][0]["served_value"], entry["served_value"],
        "status and get must not disagree about an override"
    );
}

/// A correction the upstream has already fixed is reported by neither surface.
///
/// The redundant-cell case, and it is the one that decides whether the override
/// block stays readable. If models.dev corrects `claude-sonnet-4-5` to 200,000
/// tomorrow, the cell becomes a no-op — and reporting it would print
/// `200000 -> 200000` on every read forever, which is the noise that trains an
/// operator to skip the block entirely.
///
/// Found by a surviving mutation: removing the equality check changed nothing,
/// because every fixture in this file disagrees with its correction. The
/// agreeing case had no coverage at all.
#[test]
fn a_correction_the_upstream_has_adopted_is_not_reported() {
    // The same upstream document, with the row already carrying Anthropic's
    // real number — which is exactly what a fixed upstream looks like.
    let fixed = UPSTREAM.replace(
        r#""limit": { "context": 1000000, "output": 64000 }"#,
        r#""limit": { "context": 200000, "output": 64000 }"#,
    );
    assert_ne!(fixed, UPSTREAM, "the fixture mutation must apply");

    let (store, _dir) = store_from(&fixed);

    let from_get = get(
        &store,
        r#"{"name":"catalog.get","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5"}}"#,
    );
    assert_eq!(
        from_get["models"]["anthropic/claude-sonnet-4-5"]["limit.context"], 200_000,
        "the value is right either way, which is why the report is the only \
         thing that can be wrong here"
    );
    let overridden = from_get["overridden"].as_array();
    assert!(
        overridden.is_none() || overridden.unwrap().is_empty(),
        "an override that changes nothing must not be reported: {overridden:?}"
    );

    let status = match serve_tool_call(
        &store,
        br#"{"name":"catalog.status","arguments":{"polls":1}}"#,
    )
    .expect("must serve")
    {
        ToolResponse::Status(s) => serde_json::to_value(s).unwrap(),
        other => panic!("expected a status, got {other:?}"),
    };
    let s_over = status["overridden"].as_array();
    assert!(
        s_over.is_none() || s_over.unwrap().is_empty(),
        "status must agree with get about a no-op override: {s_over:?}"
    );
}

/// A correction can only REPLACE a fact the store already holds, never create
/// one the upstream never published.
///
/// # Why this is a contract rather than an implementation detail
///
/// ASTRO's accounting derives from `rate.reasoning` PRESENT-OR-ABSENT: an
/// absent reasoning rate plus a reasoning-capable model means the reasoning
/// tokens are included in output, which covers 5,726 of their 5,833 models.
/// The absent arm is the dominant one.
///
/// So a correction that CREATED a `rate.reasoning` where the upstream published
/// none would flip models out of that arm — silently, because every value
/// involved is real and every row well formed. It would not look like a
/// pricing change. It would look like the upstream started charging.
///
/// The apply loop iterates the facts a model HAS, so an absent fact is never
/// reached and the property holds by construction. That is exactly why it needs
/// a test: a property held by construction is one refactor from being held by
/// nothing, and this one is invisible in a diff — inserting into the map rather
/// than iterating it reads as a fix for "corrections do not apply to new
/// models".
#[test]
fn a_correction_cannot_create_a_fact_the_upstream_never_published() {
    // A model with NO reasoning rate, which is the ordinary case upstream and
    // the arm ASTRO's mapping depends on.
    let (store, _dir) = store();

    let response = get(&store, r#"{"name":"catalog.get","arguments":{}}"#);
    let sonnet = &response["models"]["anthropic/claude-sonnet-4-5"];

    assert!(
        sonnet["limit.context"].is_i64(),
        "the fixture must reach the corrected fact, or this test proves nothing \
         about corrections"
    );
    assert_eq!(
        sonnet["limit.context"], 200_000,
        "the control: the correction under test must actually be applying"
    );

    // The upstream publishes no reasoning rate for this model, so no correction
    // may invent one.
    assert!(
        sonnet.get("rate.reasoning").is_none(),
        "a correction created rate.reasoning where the upstream published none. \
         ASTRO reads an ABSENT reasoning rate as 'reasoning included in output' \
         for 5,726 of 5,833 models, so inventing this fact moves models out of \
         that arm with every value looking legitimate. Corrections may replace \
         what the store holds; they may never add to it."
    );

    // Stated as a general property rather than one key, so a correction on any
    // unpublished fact fails here too.
    let served: Vec<&String> = sonnet.as_object().unwrap().keys().collect();
    for key in &served {
        assert!(
            !key.starts_with("rate.") || *key == "rate.input" || *key == "rate.output",
            "served fact {key} was not published by this fixture: a correction \
             has created a fact rather than replacing one"
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

/// Drive the real `catalog.history` route and read what it DISCLOSES.
///
/// # The production defect this pins
///
/// The first version of history's override disclosure did the correction
/// lookup by hand instead of through the function `catalog.get` uses, and got
/// both of that function's rules wrong at once. In production it printed:
///
/// ```text
/// NOTE: fusiform serves 200000 for this fact, not the  recorded below
/// ```
///
/// — one value where the sentence promises two, because it read
/// `Correction::upstream_value`, which is a SLOT filled from the served row
/// rather than a value the overlay carries. And it printed the same note for
/// `claude-opus-5`, whose overlay value AGREES with the catalog, where the
/// sentence is affirmatively false: nothing differs there.
///
/// The second is the worse half. A note on every history query destroys the
/// distinction the disclosure exists to draw.
///
/// # Why the test that passed could not have caught it
///
/// It rendered a HAND-BUILT response object with the fields already correct.
/// That tests the sentence and says nothing about whether the module ever
/// produces those fields — the two halves were verified separately and never
/// met. This drives the real route over a real store, so the values are
/// whatever the code actually computes.
#[test]
fn history_discloses_an_override_only_where_one_applies() {
    let (store, _dir) = store();

    // A second poll moving limit.context, so the fact has a history rather than
    // a single row. The disclosure must report the value in force NOW.
    // Edited through JSON rather than string replacement: BOTH anthropic models
    // in this fixture publish context 1000000, so a textual replace moves
    // opus-5 too — which makes its overlay cell genuinely differ and the
    // control below fire for a real reason. The control caught exactly that,
    // which is why this edit is scoped to one model.
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

    // The overridden fact.
    let overridden = history(
        r#"{"name":"catalog.history","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","fact_key":"limit.context"}}"#,
    );

    // TWO eras, deliberately. With one, `first()` and `last()` are the same row
    // and a mutation swapping them survives — the fixture cannot tell the
    // newest value from the oldest, so it certifies neither. This query orders
    // OLDEST-first, so the current value is the last, and reaching for `first`
    // is the natural mistake that would compare against a model's original
    // value forever.
    assert!(
        overridden["eras"].as_array().map(|e| e.len()).unwrap_or(0) >= 2,
        "the fixture must carry more than one era for this fact, or the \
         newest-versus-oldest distinction is untested"
    );
    let o = &overridden["overridden"];
    assert!(
        !o.is_null(),
        "history must disclose the override on a fact whose served value \
         differs from the record"
    );
    assert_eq!(
        o["served_value"].as_str(),
        Some("200000"),
        "the served value must be the one the catalog actually returns"
    );
    assert_eq!(
        o["upstream_value"].as_str(),
        Some("900000"),
        "the upstream value must come from the ROW, not from \
         Correction::upstream_value, which is an unfilled slot — reading it \
         yields an empty string and a sentence naming one value where it \
         promises two"
    );
    assert!(
        !o["authority"].as_str().unwrap_or_default().is_empty(),
        "an override without its authority asserts without a source"
    );

    // The eras themselves must be untouched: an override is a serve-time
    // judgment, and editing history to match would forge the record.
    let eras = overridden["eras"].as_array().expect("eras is an array");
    assert!(
        eras.iter().any(|e| e["value"] == 1_000_000) && eras.iter().any(|e| e["value"] == 900_000),
        "the recorded eras must still say what the upstream published, both \
         before and after it moved"
    );
    assert!(
        !eras.iter().any(|e| e["value"] == 200_000),
        "no era may carry the served value: the record is disclosed against, \
         never rewritten"
    );

    // THE CONTROL, and it is the arm that failed in production. This model
    // carries an overlay cell whose value AGREES with the catalog, so the
    // correction exists and must produce no disclosure.
    let agreeing = history(
        r#"{"name":"catalog.history","arguments":{"provider_id":"anthropic","model_id":"claude-opus-5","fact_key":"limit.context"}}"#,
    );
    assert!(
        agreeing["overridden"].is_null(),
        "a correction that changes nothing must not be disclosed: a note on \
         every response destroys the distinction the note exists to draw. Got: {}",
        agreeing["overridden"]
    );

    // And a fact with no correction at all.
    let untouched = history(
        r#"{"name":"catalog.history","arguments":{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","fact_key":"limit.output"}}"#,
    );
    assert!(
        untouched["overridden"].is_null(),
        "a fact with no correction must disclose nothing"
    );
}
