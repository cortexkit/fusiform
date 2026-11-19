use super::*;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, ObservationOutcome, TokenClass};
use fusiform_store::{NewCuratedModel, NewEra, NewObservation};
use serde_json::{json, Value};

fn fixture() -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_str().unwrap().into(),
        },
    })
    .unwrap();
    let observation = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1000),
            outcome: ObservationOutcome::Seeded,
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    store
        .advance_catalog_version(observation, Timestamp(1000))
        .unwrap();
    (store, dir)
}

fn era(provider: &str, key: &str, value: Value, at: i64) -> NewEra {
    NewEra {
        source: SourceId::ModelsDev,
        provider_id: provider.into(),
        model_id: "model".into(),
        fact_key: FactKey::from_stored(key.into()),
        value_json: value.to_string(),
        boundary_at: Timestamp(at),
        boundary_kind: if at == 1000 {
            BoundaryKind::Seed
        } else {
            BoundaryKind::Observed
        },
        observation_id: None,
    }
}

fn get(store: &CatalogStore, args: Value) -> CatalogGetResponse {
    serve_catalog_get(store, &serde_json::to_vec(&args).unwrap()).unwrap()
}

#[test]
fn decommission_projection_follows_the_canonical_key() {
    let (store, _dir) = fixture();
    store
        .append_eras(&[
            era("reseller", "existence", json!("present"), 1000),
            era("reseller", "model.canonical_id", json!("lab/model"), 1000),
        ])
        .unwrap();
    store
        .append_model_mark("lab/model", true, "operator", "test veto", 1200)
        .unwrap();

    for at in [None, Some(1500)] {
        let mut args = json!({"provider_id":"reseller", "model_id":"model", "at_ms":at});
        let full = get(&store, args.clone());
        assert_eq!(
            full.models["reseller/model"]["quality.decommissioned"],
            true
        );
        args["fact_prefixes"] = json!(["quality.decommissioned"]);
        let filtered = get(&store, args);
        assert_eq!(
            filtered.models["reseller/model"],
            BTreeMap::from([("quality.decommissioned".into(), json!(true))]),
            "at_ms={at:?}"
        );
    }
}

#[test]
fn narrow_chat_scores_still_respect_the_row_type() {
    let (store, _dir) = fixture();
    store
        .append_quality_grade(
            "lab/model",
            None,
            Some(&fusiform_store::QualityGrade::Chat {
                elo: 1200.into(),
                eq: 1300.into(),
            }),
            "operator",
            "test grade",
            1200,
        )
        .unwrap();
    store
        .append_eras(&[
            era("chat", "existence", json!("present"), 1000),
            era("chat", "model.canonical_id", json!("lab/model"), 1000),
            era("search", "existence", json!("present"), 1000),
            era("search", "model.canonical_id", json!("lab/model"), 1000),
            era("search", "model.type", json!("search"), 1000),
        ])
        .unwrap();
    for at in [None, Some(1500)] {
        for key in ["quality.elo", "quality.eq"] {
            let mut args = json!({"provider_id":"search", "model_id":"model", "at_ms":at});
            let full = get(&store, args.clone());
            assert!(!full.models["search/model"].contains_key(key));
            args["fact_prefixes"] = json!([key]);
            let filtered = get(&store, args.clone());
            assert!(filtered.models["search/model"].is_empty(), "at_ms={at:?}");
            args["provider_id"] = json!("chat");
            let chat = get(&store, args);
            assert!(chat.models["chat/model"].contains_key(key), "at_ms={at:?}");
        }
    }
}

#[test]
fn dated_pages_keep_trailing_historical_curated_providers() {
    let (store, _dir) = fixture();
    store
        .append_eras(&[era("upstream", "existence", json!("present"), 1000)])
        .unwrap();
    let row = NewCuratedModel {
        provider_id: "zzzz-curated-only".into(),
        model_id: "model".into(),
        facts: BTreeMap::from([
            ("existence".into(), json!("present")),
            ("model.name".into(), json!("Curated")),
        ]),
        auth_methods: vec!["apikey".into()],
        source_ref: "https://example.test/catalog".into(),
        established_by: Some("fixture".into()),
        established_at_ms: 1100,
        review_by_ms: 10_000,
    };
    store.append_curated_models(&[row], &[], 1200).unwrap();
    store.append_curated_models(&[], &[], 2000).unwrap();
    let full = get(&store, json!({"at_ms":1500}));
    assert!(full.models.contains_key("zzzz-curated-only/model"));
    let mut args = json!({"at_ms":1500,"paged":true});
    let mut models = BTreeMap::new();
    loop {
        let page = get(&store, args.clone());
        models.extend(page.models);
        let Some(token) = page.next_page_token else {
            break;
        };
        args["page_token"] = json!(token);
    }
    assert_eq!(models, full.models);
}

