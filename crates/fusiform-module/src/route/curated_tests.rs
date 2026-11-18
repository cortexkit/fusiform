use super::*;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::BoundaryKind;
use fusiform_store::NewEra;
use serde_json::{json, Value};

const ID: &str = "meta/muse-wonder-full-1.0";
const MODEL: &str = "muse-wonder-full-1.0";

fn store() -> (CatalogStore, tempfile::TempDir) {
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
    (store, dir)
}

fn era(model: &str, key: &str, value: Value) -> NewEra {
    NewEra {
        source: SourceId::ModelsDev,
        provider_id: "meta".into(),
        model_id: model.into(),
        fact_key: FactKey::from_stored(key.into()),
        value_json: value.to_string(),
        boundary_at: Timestamp(1000),
        boundary_kind: BoundaryKind::Seed,
        observation_id: None,
    }
}

fn get(store: &CatalogStore, args: Value) -> CatalogGetResponse {
    serve_catalog_get(store, &serde_json::to_vec(&args).unwrap()).unwrap()
}

#[test]
fn search_rows_serve_sourced_request_rates_on_named_provider_and_whole_reads() {
    let (store, _dir) = store();
    let whole = get(&store, json!({}));
    for (provider, model, name, amount) in [
        ("exa", "search", "Exa Search", Some((7, 0))),
        ("tavily", "search", "Tavily Search", Some((8, 0))),
        (
            "brave",
            "web-search",
            "Brave Search API (Web Search)",
            Some((5, 0)),
        ),
        ("deepseek", "search", "DeepSeek Search", Some((75, 2))),
        ("kimi-for-coding", "search", "Kimi Code Search", None),
        ("ollama-cloud", "web-search", "Ollama Web Search", None),
    ] {
        let id = format!("{provider}/{model}");
        let provider_read = get(&store, json!({"provider_id": provider}));
        let named = get(&store, json!({"provider_id": provider, "model_id": model}));
        for response in [&whole, &provider_read, &named] {
            let facts = response.models.get(&id).expect(&id);
            assert_eq!(facts["existence"], "present", "{id}");
            assert_eq!(facts["model.type"], "search", "{id}");
            assert_eq!(facts["model.name"], name, "{id}");
            let rates: BTreeMap<_, _> = facts
                .iter()
                .filter(|(key, _)| key.starts_with("rate."))
                .collect();
            match amount {
                Some((units, exponent)) => assert_eq!(
                    serde_json::to_value(rates).unwrap(),
                    json!({"rate.request.per_request": {
                        "state": "priced", "units": units, "exponent": exponent,
                        "currency": "USD", "unit_provenance": {"kind": "stated"},
                        "per": 1000, "increment": {"kind": "unknown"},
                        "minimum_quantity": {"kind": "unknown"}
                    }}),
                    "{id}"
                ),
                None => assert!(rates.is_empty(), "{id}: no per-search price is published"),
            }
            assert!(!facts.keys().any(|key| key.starts_with("wire.")
                || key.starts_with("capability.")
                || key == "limit.context"));
            let disclosure = response.curated.iter().find(|row| row.model == id).unwrap();
            assert_eq!(disclosure.auth_methods, ["apikey"]);
            assert_eq!(disclosure.review_by_ms, 1799884800000);
        }
        let api = get(
            &store,
            json!({"provider_id": provider, "model_id": model, "auth_method": "apikey"}),
        );
        let mut expected_api = named.models[&id].clone();
        if provider == "deepseek" {
            // An explicit API-key request already applies a provider-wide
            // cache-write rule. Without a token input rate it refuses the
            // cache price; the ordinary search card remains request-only.
            expected_api.insert(
                "rate.cache_write".into(),
                json!({"state": "unpriced", "reason": "not_established"}),
            );
        }
        assert_eq!(api.models[&id], expected_api, "{id}: apikey");
        for method in ["oauth", "chatgpt", "antigravity"] {
            let result = serve_catalog_get(
                &store,
                &serde_json::to_vec(
                    &json!({"provider_id": provider, "model_id": model, "auth_method": method}),
                )
                .unwrap(),
            );
            match result {
                Ok(response) => assert!(!response.models.contains_key(&id), "{id}: {method}"),
                Err(error) => assert_eq!(error.code, "no_coverage", "{id}: {method}"),
            }
        }
    }
}

