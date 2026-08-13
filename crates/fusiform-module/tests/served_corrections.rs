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
