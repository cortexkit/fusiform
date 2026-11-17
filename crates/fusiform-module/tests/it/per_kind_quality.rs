use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, SourceId, Timestamp};
use fusiform_module::route::{serve_tool_call, RouteError};
use fusiform_store::{CatalogStore, FactKey, NewEra};
use serde_json::{json, Value};

fn fixture() -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("kinds.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    (store, dir)
}

fn seed(store: &CatalogStore, model: &str, facts: Vec<(&str, Value)>) {
    let (provider, model) = model.split_once('/').unwrap();
    let eras = std::iter::once(("existence", json!("present")))
        .chain(facts)
        .map(|(key, value)| NewEra {
            source: SourceId::ModelsDev,
            provider_id: provider.into(),
            model_id: model.into(),
            fact_key: FactKey::from_stored(key.into()),
            value_json: value.to_string(),
            boundary_at: Timestamp(1000),
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        })
        .collect::<Vec<_>>();
    store.append_eras(&eras).unwrap();
}

fn call(store: &CatalogStore, name: &str, arguments: Value) -> Result<Value, RouteError> {
    serve_tool_call(
        store,
        &serde_json::to_vec(&json!({"name":name,"arguments":arguments})).unwrap(),
    )
    .map(|response| serde_json::to_value(response).unwrap())
}

fn grade(store: &CatalogStore, model: &str, kind: &str, n: i64) -> Value {
    call(
        store,
        "quality.grade",
        json!({"model":model,"kind":kind,"grade":n,"reason":"evaluation","dry_run":false}),
    )
    .unwrap()
}

#[test]
fn kind_guard_checks_the_matching_modality_on_at_least_one_reached_row() {
    let (store, _dir) = fixture();
    seed(
        &store,
        "test/no-media",
        vec![
            ("capability.input_modalities", json!(["text"])),
            ("capability.output_modalities", json!(["text"])),
        ],
    );
    for (kind, fact, modality) in [
        ("transcription", "capability.input_modalities", "audio"),
        ("speech", "capability.output_modalities", "audio"),
        ("image", "capability.output_modalities", "image"),
        ("video", "capability.output_modalities", "video"),
    ] {
        let refused = call(
            &store,
            "quality.grade",
            json!({"model":"test/no-media","kind":kind,"grade":80,"reason":"evaluation"}),
        )
        .unwrap_err();
        assert_eq!(refused.code, "bad_request");
        assert!(
            refused.message.contains(fact) && refused.message.contains(modality),
            "{}",
            refused.message
        );
        // The wrong direction or a boolean capability must not satisfy the guard.
        let other = if fact == "capability.input_modalities" {
            "capability.output_modalities"
        } else {
            "capability.input_modalities"
        };
        let key = format!("test/{kind}");
        seed(
            &store,
            &key,
            vec![(other, json!([modality])), (fact, json!(null))],
        );
        assert!(call(
            &store,
            "quality.grade",
            json!({"model":key,"kind":kind,"grade":80,"reason":"evaluation"})
        )
        .is_err());
        seed(
            &store,
            &format!("capable/{kind}"),
            vec![
                ("model.canonical_id", json!(key)),
                (fact, json!([modality])),
            ],
        );
        let accepted = grade(&store, &key, kind, 80);
        assert_eq!(accepted["affected_count"], 2);
        assert_eq!(accepted["new"]["kind"], kind);
    }
}

#[test]
fn chat_keeps_elo_eq_with_media_grades_and_kind_scoped_withdrawals() {
    let (store, _dir) = fixture();
    seed(
        &store,
        "google/gemini-4-hydrogen",
        vec![
            ("model.type", json!("chat")),
            ("capability.input_modalities", json!(["audio", "text"])),
        ],
    );
    call(&store, "quality.grade", json!({"model":"google/gemini-4-hydrogen","elo":1500,"eq":1600,"reason":"chat evaluation","dry_run":false})).unwrap();
    grade(&store, "google/gemini-4-hydrogen", "transcription", 95);
    for args in [
        json!({}),
        json!({"provider_id":"google","model_id":"gemini-4-hydrogen"}),
        json!({"paged":true}),
        json!({"fact_prefixes":["quality."]}),
    ] {
        let response = call(&store, "catalog.get", args).unwrap();
        let facts = &response["models"]["google/gemini-4-hydrogen"];
        assert_eq!(facts["quality.elo"], 1500);
        assert_eq!(facts["quality.eq"], 1600);
        assert_eq!(facts["quality.grade.transcription"], 95);
        assert!(facts.get("quality.grade").is_none());
        assert!(response.get("quality_scores").is_some());
    }
    let filtered = call(
        &store,
        "catalog.get",
        json!({"fact_prefixes":["quality.grade.transcription"]}),
    )
    .unwrap();
    assert_eq!(
        filtered["models"]["google/gemini-4-hydrogen"],
        json!({"quality.grade.transcription":95})
    );
    let past = call(
        &store,
        "catalog.get",
        json!({"provider_id":"google","model_id":"gemini-4-hydrogen","at_ms":1000}),
    )
    .unwrap();
    assert!(past["models"]["google/gemini-4-hydrogen"]
        .get("quality.grade.transcription")
        .is_none());
    call(&store, "quality.withdraw", json!({"model":"google/gemini-4-hydrogen","kind":"transcription","reason":"withdraw media","dry_run":false})).unwrap();
    let response = call(
        &store,
        "catalog.get",
        json!({"provider_id":"google","model_id":"gemini-4-hydrogen"}),
    )
    .unwrap();
    assert_eq!(
        response["models"]["google/gemini-4-hydrogen"]["quality.elo"],
        1500
    );
    assert!(response["models"]["google/gemini-4-hydrogen"]
        .get("quality.grade.transcription")
        .is_none());
}

