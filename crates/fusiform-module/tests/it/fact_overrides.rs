//! Sourced fact overrides on served catalog reads.
//!
//! The shipped row serves `limit.output` 65,536 for google/gemini-4-argon in
//! place of models.dev's 32,768. These tests drive the real serve path over a
//! store seeded from the embedded models.dev copy, which publishes 32,768.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::{serve_catalog_get, serve_history, CatalogGetResponse};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};
use serde_json::{json, Value};

const TARGET: &str = "google/gemini-4-argon";
const ALIAS: &str = "google/antigravity-gemini-4-argon";
const CONTROL: &str = "google/gemini-3.8-flash";

/// The embedded seed's google entry cut to the overridden model and one
/// control, with Argon's output limit set to `output`.
fn upstream(output: Value) -> Vec<u8> {
    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    let mut google = seed["google"].clone();
    google["models"] = json!({});
    for id in ["gemini-4-argon", "gemini-3.8-flash"] {
        let row = seed["google"]["models"][id].clone();
        assert!(row.is_object(), "the seed must carry google/{id}");
        google["models"][id] = row;
    }
    assert_eq!(
        google["models"]["gemini-4-argon"]["limit"]["output"],
        json!(32_768),
        "the embedded seed must publish the value the override replaces"
    );
    google["models"]["gemini-4-argon"]["limit"]["output"] = output;
    serde_json::to_vec(&json!({ "google": google })).unwrap()
}

fn ingest(store: &CatalogStore, bytes: &[u8], at: i64) {
    let observation = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(at),
            outcome: ObservationOutcome::Changed { snapshot_seq: at },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let catalog = normalize_models_dev(bytes).unwrap().catalog;
    let (kind, observation) = if at == 1_000 {
        (BoundaryKind::Seed, None)
    } else {
        (BoundaryKind::Observed, Some(observation))
    };
    let plan = plan_ingest(store, &catalog, Timestamp(at), kind, observation).unwrap();
    store.append_eras(&plan.eras).unwrap();
}

fn store_publishing(output: Value) -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    ingest(&store, &upstream(output), 1_000);
    (store, dir)
}

fn get(store: &CatalogStore, args: Value) -> CatalogGetResponse {
    serve_catalog_get(store, &serde_json::to_vec(&args).unwrap()).unwrap()
}

/// Every page of a paged read, merged.
fn paged(store: &CatalogStore, mut args: Value) -> CatalogGetResponse {
    args["paged"] = json!(true);
    let mut merged = get(store, args.clone());
    while let Some(token) = merged.next_page_token.take() {
        args["page_token"] = json!(token);
        let page = get(store, args.clone());
        merged.models.extend(page.models);
        merged.fact_overrides.extend(page.fact_overrides);
        merged.next_page_token = page.next_page_token;
    }
    merged
}

fn disclosed<'a>(
    response: &'a CatalogGetResponse,
    model: &str,
) -> Vec<&'a fusiform_protocol::FactOverrideWire> {
    response
        .fact_overrides
        .iter()
        .filter(|row| row.model == model)
        .collect()
}

fn assert_overridden(response: &CatalogGetResponse, model: &str, label: &str) {
    assert_eq!(
        response.models[model]["limit.output"],
        json!(65_536),
        "{label}: {model}"
    );
    let rows = disclosed(response, model);
    assert_eq!(
        rows.len(),
        1,
        "{label}: {model} {:?}",
        response.fact_overrides
    );
    let row = rows[0];
    assert_eq!(
        (row.provider_id.as_str(), row.model_id.as_str()),
        ("google", "gemini-4-argon"),
        "{label}: the disclosure names the row the override is declared for"
    );
    assert_eq!(row.fact_key, "limit.output");
    assert_eq!(row.value, json!(65_536));
    assert_eq!(row.replaces, json!(32_768));
    assert!(row
        .source_ref
        .contains("https://ai.google.dev/gemini-api/docs/generate-content/latest-model"));
    assert!(row
        .quote
        .contains("One streaming request returns up to 65,536 tokens."));
    assert_eq!(row.established_by, "fusiform");
    assert_eq!(row.review_by_ms - row.established_at_ms, 60 * 86_400_000);
}

#[test]
fn a_current_read_serves_the_override_and_discloses_it() {
    let (store, _dir) = store_publishing(json!(32_768));
    for (label, response) in [
        (
            "named",
            get(
                &store,
                json!({"provider_id":"google","model_id":"gemini-4-argon"}),
            ),
        ),
        ("provider", get(&store, json!({"provider_id":"google"}))),
        ("bulk", get(&store, json!({}))),
        (
            "limits",
            get(&store, json!({"fact_prefixes":["limit.output"]})),
        ),
        ("paged", paged(&store, json!({}))),
        (
            "apikey plane",
            get(
                &store,
                json!({"provider_id":"google","model_id":"gemini-4-argon","auth_method":"apikey"}),
            ),
        ),
    ] {
        assert_overridden(&response, TARGET, label);
        // Nothing else is touched: the control model gets no disclosure.
        assert!(disclosed(&response, CONTROL).is_empty(), "{label}");
        // Each disclosure describes a value the response actually serves.
        for row in &response.fact_overrides {
            assert_eq!(
                response.models[&row.model][&row.fact_key], row.value,
                "{label}"
            );
        }
    }
}

