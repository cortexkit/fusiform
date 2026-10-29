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

const FALLBACK: &str = "capability.server_side_fallback";
const OPUS: &str = "anthropic/claude-opus-5-5";
const HAIKU: &str = "anthropic/claude-haiku-5-5";
const SONNET: &str = "anthropic/claude-sonnet-5-5";
const OPUS_ALIAS: &str = "google/antigravity-claude-opus-5-5";

/// A store holding three of the embedded seed's anthropic rows.
///
/// models.dev has no server-side fallback field, so `planted` stands in for
/// the day it publishes one: it writes that fact for Opus 5.5 into the same
/// seed ingest, as a stored upstream era.
fn anthropic_store(planted: Option<Value>) -> (CatalogStore, tempfile::TempDir) {
    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    let mut upstream = json!({});
    for (provider, ids) in [
        (
            "anthropic",
            vec!["claude-opus-5-5", "claude-haiku-5-5", "claude-sonnet-5-5"],
        ),
        // Only so the Antigravity alias of Opus 5.5 has its provider.
        ("google", vec!["gemini-3.8-flash"]),
    ] {
        upstream[provider] = seed[provider].clone();
        upstream[provider]["models"] = json!({});
        for id in ids {
            let row = seed[provider]["models"][id].clone();
            assert!(row.is_object(), "the seed must carry {provider}/{id}");
            upstream[provider]["models"][id] = row;
        }
    }
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
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1_000),
            outcome: ObservationOutcome::Changed {
                snapshot_seq: 1_000,
            },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let catalog = normalize_models_dev(&serde_json::to_vec(&upstream).unwrap())
        .unwrap()
        .catalog;
    let mut plan =
        plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    assert!(
        plan.eras
            .iter()
            .all(|era| era.fact_key.as_str() != FALLBACK),
        "the normalizer now produces {FALLBACK}; the addition rows need review"
    );
    if let Some(value) = planted {
        plan.eras.push(fusiform_store::NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-opus-5-5".into(),
            fact_key: fusiform_store::FactKey::capability("server_side_fallback"),
            value_json: value.to_string(),
            boundary_at: Timestamp(1_000),
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        });
    }
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

#[test]
fn a_current_read_serves_the_documented_server_side_fallback() {
    let (store, _dir) = anthropic_store(None);
    for (label, response) in [
        (
            "named",
            get(
                &store,
                json!({"provider_id":"anthropic","model_id":"claude-opus-5-5"}),
            ),
        ),
        ("provider", get(&store, json!({"provider_id":"anthropic"}))),
        ("bulk", get(&store, json!({}))),
        ("paged", paged(&store, json!({}))),
        // The store holds no fact under this key, so a read filtered to it
        // must still reach the rows an addition applies to.
        (
            "filtered bulk",
            get(&store, json!({"fact_prefixes":[FALLBACK]})),
        ),
        (
            "filtered named",
            get(
                &store,
                json!({"provider_id":"anthropic","model_id":"claude-opus-5-5","fact_prefixes":[FALLBACK]}),
            ),
        ),
    ] {
        assert_eq!(response.models[OPUS][FALLBACK], json!(true), "{label}");
        let rows = disclosed(&response, OPUS);
        assert_eq!(rows.len(), 1, "{label}: {:?}", response.fact_overrides);
        let row = rows[0];
        assert_eq!(row.fact_key, FALLBACK, "{label}");
        assert_eq!(row.value, json!(true), "{label}");
        assert_eq!(
            row.replaces,
            Value::Null,
            "{label}: an addition replaces nothing"
        );
        assert!(row.source_ref.contains(
            "https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback"
        ));
        assert!(row.quote.contains("server-side-fallback-2026-07-01"));
        assert_eq!(row.review_by_ms - row.established_at_ms, 60 * 86_400_000);
        // The disclosure serializes `replaces` as an explicit null.
        let wire = serde_json::to_value(row).unwrap();
        assert!(
            wire.as_object().unwrap().contains_key("replaces"),
            "{label}"
        );
        assert_eq!(wire["replaces"], Value::Null, "{label}");

        // Named reads carry only Opus 5.5; every other read carries Haiku 5.5.
        if !label.starts_with("named") && !label.starts_with("filtered named") {
            assert_eq!(response.models[HAIKU][FALLBACK], json!(false), "{label}");
            assert_eq!(disclosed(&response, HAIKU).len(), 1, "{label}");
        }
        // Sonnet 5.5 has no row, so no value and no disclosure.
        if let Some(facts) = response.models.get(SONNET) {
            assert!(!facts.contains_key(FALLBACK), "{label}");
        }
        assert!(disclosed(&response, SONNET).is_empty(), "{label}");
        for row in &response.fact_overrides {
            assert_eq!(
                response.models[&row.model][&row.fact_key], row.value,
                "{label}"
            );
        }
    }
    // A filtered read serves only the key it asked for.
    let filtered = get(&store, json!({"fact_prefixes":[FALLBACK]}));
    assert!(filtered
        .models
        .values()
        .all(|facts| facts.keys().all(|key| key == FALLBACK)));
}