#[test]
fn local_keys_never_attach_to_catalog_and_refuse_chat_scores_and_marks() {
    let (store, _dir) = fixture();
    seed(
        &store,
        "apple/phone-transcriber",
        vec![
            ("model.type", json!("transcription")),
            ("capability.input_modalities", json!(["audio"])),
        ],
    );
    let version = store.catalog_version().unwrap();
    let preview = call(&store, "quality.grade", json!({"model":"local:apple/phone-transcriber","kind":"transcription","grade":80,"reason":"preview"})).unwrap();
    assert_eq!(preview["affected_count"], 0);
    assert_eq!(store.catalog_version().unwrap(), version);
    grade(&store, "local:apple/phone-transcriber", "transcription", 80);
    assert!(store.catalog_version().unwrap() > version);
    for args in [
        json!({}),
        json!({"provider_id":"apple","model_id":"phone-transcriber"}),
        json!({"paged":true}),
    ] {
        let catalog = call(&store, "catalog.get", args).unwrap();
        assert!(catalog["models"]
            .as_object()
            .unwrap()
            .keys()
            .all(|key| !key.starts_with("local:")));
        assert!(catalog["models"]["apple/phone-transcriber"]
            .get("quality.grade.transcription")
            .is_none());
    }
    for dimensions in [
        json!({"elo":1}),
        json!({"eq":1}),
        json!({"elo":1,"eq":2}),
        json!({"elo":1,"eq":2,"kind":"speech"}),
    ] {
        let mut args =
            json!({"model":"local:apple/phone-transcriber","reason":"invalid","dry_run":false});
        args.as_object_mut()
            .unwrap()
            .extend(dimensions.as_object().unwrap().clone());
        let error = call(&store, "quality.grade", args).unwrap_err();
        assert!(
            error.message.contains("local engine") && error.message.contains("ELO/EQ"),
            "{}",
            error.message
        );
    }
    for tool in ["quality.decommission", "quality.restore"] {
        let error = call(
            &store,
            tool,
            json!({"model":"local:apple/phone-transcriber","reason":"invalid","dry_run":false}),
        )
        .unwrap_err();
        assert!(error.message.contains("local engine") && error.message.contains("decommission"));
    }
    for key in [
        "local:",
        "local:/model",
        "local:provider/",
        "local:provider",
        "local:provider/a b",
        "local:provider/a\n",
        "local:provider/a\u{7f}",
    ] {
        assert!(
            call(
                &store,
                "quality.grade",
                json!({"model":key,"kind":"speech","grade":1,"reason":"invalid"})
            )
            .is_err(),
            "{key:?}"
        );
    }
    // Local engines are valid without catalog rows, including nested model ids.
    grade(&store, "local:unlisted/nested/model", "speech", 50);
    let version = store.catalog_version().unwrap();
    call(&store, "quality.withdraw", json!({"model":"local:apple/phone-transcriber","kind":"transcription","reason":"withdraw","dry_run":false})).unwrap();
    assert!(store.catalog_version().unwrap() > version);
}