#[test]
fn the_antigravity_alias_serves_the_overridden_value() {
    let (store, _dir) = store_publishing(json!(32_768));
    for (label, response) in [
        (
            "named alias",
            get(
                &store,
                json!({"provider_id":"google","model_id":"antigravity-gemini-4-argon"}),
            ),
        ),
        (
            "named alias, limits",
            get(
                &store,
                json!({"provider_id":"google","model_id":"antigravity-gemini-4-argon","fact_prefixes":["limit."]}),
            ),
        ),
        ("provider", get(&store, json!({"provider_id":"google"}))),
        ("bulk", get(&store, json!({}))),
        ("paged", paged(&store, json!({}))),
        (
            "apikey plane",
            get(
                &store,
                json!({"provider_id":"google","model_id":"antigravity-gemini-4-argon","auth_method":"apikey"}),
            ),
        ),
    ] {
        assert_overridden(&response, ALIAS, label);
        assert!(
            response.aliased.iter().any(|row| row.model == ALIAS),
            "{label}: the entry is served as an alias"
        );
    }
    // The named alias read carries only the alias entry, so its one
    // disclosure is re-keyed to it rather than left naming the target.
    let named = get(
        &store,
        json!({"provider_id":"google","model_id":"antigravity-gemini-4-argon"}),
    );
    assert_eq!(named.fact_overrides.len(), 1);
}

#[test]
fn a_filter_excluding_the_fact_serves_and_discloses_nothing() {
    let (store, _dir) = store_publishing(json!(32_768));
    for args in [
        json!({"fact_prefixes":["rate."]}),
        json!({"provider_id":"google","model_id":"gemini-4-argon","fact_prefixes":["rate."]}),
        json!({"provider_id":"google","model_id":"antigravity-gemini-4-argon","fact_prefixes":["limit.context"]}),
        // The plane view widens its intermediate filter, then narrows to this.
        json!({"provider_id":"google","model_id":"gemini-4-argon","auth_method":"apikey","fact_prefixes":["rate."]}),
    ] {
        let response = get(&store, args.clone());
        assert!(response.fact_overrides.is_empty(), "{args}");
        assert!(
            response
                .models
                .values()
                .all(|facts| !facts.contains_key("limit.output")),
            "{args}"
        );
    }
}

#[test]
fn an_at_ms_read_serves_the_stored_value_without_disclosure() {
    let (store, _dir) = store_publishing(json!(32_768));
    for response in [
        get(
            &store,
            json!({"provider_id":"google","model_id":"gemini-4-argon","at_ms":1_000}),
        ),
        get(&store, json!({"at_ms":1_000})),
        paged(&store, json!({"at_ms":1_000})),
    ] {
        assert_eq!(response.models[TARGET]["limit.output"], json!(32_768));
        assert!(response.fact_overrides.is_empty());
    }
    // Nothing was stored: the fact's history holds only the upstream value.
    let history = serve_history(
        &store,
        &serde_json::to_vec(
            &json!({"provider_id":"google","model_id":"gemini-4-argon","fact_key":"limit.output"}),
        )
        .unwrap(),
    )
    .unwrap();
    let values: Vec<_> = history.eras.iter().map(|era| era.value.clone()).collect();
    assert_eq!(values, [json!(32_768)]);
}

#[test]
fn an_upstream_value_other_than_replaces_is_served_as_published() {
    // Upstream already publishes something else, including the override's own
    // value: the row does not apply and nothing is disclosed.
    for published in [json!(49_152), json!(65_536), json!(1_048_576)] {
        let (store, _dir) = store_publishing(published.clone());
        for args in [
            json!({"provider_id":"google","model_id":"gemini-4-argon"}),
            json!({"provider_id":"google","model_id":"antigravity-gemini-4-argon"}),
            json!({}),
        ] {
            let response = get(&store, args.clone());
            assert!(response.fact_overrides.is_empty(), "{published} {args}");
            for model in [TARGET, ALIAS] {
                if let Some(facts) = response.models.get(model) {
                    assert_eq!(facts["limit.output"], published, "{model} {args}");
                }
            }
        }
    }

    // Upstream moves after the override shipped: the current read serves the
    // new upstream value, while the past instant still serves what was
    // recorded then, undisclosed either way.
    let (store, _dir) = store_publishing(json!(32_768));
    ingest(&store, &upstream(json!(49_152)), 2_000);
    let now = get(
        &store,
        json!({"provider_id":"google","model_id":"gemini-4-argon"}),
    );
    assert_eq!(now.models[TARGET]["limit.output"], json!(49_152));
    assert!(now.fact_overrides.is_empty());
    let then = get(
        &store,
        json!({"provider_id":"google","model_id":"gemini-4-argon","at_ms":1_500}),
    );
    assert_eq!(then.models[TARGET]["limit.output"], json!(32_768));
    assert!(then.fact_overrides.is_empty());
}
