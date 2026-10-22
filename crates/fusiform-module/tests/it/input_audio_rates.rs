//! `rate.input_audio`, served only where a provider's own pricing page bills
//! audio input per token.
//!
//! The rows are cut from the embedded seed rather than written here, so the
//! amounts are the upstream's own. The store is first seeded without the
//! input-audio evidence (what a store written before this fact existed
//! holds), then a poll is ingested with the shipped evidence, which is what
//! the first poll after deploying this change does.

use std::collections::BTreeMap;

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{
    normalize_models_dev, normalize_models_dev_with, BoundaryKind, ObservationOutcome, SourceId,
    Timestamp,
};
use fusiform_module::route::{serve_tool_call, ToolResponse};
use fusiform_store::ingest::{catalog_digest, plan_ingest};
use fusiform_store::{CatalogStore, NewObservation};

const SEED: &str = include_str!("../../data/models-dev-seed.json");

/// Rows cut from the embedded seed: an evidenced provider's row, the curated
/// alias's target, rows from providers with no evidence (ofox and
/// google-vertex publish the same `input_audio` number), and a google model
/// the shipped table excludes.
const ROWS: &[(&str, &str)] = &[
    ("google", "gemini-3.8-flash"),
    ("google", "gemini-4-argon"),
    ("ofox", "google/gemini-3.8-flash"),
    ("aihubmix", "gemini-3.8-flash"),
    ("google-vertex", "gemini-3.8-flash"),
    ("google", "gemini-3.1-flash-lite-preview"),
];

const SEEDED_AT: i64 = 1_000;
const POLLED_AT: i64 = 5_000;

fn doc() -> String {
    let seed: serde_json::Value = serde_json::from_str(SEED).unwrap();
    let mut out = serde_json::Map::new();
    for (provider, model) in ROWS {
        let row = seed[provider]["models"][model].clone();
        assert!(
            row.is_object(),
            "{provider}/{model} must be in the embedded seed"
        );
        out.entry(provider.to_string())
            .or_insert_with(|| serde_json::json!({"id": provider, "models": {}}))["models"]
            [model] = row;
    }
    serde_json::Value::Object(out).to_string()
}

fn observe(store: &CatalogStore, at: i64, outcome: ObservationOutcome) -> i64 {
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(at),
            outcome,
            normalized_hash: Some(format!("h{at}")),
            raw_hash: Some(format!("r{at}")),
            etag: None,
            duration_ms: Some(10),
            detail: None,
        })
        .unwrap()
}

/// A store seeded before the fact existed, then polled with the evidence.
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
    let doc = doc();
    observe(&store, SEEDED_AT, ObservationOutcome::Seeded);
    let before = normalize_models_dev(doc.as_bytes()).unwrap().catalog;
    let plan = plan_ingest(
        &store,
        &before,
        Timestamp(SEEDED_AT),
        BoundaryKind::Seed,
        None,
    )
    .unwrap();
    store.append_eras(&plan.eras).unwrap();

    let id = observe(
        &store,
        POLLED_AT,
        ObservationOutcome::Changed { snapshot_seq: 1 },
    );
    let after =
        normalize_models_dev_with(doc.as_bytes(), &fusiform_module::input_audio::evidence())
            .unwrap()
            .catalog;
    let plan = plan_ingest(
        &store,
        &after,
        Timestamp(POLLED_AT),
        BoundaryKind::Observed,
        Some(id),
    )
    .unwrap();
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

fn read(store: &CatalogStore, args: serde_json::Value) -> serde_json::Value {
    let body = serde_json::json!({"name": "catalog.get", "arguments": args}).to_string();
    match serve_tool_call(store, body.as_bytes()).expect("must serve") {
        ToolResponse::Catalog(c) => serde_json::to_value(c).unwrap(),
        other => panic!("expected a catalog, got {other:?}"),
    }
}

fn rates(
    store: &CatalogStore,
    provider: &str,
    model: &str,
    at_ms: Option<i64>,
) -> serde_json::Value {
    let mut args = serde_json::json!({
        "provider_id": provider, "model_id": model, "fact_prefixes": ["rate."],
    });
    if let Some(at) = at_ms {
        args["at_ms"] = at.into();
    }
    let response = read(store, args);
    response["models"][format!("{provider}/{model}")].clone()
}

