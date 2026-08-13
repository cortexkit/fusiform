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

    let catalog = normalize_models_dev(UPSTREAM.as_bytes()).unwrap().catalog;
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