#[test]
fn plain_and_oauth_reads_serve_published_curated_facts() {
    let (store, _dir) = store();
    for args in [
        json!({}),
        json!({"provider_id":"meta"}),
        json!({"provider_id":"meta", "model_id":MODEL}),
        json!({"provider_id":"meta", "auth_method":"oauth"}),
        json!({"provider_id":"meta", "model_id":MODEL, "auth_method":"oauth"}),
    ] {
        let response = get(&store, args);
        let facts = &response.models[ID];
        assert_eq!(facts["model.canonical_id"], "meta/muse-wonder-full");
        assert_eq!(facts["limit.context"], 1048576);
        assert_eq!(facts["limit.output"], 131072);
        assert_eq!(facts["model.family"], "muse");
        assert_eq!(facts["model.open_weights"], false);
        for key in [
            "capability.attachment",
            "capability.reasoning",
            "capability.tool_call",
        ] {
            assert_eq!(facts[key], true);
        }
        assert!(!facts.keys().any(|key| key.starts_with("rate.")));
    }
}

#[test]
fn unlisted_login_method_never_serves_a_curated_row() {
    let (store, _dir) = store();
    store
        .append_eras(&[era("ordinary", "existence", json!("present"))])
        .unwrap();
    for args in [
        json!({"provider_id":"meta", "auth_method":"apikey"}),
        json!({"provider_id":"meta", "model_id":MODEL, "auth_method":"apikey"}),
    ] {
        let response = get(&store, args);
        assert!(!response.models.contains_key(ID));
        assert!(response.curated.is_empty());
    }
    assert!(get(
        &store,
        json!({"provider_id":"meta", "auth_method":"apikey"})
    )
    .models
    .contains_key("meta/ordinary"));
}

#[test]
fn a_present_upstream_row_supersedes_curated_facts_and_a_withdrawn_one_gives_way() {
    let (store, _dir) = store();
    for state in ["present", "absent"] {
        let mut existence = era(MODEL, "existence", json!(state));
        existence.boundary_at = Timestamp(if state == "present" { 1000 } else { 2000 });
        let mut limit = era(MODEL, "limit.context", json!(42));
        limit.boundary_at = existence.boundary_at;
        store.append_eras(&[existence, limit]).unwrap();
        for args in [
            json!({"provider_id":"meta"}),
            json!({"provider_id":"meta", "model_id":MODEL}),
        ] {
            let response = get(&store, args);
            if state == "present" {
                // Upstream currently publishes the model, so it wins.
                assert!(response.curated.is_empty());
                assert_eq!(response.models[ID]["limit.context"], 42);
                assert!(!response.models[ID].contains_key("model.canonical_id"));
            } else {
                // Upstream withdrew it: the curated row is served whole.
                assert_eq!(response.curated.len(), 1);
                assert_eq!(response.models[ID]["limit.context"], 1048576);
                assert_eq!(
                    response.models[ID]["model.canonical_id"],
                    "meta/muse-wonder-full"
                );
                assert!(response.retired.is_empty());
            }
        }
    }
}