#[test]
fn a_haiku_read_serves_false_with_the_exclusion_quoted() {
    let (store, _dir) = anthropic_store(None);
    let response = get(
        &store,
        json!({"provider_id":"anthropic","model_id":"claude-haiku-5-5"}),
    );
    assert_eq!(response.models[HAIKU][FALLBACK], json!(false));
    let rows = disclosed(&response, HAIKU);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].replaces.is_null());
    assert!(rows[0]
        .quote
        .contains("Claude Haiku 5.5 has no server-side fallback"));
}

#[test]
fn an_at_ms_read_serves_no_server_side_fallback() {
    let (store, _dir) = anthropic_store(None);
    for response in [
        get(
            &store,
            json!({"provider_id":"anthropic","model_id":"claude-opus-5-5","at_ms":1_000}),
        ),
        get(&store, json!({"at_ms":1_000})),
        paged(&store, json!({"at_ms":1_000})),
    ] {
        for model in [OPUS, HAIKU] {
            if let Some(facts) = response.models.get(model) {
                assert!(!facts.contains_key(FALLBACK), "{model}");
                assert!(facts.contains_key("limit.context"), "{model} is served");
            }
        }
        assert!(response.models.contains_key(OPUS));
        assert!(response.fact_overrides.is_empty());
    }
}

#[test]
fn an_upstream_value_for_the_key_suppresses_the_addition() {
    // Any stored value wins, including one that disagrees with the row and a
    // null, which is upstream recording the key with no value.
    for published in [json!(false), json!(true), Value::Null] {
        let (store, _dir) = anthropic_store(Some(published.clone()));
        for args in [
            json!({"provider_id":"anthropic","model_id":"claude-opus-5-5"}),
            json!({"provider_id":"anthropic"}),
            json!({"fact_prefixes":[FALLBACK]}),
        ] {
            let response = get(&store, args.clone());
            assert_eq!(response.models[OPUS][FALLBACK], published, "{args}");
            assert!(disclosed(&response, OPUS).is_empty(), "{published} {args}");
            // Rows upstream still says nothing about keep their addition.
            if let Some(facts) = response.models.get(HAIKU) {
                assert_eq!(facts[FALLBACK], json!(false), "{args}");
            }
        }
    }
}

#[test]
fn the_antigravity_route_to_opus_does_not_carry_server_side_fallback() {
    // The route reaches Opus 5.5 through Google-managed Vertex, which the docs
    // do not list among the platforms with server-side fallback.
    let (store, _dir) = anthropic_store(None);
    for args in [
        json!({"provider_id":"google","model_id":"antigravity-claude-opus-5-5"}),
        json!({"provider_id":"google"}),
        json!({}),
    ] {
        let response = get(&store, args.clone());
        let facts = &response.models[OPUS_ALIAS];
        assert!(facts.contains_key("limit.context"), "{args}");
        assert!(!facts.contains_key(FALLBACK), "{args}");
        assert!(disclosed(&response, OPUS_ALIAS).is_empty(), "{args}");
    }
}

const STATUS: &str = "model.status";
const DAYBREAK_BLUE: &str = "openai/gpt-daybreak-blue-latest";
const DAYBREAK_RED: &str = "openai/gpt-daybreak-red-latest";
// A seed row with no status and no override, so the addition is shown to
// reach only the rows it names.
const DAYBREAK_CONTROL: &str = "openai/gpt-5.6-sol";