#[test]
fn prefix_reads_latest_live_state_per_key_and_kind_and_history_keeps_kind() {
    let (store, _dir) = fixture();
    grade(&store, "local:apple/phone-transcriber", "transcription", 80);
    grade(&store, "local:apple/phone-transcriber", "speech", 81);
    grade(&store, "local:apple/phone-transcriber", "transcription", 90);
    grade(&store, "local:other/image", "image", 70);
    call(
        &store,
        "quality.withdraw",
        json!({"model":"local:other/image","kind":"image","reason":"withdraw","dry_run":false}),
    )
    .unwrap();
    let response = call(&store, "quality.grades", json!({"prefix":"local:"})).unwrap();
    let rows = response["grades"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["model"], "local:apple/phone-transcriber");
    assert_eq!(rows[0]["kind"], "speech");
    assert_eq!(rows[0]["grade"], 81);
    assert_eq!(rows[1]["kind"], "transcription");
    assert_eq!(rows[1]["grade"], 90);
    assert!(rows.iter().all(|row| row["withdrawn"] == false));
    assert!(
        call(&store, "quality.grades", json!({"prefix":"local:missing"})).unwrap()["grades"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let history = call(
        &store,
        "quality.grades",
        json!({"model":"local:other/image"}),
    )
    .unwrap();
    assert_eq!(history["grades"][0]["kind"], "image");
    assert_eq!(history["grades"][0]["withdrawn"], true);
    assert_eq!(history["grades"][1]["grade"], 70);
    assert!(call(
        &store,
        "quality.grades",
        json!({"model":"local:other/image","prefix":"local:"})
    )
    .unwrap_err()
    .message
    .contains("exclusive"));
}

#[test]
fn search_kind_requires_search_type_not_modalities_and_supports_local_keys() {
    let (store, _dir) = fixture();
    seed(&store, "test/chat", vec![("model.type", json!("chat"))]);
    let error = call(
        &store,
        "quality.grade",
        json!({"model":"test/chat","kind":"search","grade":50,"reason":"invalid"}),
    )
    .unwrap_err();
    assert_eq!(error.code, "bad_request");
    assert!(error.message.contains("model.type=search"));
    let response = grade(&store, "exa/search", "search", 85);
    assert_eq!(response["affected_ids"], json!(["exa/search"]));
    let catalog = call(
        &store,
        "catalog.get",
        json!({"provider_id":"exa","model_id":"search","fact_prefixes":["quality."]}),
    )
    .unwrap();
    assert_eq!(catalog["models"]["exa/search"]["quality.grade.search"], 85);
    seed(
        &store,
        "test/search-reseller",
        vec![
            ("model.type", json!("search")),
            ("model.canonical_id", json!("exa/search")),
        ],
    );
    for arguments in [
        json!({"provider_id":"test","model_id":"search-reseller","fact_prefixes":["quality.grade.search"]}),
        json!({"provider_id":"test","fact_prefixes":["quality.grade.search"]}),
        json!({"paged":true,"fact_prefixes":["quality.grade.search"]}),
    ] {
        let filtered = call(&store, "catalog.get", arguments).unwrap();
        assert_eq!(
            filtered["models"]["test/search-reseller"],
            json!({"quality.grade.search":85})
        );
    }
    grade(&store, "local:example/offline-search", "search", 60);
    let local = call(&store, "quality.grades", json!({"prefix":"local:example/"})).unwrap();
    assert_eq!(local["grades"][0]["kind"], "search");
    for kind in ["chat", "embedding", "reranking", "", "SEARCH"] {
        assert!(call(
            &store,
            "quality.grade",
            json!({"model":"exa/search","kind":kind,"grade":1,"reason":"invalid"})
        )
        .unwrap_err()
        .message
        .contains("kind must be"));
    }
}

#[test]
fn every_curated_media_and_search_row_passes_guard_over_the_embedded_catalog() {
    let (store, _dir) = fixture();
    let catalog = fusiform_core::normalize_models_dev_with(
        include_bytes!("../../data/models-dev-seed.json"),
        &fusiform_module::input_audio::evidence(),
    )
    .unwrap()
    .catalog;
    let plan = fusiform_store::ingest::plan_ingest(
        &store,
        &catalog,
        Timestamp(1000),
        BoundaryKind::Seed,
        None,
    )
    .unwrap();
    store.append_eras(&plan.eras).unwrap();
    let curated: Value =
        serde_json::from_str(include_str!("../../data/curated-models.json")).unwrap();
    let mut checked = 0;
    for row in curated["models"].as_array().unwrap() {
        let Some(kind) = row["facts"]["model.type"].as_str().filter(|kind| {
            matches!(
                *kind,
                "transcription" | "speech" | "image" | "video" | "search"
            )
        }) else {
            continue;
        };
        let model = format!(
            "{}/{}",
            row["provider_id"].as_str().unwrap(),
            row["model_id"].as_str().unwrap()
        );
        call(
            &store,
            "quality.grade",
            json!({"model":model,"kind":kind,"grade":1,"reason":"guard audit"}),
        )
        .unwrap_or_else(|error| panic!("curated engine {model} refused {kind}: {error:?}"));
        eprintln!("guard passes: {model} ({kind})");
        checked += 1;
    }
    assert_eq!(checked, 14);
}