#[test]
fn every_served_curated_row_discloses_its_allowlist_and_source() {
    let (store, _dir) = store();
    for args in [
        json!({}),
        json!({"provider_id":"meta", "model_id":MODEL}),
        json!({"provider_id":"meta", "auth_method":"oauth"}),
    ] {
        let response = get(&store, args);
        assert!(response.models.contains_key(ID));
        // One disclosure per served curated row: the whole-catalog read
        // serves every shipped row, the meta reads only this one.
        assert_eq!(
            response.curated.len(),
            response
                .models
                .keys()
                .filter(|id| crate::curated_models::models().contains_key(
                    &id.split_once('/')
                        .map(|(p, m)| (p.to_string(), m.to_string()))
                        .unwrap()
                ))
                .count()
        );
        let row = response
            .curated
            .iter()
            .find(|row| row.model == ID)
            .expect("the served row is disclosed");
        assert_eq!(row.model, ID);
        assert_eq!(row.auth_methods, ["oauth"]);
        assert!(row.source_ref.contains("16:42:35Z"));
        assert!(row.source_ref.contains("16:42:45Z"));
        assert!(row.source_ref.contains("https://models.dev/models.json"));
        assert!(row.source_ref.contains("Output limit 131072"));
        assert_eq!(row.review_by_ms, 1798761600000);
    }
}

#[test]
fn curated_rows_respect_fact_and_provider_filters() {
    let (store, _dir) = store();
    store
        .append_eras(&[era("ordinary", "existence", json!("present"))])
        .unwrap();
    for method in [None, Some("oauth")] {
        let mut args = json!({"provider_id":"meta", "fact_prefixes":["capability."]});
        if let Some(method) = method {
            args["auth_method"] = method.into();
        }
        let response = get(&store, args.clone());
        // The shipped row's capability facts: attachment, reasoning,
        // reasoning_options and tool_call.
        assert_eq!(response.models[ID].len(), 4);
        assert!(response.models[ID]
            .keys()
            .all(|key| key.starts_with("capability.")));
        assert_eq!(response.curated.len(), 1);
        args["fact_prefixes"] = json!(["rate."]);
        let response = get(&store, args.clone());
        assert!(!response.models.contains_key(ID));
        assert!(response.curated.is_empty());
        args["model_id"] = MODEL.into();
        let response = get(&store, args);
        assert!(response.models[ID].is_empty());
        assert_eq!(response.curated.len(), 1);
    }
    let response = get(&store, json!({"provider_id":"meta", "model_id":"ordinary"}));
    assert!(!response.models.contains_key(ID));
    assert!(response.curated.is_empty());
}

/// A point-in-time read serves curated rows only from the store's recorded
/// history of the curated file, never from the compiled file: with no
/// history recorded, no curated row is served at any instant.
#[test]
fn point_in_time_reads_serve_no_unrecorded_curated_row() {
    let (store, _dir) = store();
    store
        .append_eras(&[era("ordinary", "existence", json!("present"))])
        .unwrap();
    for args in [
        json!({"at_ms":1000}),
        json!({"provider_id":"meta", "at_ms":1000}),
    ] {
        let response = get(&store, args);
        assert!(!response.models.contains_key(ID));
        assert!(response.curated.is_empty());
    }
    assert!(serve_catalog_get(
        &store,
        &serde_json::to_vec(&json!({"provider_id":"meta", "model_id":MODEL, "at_ms":1000}))
            .unwrap()
    )
    .is_err());
}

#[test]
fn curated_history_names_the_source_without_storing_eras_or_changing_status() {
    let (store, _dir) = store();
    let before = serve_status(&store, b"{}").unwrap();
    let version = store.catalog_version().unwrap();
    let _ = get(&store, json!({"provider_id":"meta", "auth_method":"oauth"}));
    let response = serve_history(
        &store,
        &serde_json::to_vec(
            &json!({"provider_id":"meta", "model_id":MODEL, "fact_key":"limit.context"}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(response.eras.is_empty());
    assert!(response
        .curated
        .unwrap()
        .source_ref
        .contains("operator's Meta coding-plan login"));
    let after = serve_status(&store, b"{}").unwrap();
    assert_eq!(after.model_count, before.model_count);
    assert_eq!(after.models_priced, before.models_priced);
    assert_eq!(after.era_count, before.era_count);
    assert_eq!(store.catalog_version().unwrap(), version);
}