#[test]
fn named_dated_retirement_uses_the_boundary_in_force() {
    let (store, _dir) = fixture();
    store
        .append_eras(&[
            era("upstream", "existence", json!("present"), 1000),
            era("upstream", "existence", json!("absent"), 2000),
            era("upstream", "existence", json!("present"), 3000),
        ])
        .unwrap();
    let response = get(
        &store,
        json!({"provider_id":"upstream", "model_id":"model", "at_ms":2500}),
    );
    assert_eq!(response.retired.len(), 1);
    assert_eq!(response.retired[0].retired_at_ms, 2000);
}

#[test]
fn retirement_lookup_failure_is_unavailable() {
    let (store, _dir) = fixture();
    store
        .append_eras(&[
            era("upstream", "existence", json!("present"), 1000),
            era("upstream", "existence", json!("absent"), 2000),
        ])
        .unwrap();
    rusqlite::Connection::open(store.path())
        .unwrap()
        .execute("DROP TABLE observation_artifact_event", [])
        .unwrap();
    let error = serve_catalog_get(
        &store,
        br#"{"provider_id":"upstream","model_id":"model","at_ms":2500}"#,
    )
    .unwrap_err();
    assert_eq!(error.code, "unavailable");
    assert!(error.message.contains("observation_artifact_event"));
}

#[test]
fn artifact_marks_change_named_retirement_times_but_not_dated_pages() {
    let (store, _dir) = fixture();
    let observation_id = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(3000),
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    store
        .append_eras(&[
            era("a", "existence", json!("present"), 1000),
            era("b", "existence", json!("present"), 1000),
            era("b", "existence", json!("absent"), 2000),
            NewEra {
                observation_id: Some(observation_id),
                ..era("b", "existence", json!("absent"), 3000)
            },
        ])
        .unwrap();
    let retired_at = || {
        get(
            &store,
            json!({"provider_id":"b","model_id":"model","at_ms":3500}),
        )
        .retired[0]
            .retired_at_ms
    };
    let page = |args: Value| {
        serve_catalog_get_with_budget(&store, &serde_json::to_vec(&args).unwrap(), 1).unwrap()
    };
    let mut args = json!({"at_ms":3500,"paged":true,"include_retired":true});
    args["page_token"] = json!(page(args.clone()).next_page_token.unwrap());
    let before = page(args.clone());
    assert_eq!(before.models["b/model"]["existence"], "absent");
    assert_eq!(retired_at(), 3000);

    let mark =
        json!({"observation_id":observation_id,"reason":"restated presence","dry_run":false});
    let mark = serde_json::to_vec(&mark).unwrap();
    assert!(serve_mark_artifact(&store, &mark).unwrap().committed);
    assert_eq!(retired_at(), 2000);
    assert_eq!(
        serde_json::to_vec(&page(args.clone())).unwrap(),
        serde_json::to_vec(&before).unwrap()
    );

    assert!(serve_retract_artifact(&store, &mark).unwrap().committed);
    assert_eq!(retired_at(), 3000);
    assert_eq!(
        serde_json::to_vec(&page(args)).unwrap(),
        serde_json::to_vec(&before).unwrap()
    );
}

#[test]
fn committed_correction_invalidates_a_dated_page_token() {
    let (store, _dir) = fixture();
    let rate = FactKey::rate(TokenClass::Input);
    store
        .append_eras(&[
            era("a", "existence", json!("present"), 1000),
            era("a", rate.as_str(), json!({"state":"stated_zero"}), 1000),
            era("b", "existence", json!("present"), 1000),
            era("a", rate.as_str(), json!({"state":"absent"}), 3000),
        ])
        .unwrap();
    // Keep page one to a single provider without needing MiBs of input facts.
    let page =
        |args: Value| serve_catalog_get_with_budget(&store, &serde_json::to_vec(&args).unwrap(), 1);
    let first = page(json!({"at_ms":1500, "paged":true})).unwrap();
    assert!(first.models["a/model"].contains_key(rate.as_str()));
    let token = first.next_page_token.unwrap();
    let correction = json!({"name":"catalog.correct","arguments":{
        "provider_id":"a", "model_id":"model", "fields":[{"field":"rate","class":"input"}],
        "affected_from_ms":1000, "affected_until_ms":2000, "reason":"invalid price", "dry_run":false
    }});
    let reply = serve_tool_call(&store, &serde_json::to_vec(&correction).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(reply).unwrap()["written"], true);
    let corrected = get(&store, json!({"at_ms":1500}));
    assert!(corrected
        .withheld
        .iter()
        .any(|r| r.model == "a/model" && r.fact_key == rate.as_str()));
    let after = page(json!({"at_ms":1500,"paged":true,"page_token":token}));
    let Err(error) = after else {
        panic!("continuation accepted a page token issued before the correction");
    };
    assert_eq!(error.code, fusiform_protocol::CODE_CATALOG_CHANGED);
    assert!(corrected.catalog_version > first.catalog_version);
}
