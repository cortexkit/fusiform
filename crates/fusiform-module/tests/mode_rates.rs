//! Mode rates (`rate.<class>.mode.<name>`) through the real serve path.
//!
//! A consumer prices a service tier before choosing it, so a named read filtered
//! to the rate plane must return a model's mode rates in the same shape as its
//! base rates, a curated alias must carry its target's mode rates marked as
//! borrowed, and no request byte that selects a mode may appear anywhere in
//! what is served.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::{serve_tool_call, ToolResponse};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};

/// One real alias from the shipped table, whose target this document gives a
/// `fast` mode. The target row is synthetic; the alias mapping is real, so the
/// test exercises the curated table rather than one written for it.
const ALIAS: (&str, &str) = ("google", "antigravity-gemini-3.8-flash");

const DOC: &str = r#"{
  "google": {
    "id": "google",
    "name": "Google (target row is synthetic)",
    "models": {
      "gemini-3.8-flash": {
        "id": "gemini-3.8-flash",
        "family": "gemini-flash",
        "open_weights": false,
        "limit": { "context": 1048576, "output": 65536 },
        "cost": { "input": 0.5, "output": 3, "cache_read": 0.05 },
        "experimental": {
          "modes": {
            "fast": {
              "cost": { "input": 1, "output": 6, "cache_read": 0.1 },
              "provider": {
                "body": { "service_tier": "priority", "speed": "fast" },
                "headers": { "x-mode-header": "fast-mode-2026-02-01" }
              }
            },
            "pro": {
              "provider": { "body": { "reasoning": { "mode": "pro" } } }
            }
          }
        }
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
    let catalog = normalize_models_dev(DOC.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

/// The whole served response for a named read, as JSON.
fn read(store: &CatalogStore, provider: &str, model: &str, prefixes: &str) -> serde_json::Value {
    let body = format!(
        r#"{{"name":"catalog.get","arguments":{{"provider_id":"{provider}","model_id":"{model}","fact_prefixes":{prefixes}}}}}"#
    );
    match serve_tool_call(store, body.as_bytes()).expect("must serve") {
        ToolResponse::Catalog(c) => serde_json::to_value(c).unwrap(),
        other => panic!("expected a catalog, got {other:?}"),
    }
}

/// A `rate.` read returns the mode rates beside the base rates, in the same
/// value shape, and nothing for a mode that publishes no price.
#[test]
fn a_rate_read_returns_mode_rates_shaped_like_base_rates() {
    let (store, _dir) = store();
    let response = read(&store, "google", "gemini-3.8-flash", r#"["rate."]"#);
    let facts = response["models"]["google/gemini-3.8-flash"]
        .as_object()
        .expect("the model is served");

    let mut keys: Vec<&str> = facts.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "rate.cache_read",
            "rate.cache_read.mode.fast",
            "rate.input",
            "rate.input.mode.fast",
            "rate.output",
            "rate.output.mode.fast",
        ],
        "the rate plane is the base rates plus the priced mode's rates; `pro` \
         publishes no cost and serves nothing"
    );

    let base = &facts["rate.input"];
    let fast = &facts["rate.input.mode.fast"];
    assert_eq!(fast["units"], 1_000_000_000);
    assert_eq!(base["units"], 500_000_000);
    // Same shape: every field the base rate carries at serve time, including
    // the currency provenance attached here rather than stored, is on the mode
    // rate too, and nothing else is.
    let shape = |v: &serde_json::Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(shape(fast), shape(base), "fast={fast} base={base}");
    assert!(
        fast.get("unit_provenance").is_some(),
        "the serve path must attach the currency provenance to a mode rate: {fast}"
    );
}

/// A curated alias carries its target's mode rates, each marked as borrowed
/// through the alias.
#[test]
fn an_alias_carries_its_targets_mode_rates_marked() {
    let (store, _dir) = store();
    let response = read(&store, ALIAS.0, ALIAS.1, r#"["rate."]"#);
    let facts = response["models"][format!("{}/{}", ALIAS.0, ALIAS.1)]
        .as_object()
        .unwrap_or_else(|| panic!("the alias must serve its target: {response}"));

    let fast = facts
        .get("rate.input.mode.fast")
        .unwrap_or_else(|| panic!("the alias must carry the target's mode rate: {facts:?}"));
    assert_eq!(fast["units"], 1_000_000_000);
    assert_eq!(fast["inherited_from"]["basis"], "alias", "{fast}");
    assert_eq!(fast["inherited_from"]["provider_id"], "google", "{fast}");
    assert_eq!(
        fast["inherited_from"]["model_id"], "gemini-3.8-flash",
        "{fast}"
    );
}

/// No request byte that selects a mode appears anywhere in a served response,
/// on the model itself or through its alias.
#[test]
fn a_modes_request_bytes_are_never_served() {
    let (store, _dir) = store();
    for (provider, model) in [("google", "gemini-3.8-flash"), ALIAS] {
        let served = read(&store, provider, model, "null").to_string();
        // Control: the read did serve the mode's price.
        assert!(
            served.contains("rate.input.mode.fast"),
            "{provider}/{model}: {served}"
        );
        for needle in [
            "service_tier",
            "speed",
            "x-mode-header",
            "fast-mode-2026-02-01",
            "priority",
        ] {
            assert!(
                !served.contains(needle),
                "{provider}/{model}: {needle:?} reached the wire: {served}"
            );
        }
    }
}