/// An evidenced provider serves `rate.input_audio` on a `--rates` read, with
/// the published amount and the same value shape as `rate.input`.
#[test]
fn an_evidenced_provider_serves_input_audio_on_a_rate_read() {
    let (store, _dir) = store();
    for (model, units) in [
        ("gemini-3.8-flash", 750_000_000u64),
        ("gemini-4-argon", 2_000_000_000),
    ] {
        let facts = rates(&store, "google", model, None);
        let audio = &facts["rate.input_audio"];
        assert_eq!(audio["state"], "priced", "{model}: {facts}");
        assert_eq!(audio["units"], units, "{model}: {audio}");
        assert_eq!(audio["exponent"], 9, "{model}: {audio}");
        assert_eq!(audio["currency"], "USD", "{model}: {audio}");
        assert_eq!(
            audio["unit_provenance"], facts["rate.input"]["unit_provenance"],
            "{model}: the serve path attaches the same currency policy"
        );
        assert!(audio.get("inherited_from").is_none(), "{model}: {audio}");
        let shape = |v: &serde_json::Value| {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(shape(audio), shape(&facts["rate.input"]), "{model}");
    }
}

/// A provider without evidence, or a model its provider's table excludes,
/// serves no audio rate, even when it publishes an audio number, and keeps
/// every other rate.
#[test]
fn a_provider_without_evidence_serves_no_input_audio() {
    let (store, _dir) = store();
    for (provider, model) in [
        ("ofox", "google/gemini-3.8-flash"),
        ("aihubmix", "gemini-3.8-flash"),
        ("google-vertex", "gemini-3.8-flash"),
        ("google", "gemini-3.1-flash-lite-preview"),
    ] {
        assert!(
            !fusiform_module::input_audio::evidence().bills_input_audio_per_token(provider, model),
            "{provider} must not be in the shipped table for this test to mean anything"
        );
        let facts = rates(&store, provider, model, None);
        assert!(
            facts.get("rate.input").is_some(),
            "{provider}/{model}: {facts}"
        );
        assert!(
            facts.get("rate.input_audio").is_none(),
            "{provider}/{model}: {facts}"
        );
    }
}

/// A point-in-time read before the fact was first stored lacks it; one after
/// carries it. The fact is an observation with a history, not today's
/// curation replayed onto the past.
#[test]
fn a_point_in_time_read_before_the_fact_existed_lacks_it() {
    let (store, _dir) = store();
    let before = rates(&store, "google", "gemini-3.8-flash", Some(POLLED_AT - 1));
    assert!(before.get("rate.input").is_some(), "{before}");
    assert!(before.get("rate.input_audio").is_none(), "{before}");
    let after = rates(&store, "google", "gemini-3.8-flash", Some(POLLED_AT));
    assert_eq!(after["rate.input_audio"]["units"], 750_000_000, "{after}");
}

/// The Antigravity alias serves its target's audio rate like its other rates,
/// marked as borrowed through the alias.
#[test]
fn the_alias_carries_its_targets_input_audio_marked() {
    let (store, _dir) = store();
    let facts = rates(&store, "google", "antigravity-gemini-3.8-flash", None);
    let audio = &facts["rate.input_audio"];
    assert_eq!(audio["units"], 750_000_000, "{facts}");
    assert_eq!(audio["inherited_from"]["basis"], "alias", "{audio}");
    assert_eq!(audio["inherited_from"]["provider_id"], "google", "{audio}");
    assert_eq!(
        audio["inherited_from"]["model_id"], "gemini-3.8-flash",
        "{audio}"
    );
    assert_eq!(
        facts["rate.input"]["inherited_from"], audio["inherited_from"],
        "the audio rate is borrowed exactly as the input rate is"
    );
}

/// Ingesting the embedded seed with the shipped evidence over a store seeded
/// without it writes exactly one `rate.input_audio` era per evidenced model
/// that publishes `cost.input_audio`, and nothing for any other fact.
///
/// This is the first poll after deploy, at full size. The expected count is
/// computed from the raw seed JSON, not from the normalizer under test.
#[test]
fn the_first_poll_with_evidence_writes_only_the_new_audio_eras() {
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
    observe(&store, SEEDED_AT, ObservationOutcome::Seeded);
    let without = normalize_models_dev(SEED.as_bytes()).unwrap().catalog;
    let seeded = plan_ingest(
        &store,
        &without,
        Timestamp(SEEDED_AT),
        BoundaryKind::Seed,
        None,
    )
    .unwrap();
    store.append_eras(&seeded.eras).unwrap();

    let evidence = fusiform_module::input_audio::evidence();
    let with = normalize_models_dev_with(SEED.as_bytes(), &evidence)
        .unwrap()
        .catalog;
    assert_ne!(
        catalog_digest(&with),
        catalog_digest(&without),
        "the new facts must move the digest, or the first poll records Unchanged"
    );
    let id = observe(
        &store,
        POLLED_AT,
        ObservationOutcome::Changed { snapshot_seq: 1 },
    );
    let plan = plan_ingest(
        &store,
        &with,
        Timestamp(POLLED_AT),
        BoundaryKind::Observed,
        Some(id),
    )
    .unwrap();

    let seed: serde_json::Value = serde_json::from_str(SEED).unwrap();
    let mut expected = BTreeMap::new();
    for (provider, row) in fusiform_module::input_audio::sources() {
        for (model, published) in seed[provider]["models"].as_object().unwrap() {
            let excluded = row.excluded_models.iter().any(|m| &m.model_id == model);
            if published["cost"].get("input_audio").is_some() && !excluded {
                expected.insert((provider.clone(), model.clone()), ());
            }
        }
    }
    let written: BTreeMap<(String, String), ()> = plan
        .eras
        .iter()
        .map(|era| {
            assert_eq!(
                era.fact_key.as_str(),
                "rate.input_audio",
                "{}/{}: only the audio rate may open an era",
                era.provider_id,
                era.model_id
            );
            ((era.provider_id.clone(), era.model_id.clone()), ())
        })
        .collect();
    assert_eq!(written.len(), plan.eras.len(), "one era per model");
    assert_eq!(written, expected);
    assert_eq!(plan.new_models, 0);
    assert_eq!(plan.disappeared_models, 0);
    // The count a deploy should expect, pinned so a reviewer sees it move:
    // google's 14 rows publishing input_audio, less its one excluded model.
    assert_eq!(plan.eras.len(), 13, "{:?}", written.keys());
}
