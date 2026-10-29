use super::*;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::BoundaryKind;
use fusiform_store::NewEra;
use serde_json::{json, Value};

fn fixture(
    canonical: Option<Value>,
) -> (
    CatalogStore,
    tempfile::TempDir,
    crate::canonical_links::Planted,
) {
    let links = crate::canonical_links::plant(
        &json!({"links": [{
            "provider_id": "ollama-cloud",
            "model_id": "kimi-k2.6",
            "canonical_id": "moonshotai/kimi-k2.6",
            "source_ref": "https://example.org/provider/kimi-k2.6",
            "quote": "The provider serves Kimi K2.6.",
            "lab_source_ref": "https://example.org/lab/kimi-k2.6",
            "lab_quote": "Kimi K2.6 is our model.",
            "established_by": "fusiform",
            "established_at_ms": 1,
            "review_by_ms": 5_184_000_001_i64
        }]})
        .to_string(),
    );
    assert_eq!(crate::canonical_links::links().len(), 1);
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("canonical.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    for model in crate::canonical_links::links()
        .values()
        .map(|row| row.model_id.as_str())
        .chain(["qwen3.5:397b", "kimi-k2.6-unverified"])
    {
        let mut facts = vec![
            ("existence", json!("present")),
            ("rate.input", json!({"state":"stated_zero"})),
        ];
        if let Some(value) = &canonical {
            facts.push(("model.canonical_id", value.clone()));
        }
        let eras: Vec<_> = facts
            .into_iter()
            .map(|(key, value)| NewEra {
                source: SourceId::ModelsDev,
                provider_id: "ollama-cloud".into(),
                model_id: model.into(),
                fact_key: FactKey::from_stored(key.into()),
                value_json: value.to_string(),
                boundary_at: Timestamp(1000),
                boundary_kind: BoundaryKind::Seed,
                observation_id: None,
            })
            .collect();
        store.append_eras(&eras).unwrap();
    }
    (store, dir, links)
}

fn get(store: &CatalogStore, args: Value) -> CatalogGetResponse {
    serve_catalog_get(store, &serde_json::to_vec(&args).unwrap()).unwrap()
}

#[test]
fn canonical_links_fill_only_missing_identities_on_current_reads() {
    for stored in [Some(Value::Null), None] {
        let (store, _dir, _links) = fixture(stored.clone());
        let eras_before = store.era_count(SourceId::ModelsDev).unwrap();
        let version_before = store.catalog_version().unwrap();
        for row in crate::canonical_links::links().values() {
            let id = format!("{}/{}", row.provider_id, row.model_id);
            for mut args in [
                json!({}),
                json!({"provider_id":"ollama-cloud"}),
                json!({"provider_id":"ollama-cloud","model_id":row.model_id}),
            ] {
                for prefixes in [
                    Value::Null,
                    json!(["model."]),
                    json!(["model.canonical_id"]),
                    json!(["rate."]),
                    json!(["limit."]),
                    json!([]),
                ] {
                    args["fact_prefixes"] = prefixes.clone();
                    let response = get(&store, args.clone());
                    let admits = prefixes.is_null()
                        || prefixes == json!(["model."])
                        || prefixes == json!(["model.canonical_id"]);
                    if admits {
                        assert_eq!(response.models[&id]["model.canonical_id"], row.canonical_id);
                        let disclosure = response
                            .canonical_links
                            .iter()
                            .find(|link| link.model == id)
                            .unwrap();
                        assert_eq!(disclosure.canonical_id, row.canonical_id);
                        assert_eq!(disclosure.source_ref, row.source_ref);
                        assert_eq!(disclosure.quote, row.quote);
                        assert_eq!(disclosure.lab_source_ref, row.lab_source_ref);
                        assert_eq!(disclosure.lab_quote, row.lab_quote);
                        assert_eq!(disclosure.review_by_ms, row.review_by_ms);
                        if prefixes == json!(["model."]) {
                            assert_eq!(
                                response.models[&id],
                                BTreeMap::from([
                                    ("model.canonical_id".into(), json!(row.canonical_id)),
                                    ("model.lineage".into(), json!("kimi")),
                                ])
                            );
                        } else if !prefixes.is_null() {
                            assert_eq!(response.models[&id].len(), 1);
                        }
                    } else {
                        assert!(response.canonical_links.is_empty());
                        assert!(response
                            .models
                            .values()
                            .all(|facts| !facts.contains_key("model.canonical_id")));
                    }
                }
            }
            let (plain, _) = store
                .read_model(SourceId::ModelsDev, &row.provider_id, &row.model_id, None)
                .unwrap();
            let plain = plain.unwrap();
            let raw = plain
                .facts
                .get(&FactKey::from_stored("model.canonical_id".into()));
            assert_eq!(
                raw.map(|raw| serde_json::from_str::<Value>(raw).unwrap()),
                stored
            );
        }
        let response = get(&store, json!({}));
        for unlinked in [
            "ollama-cloud/qwen3.5:397b",
            "ollama-cloud/kimi-k2.6-unverified",
        ] {
            assert_eq!(
                response.models[unlinked].get("model.canonical_id").cloned(),
                stored
            );
        }
        assert_eq!(store.era_count(SourceId::ModelsDev).unwrap(), eras_before);
        assert_eq!(store.catalog_version().unwrap(), version_before);
        let paged = get(
            &store,
            json!({"paged":true, "fact_prefixes":["model.canonical_id"]}),
        );
        assert_eq!(
            paged.canonical_links.len(),
            crate::canonical_links::links().len()
        );
        assert!(paged
            .canonical_links
            .iter()
            .all(|link| paged.models[&link.model]["model.canonical_id"] == link.canonical_id));
    }
}

#[test]
fn upstream_non_null_canonical_identity_wins_even_when_it_disagrees() {
    let (store, _dir, _links) = fixture(Some(json!("upstream/a-different-model")));
    for args in [
        json!({}),
        json!({"provider_id":"ollama-cloud","model_id":"kimi-k2.6","fact_prefixes":["model.canonical_id"]}),
    ] {
        let response = get(&store, args);
        assert!(response.canonical_links.is_empty());
        assert_eq!(
            response.models["ollama-cloud/kimi-k2.6"]["model.canonical_id"],
            "upstream/a-different-model"
        );
    }
}

#[test]
fn point_in_time_reads_do_not_apply_canonical_links() {
    let (store, _dir, _links) = fixture(Some(Value::Null));
    for args in [
        json!({"at_ms":1000}),
        json!({"provider_id":"ollama-cloud","model_id":"kimi-k2.6","at_ms":1000,"fact_prefixes":["model."]}),
    ] {
        let response = get(&store, args);
        assert!(response.canonical_links.is_empty());
        assert!(response.models["ollama-cloud/kimi-k2.6"]["model.canonical_id"].is_null());
    }
}
