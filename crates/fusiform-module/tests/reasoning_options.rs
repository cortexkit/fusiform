//! `capability.reasoning_options` reaches the wire exactly as models.dev
//! publishes it.
//!
//! A consumer maps its own reasoning levels onto this list and refuses any
//! setting the model does not name, so every shape the upstream uses has to
//! arrive intact through the real serve path: an absent key, an explicit empty
//! list, and a populated list carrying entry types and elements fusiform itself
//! does not interpret.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::{serve_tool_call, ToolResponse};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};

/// The populated list, as text, so the expected value below is parsed from the
/// same bytes the upstream document carries rather than restated by hand.
///
/// Each element is a shape measured on the live payload: an effort entry whose
/// `values` holds a `null` (published that way for `sarvam/sarvam-30b` and
/// `sarvam/sarvam-105b`), a toggle entry with only `type`, and a budget entry
/// carrying `min` without `max`. The last entry has a `type` no consumer knows
/// yet, which must pass through rather than be dropped.
const POPULATED: &str = r#"[
  { "type": "effort", "values": [null, "low", "medium", "high"] },
  { "type": "toggle" },
  { "type": "budget_tokens", "min": 1024 },
  { "type": "not_yet_invented", "shape": { "anything": [1, 2] } }
]"#;

fn upstream() -> String {
    format!(
        r#"{{
  "sarvam": {{
    "id": "sarvam",
    "name": "Sarvam",
    "models": {{
      "absent": {{
        "id": "absent",
        "reasoning": true,
        "limit": {{ "context": 128000, "output": 8192 }}
      }},
      "empty": {{
        "id": "empty",
        "reasoning": false,
        "reasoning_options": [],
        "limit": {{ "context": 128000, "output": 8192 }}
      }},
      "populated": {{
        "id": "populated",
        "reasoning": true,
        "reasoning_options": {POPULATED},
        "limit": {{ "context": 128000, "output": 8192 }}
      }}
    }}
  }}
}}"#
    )
}

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
    let catalog = normalize_models_dev(upstream().as_bytes()).unwrap().catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

/// The served `capability.reasoning_options` for one model, or a panic naming
/// the model if the key is missing from the response entirely.
fn served(store: &CatalogStore, model: &str) -> serde_json::Value {
    let body = format!(
        r#"{{"name":"catalog.get","arguments":{{"provider_id":"sarvam","model_id":"{model}","fact_prefixes":["capability."]}}}}"#
    );
    let response = match serve_tool_call(store, body.as_bytes()).expect("must serve") {
        ToolResponse::Catalog(c) => serde_json::to_value(c).unwrap(),
        other => panic!("expected a catalog, got {other:?}"),
    };
    response["models"][format!("sarvam/{model}")]
        .get("capability.reasoning_options")
        .unwrap_or_else(|| panic!("sarvam/{model} must carry the key even when it is null"))
        .clone()
}

/// Absent, `[]`, and a populated list are three different upstream claims and
/// are served as three different values.
///
/// `null` means the upstream said nothing; `[]` means it stated the model
/// takes no options. A consumer that received `[]` for an absent key would
/// refuse every setting for a model nobody described, and one that received
/// `null` for `[]` would lose a stated fact.
#[test]
fn absent_empty_and_populated_are_served_distinctly() {
    let (store, _dir) = store();

    let absent = served(&store, "absent");
    let empty = served(&store, "empty");
    let populated = served(&store, "populated");

    assert_eq!(absent, serde_json::Value::Null, "an absent key serves null");
    assert_eq!(empty, serde_json::json!([]), "a published [] serves []");
    assert!(
        populated.as_array().is_some_and(|a| !a.is_empty()),
        "a populated list serves a non-empty array: {populated}"
    );
    assert_ne!(absent, empty);
    assert_ne!(empty, populated);
    assert_ne!(absent, populated);
}

/// The populated list comes back equal to what the upstream published, element
/// by element and in order, including the `null` inside `values` and the entry
/// type nothing in fusiform recognises.
#[test]
fn a_populated_list_round_trips_verbatim() {
    let (store, _dir) = store();

    let expected: serde_json::Value = serde_json::from_str(POPULATED).unwrap();
    let got = served(&store, "populated");

    assert_eq!(
        got, expected,
        "the served list must be the upstream's list unchanged"
    );
    // Stated separately because they are the two things a typed parser would
    // most plausibly lose, and a mismatch reported only as two whole arrays
    // hides which one went.
    assert_eq!(
        got[0]["values"][0],
        serde_json::Value::Null,
        "a null inside `values` must survive"
    );
    assert_eq!(
        got[3]["type"], "not_yet_invented",
        "an unknown entry type must survive"
    );
    assert!(
        got[2].get("max").is_none(),
        "a budget entry published without `max` must not gain one: {}",
        got[2]
    );
}