/// A store holding the two Daybreak aliases plus a control row, as the seed
/// publishes them. Ingest stores `model.status` on every row, as `null` when
/// models.dev publishes none. `planted` replaces Daybreak Blue's stored status
/// with one models.dev would have published.
fn daybreak_store(planted: Option<Value>) -> (CatalogStore, tempfile::TempDir) {
    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    let mut upstream = json!({});
    upstream["openai"] = seed["openai"].clone();
    upstream["openai"]["models"] = json!({});
    for id in [
        "gpt-daybreak-blue-latest",
        "gpt-daybreak-red-latest",
        "gpt-5.6-sol",
    ] {
        let row = seed["openai"]["models"][id].clone();
        assert!(row.is_object(), "the seed must carry openai/{id}");
        upstream["openai"]["models"][id] = row;
    }
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
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1_000),
            outcome: ObservationOutcome::Changed {
                snapshot_seq: 1_000,
            },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let catalog = normalize_models_dev(&serde_json::to_vec(&upstream).unwrap())
        .unwrap()
        .catalog;
    let mut plan =
        plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    for era in &mut plan.eras {
        if era.fact_key.as_str() == STATUS && era.model_id.starts_with("gpt-daybreak-") {
            assert_eq!(
                era.value_json, "null",
                "models.dev now publishes a status for {}; review the deprecation rows",
                era.model_id
            );
            if let (Some(value), "gpt-daybreak-blue-latest") = (&planted, era.model_id.as_str()) {
                era.value_json = value.to_string();
            }
        }
    }
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

#[test]
fn a_current_read_marks_the_vendor_deprecated_daybreak_aliases() {
    let (store, _dir) = daybreak_store(None);
    for (label, response) in [
        (
            "named",
            get(
                &store,
                json!({"provider_id":"openai","model_id":"gpt-daybreak-blue-latest"}),
            ),
        ),
        ("provider", get(&store, json!({"provider_id":"openai"}))),
        ("paged", paged(&store, json!({}))),
        // The store holds no status for these rows, so a read filtered to the
        // key must still reach them.
        ("filtered", get(&store, json!({"fact_prefixes":[STATUS]}))),
    ] {
        assert_eq!(
            response.models[DAYBREAK_BLUE][STATUS],
            json!("deprecated"),
            "{label}"
        );
        let rows = disclosed(&response, DAYBREAK_BLUE);
        assert_eq!(rows.len(), 1, "{label}: {:?}", response.fact_overrides);
        assert_eq!(rows[0].fact_key, STATUS, "{label}");
        assert_eq!(
            rows[0].replaces,
            Value::Null,
            "{label}: it replaces the stored null"
        );
        assert!(
            rows[0]
                .source_ref
                .contains("https://platform.openai.com/docs/models/gpt-daybreak-blue-latest"),
            "{label}"
        );
        if label != "named" {
            assert_eq!(
                response.models[DAYBREAK_RED][STATUS],
                json!("deprecated"),
                "{label}"
            );
        }
        // The control row has no override, so it keeps its stored null.
        if let Some(facts) = response.models.get(DAYBREAK_CONTROL) {
            assert_eq!(facts.get(STATUS), Some(&Value::Null), "{label}: {facts:?}");
        }
    }
}

#[test]
fn an_at_ms_read_serves_the_recorded_null_status() {
    let (store, _dir) = daybreak_store(None);
    let response = get(
        &store,
        json!({"provider_id":"openai","model_id":"gpt-daybreak-blue-latest","at_ms":1_000}),
    );
    assert_eq!(response.models[DAYBREAK_BLUE][STATUS], Value::Null);
    assert!(response.fact_overrides.is_empty());
}

#[test]
fn a_status_models_dev_publishes_wins_over_the_deprecation_row() {
    // Any published status stands, including one other than deprecated.
    for published in [json!("beta"), json!("deprecated")] {
        let (store, _dir) = daybreak_store(Some(published.clone()));
        let response = get(&store, json!({"provider_id":"openai"}));
        assert_eq!(response.models[DAYBREAK_BLUE][STATUS], published);
        assert!(
            disclosed(&response, DAYBREAK_BLUE).is_empty(),
            "{published}"
        );
        // Red still has no published status, so it keeps its addition.
        assert_eq!(response.models[DAYBREAK_RED][STATUS], json!("deprecated"));
    }
}
