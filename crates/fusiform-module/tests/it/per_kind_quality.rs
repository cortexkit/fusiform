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
fn transcription_grade_reaches_only_audio_input_rows() {
    let (store, _dir) = fixture();
    let model = "lab/transcription-eligibility";
    seed(
        &store,
        "test/audio-input",
        vec![
            ("model.canonical_id", json!(model)),
            ("capability.input_modalities", json!(["text", "audio"])),
        ],
    );
    seed(
        &store,
        "test/text-input",
        vec![
            ("model.canonical_id", json!(model)),
            ("capability.input_modalities", json!(["text"])),
        ],
    );

    let accepted = grade(&store, model, "transcription", 98);
    for args in [
        json!({"provider_id":"test"}),
        json!({"provider_id":"test","at_ms":accepted["new"]["recorded_at_ms"]}),
    ] {
        let catalog = call(&store, "catalog.get", args).unwrap();
        let rows = &catalog["models"];
        assert_eq!(rows["test/audio-input"]["quality.grade.transcription"], 98);
        assert_eq!(
            rows["test/text-input"]["capability.input_modalities"],
            json!(["text"])
        );
        assert_eq!(
            rows["test/text-input"].get("quality.grade.transcription"),
            None
        );
        assert_eq!(catalog["quality_scores"]["graded_by"], "operator");
    }
    let catalog = call(
        &store,
        "catalog.get",
        json!({"provider_id":"test","model_id":"text-input"}),
    )
    .unwrap();
    assert!(catalog.get("quality_scores").is_none());
    assert_eq!(accepted["affected_ids"], json!(["test/audio-input"]));
    assert_eq!(accepted["affected_count"], 1);
    let withdrawn = call(
        &store,
        "quality.withdraw",
        json!({"model":model,"kind":"transcription","reason":"withdraw evaluation","dry_run":false}),
    )
    .unwrap();
    assert_eq!(withdrawn["affected_ids"], json!(["test/audio-input"]));
    assert_eq!(withdrawn["affected_count"], 1);
}

fn assert_task_grade_split(kind: &str, fact: &str, eligible: Value, ineligible: Value) {
    let (store, _dir) = fixture();
    let model = "lab/task-eligibility";
    for (id, value) in [("test/eligible", eligible), ("test/ineligible", ineligible)] {
        seed(
            &store,
            id,
            vec![("model.canonical_id", json!(model)), (fact, value)],
        );
    }
    let accepted = grade(&store, model, kind, 80);
    let grade_fact = format!("quality.grade.{kind}");
    for args in [
        json!({"provider_id":"test"}),
        json!({"provider_id":"test","fact_prefixes":[grade_fact,"existence"]}),
    ] {
        let catalog = call(&store, "catalog.get", args).unwrap();
        assert_eq!(catalog["models"]["test/eligible"][&grade_fact], 80);
        let ineligible = catalog["models"]["test/ineligible"].as_object().unwrap();
        assert!(!ineligible.contains_key(&grade_fact));
    }
    assert_eq!(accepted["affected_ids"], json!(["test/eligible"]));
    assert_eq!(accepted["affected_count"], 1);
    let withdrawn = call(
        &store,
        "quality.withdraw",
        json!({"model":model,"kind":kind,"reason":"withdraw evaluation","dry_run":false}),
    )
    .unwrap();
    assert_eq!(withdrawn["affected_ids"], json!(["test/eligible"]));
    assert_eq!(withdrawn["affected_count"], 1);
}

#[test]
fn speech_grade_reaches_only_audio_output_rows() {
    assert_task_grade_split(
        "speech",
        "capability.output_modalities",
        json!(["audio"]),
        json!(["text"]),
    );
}

#[test]
fn image_grade_reaches_only_image_output_rows() {
    assert_task_grade_split(
        "image",
        "capability.output_modalities",
        json!(["image"]),
        json!(["video"]),
    );
}

#[test]
fn video_grade_reaches_only_video_output_rows() {
    assert_task_grade_split(
        "video",
        "capability.output_modalities",
        json!(["video"]),
        json!(["image"]),
    );
}

#[test]
fn search_grade_reaches_only_search_rows() {
    assert_task_grade_split("search", "model.type", json!("search"), json!("chat"));
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
        assert_eq!(accepted["affected_count"], 1);
        assert_eq!(accepted["affected_ids"], json!([format!("capable/{kind}")]));
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
fn decision_grade_reaches_only_decision_rows() {
    let (store, _dir) = fixture();
    let model = "openai/gpt-6-luna-decisions";
    seed(
        &store,
        "test/decision-reseller",
        vec![
            ("model.canonical_id", json!(model)),
            ("model.kind", json!("decision")),
        ],
    );
    seed(
        &store,
        "test/chat-reseller",
        vec![
            ("model.canonical_id", json!(model)),
            ("model.type", json!("chat")),
        ],
    );

    let accepted = grade(&store, model, "decision", 83);
    let affected = json!([model, "test/decision-reseller"]);
    assert_eq!(accepted["affected_ids"], affected);
    assert_eq!(accepted["affected_count"], 2);
    let chat = call(
        &store,
        "catalog.get",
        json!({"provider_id":"test","model_id":"chat-reseller"}),
    )
    .unwrap();
    assert_eq!(chat["models"]["test/chat-reseller"]["model.type"], "chat");
    assert!(chat["models"]["test/chat-reseller"]
        .get("quality.grade.decision")
        .is_none());
    for args in [
        json!({"provider_id":"openai","model_id":"gpt-6-luna-decisions"}),
        json!({"provider_id":"test","model_id":"decision-reseller"}),
        json!({"provider_id":"test","fact_prefixes":["quality.grade.decision"]}),
        json!({"paged":true,"fact_prefixes":["quality.grade.decision"]}),
        json!({"provider_id":"test","model_id":"decision-reseller","fact_prefixes":["quality.grade.decision"],"at_ms":accepted["new"]["recorded_at_ms"]}),
    ] {
        let expected = if args["provider_id"] == "openai" {
            model
        } else {
            "test/decision-reseller"
        };
        let catalog = call(&store, "catalog.get", args).unwrap();
        assert_eq!(catalog["models"][expected]["quality.grade.decision"], 83);
        assert!(catalog["models"]["test/chat-reseller"]
            .get("quality.grade.decision")
            .is_none());
    }
    let withdrawn = call(
        &store,
        "quality.withdraw",
        json!({"model":model,"kind":"decision","reason":"withdraw test grade","dry_run":false}),
    )
    .unwrap();
    assert_eq!(withdrawn["affected_ids"], affected);
    assert_eq!(withdrawn["affected_count"], 2);
    let catalog = call(
        &store,
        "catalog.get",
        json!({"provider_id":"openai","model_id":"gpt-6-luna-decisions"}),
    )
    .unwrap();
    assert!(catalog["models"][model]
        .get("quality.grade.decision")
        .is_none());
}

#[test]
fn decision_kind_rejects_chat_and_search_and_supports_local_keys() {
    let (store, _dir) = fixture();
    seed(&store, "test/chat", vec![("model.type", json!("chat"))]);
    seed(
        &store,
        "test/type-only",
        vec![("model.type", json!("decision"))],
    );
    seed(
        &store,
        "cloudflare-workers-ai/@cf/cloudflare/clairic",
        vec![("model.type", json!("extraction"))],
    );
    for model in [
        "test/chat",
        "exa/search",
        "cloudflare-workers-ai/@cf/cloudflare/clairic",
    ] {
        let error = call(
            &store,
            "quality.grade",
            json!({"model":model,"kind":"decision","grade":50,"reason":"invalid","dry_run":false}),
        )
        .unwrap_err();
        assert_eq!(error.code, "bad_request");
        assert!(error.message.contains("model.kind=decision"), "{error:?}");
    }
    let derived = grade(&store, "test/type-only", "decision", 78);
    assert_eq!(derived["affected_count"], 1);
    let accepted = grade(&store, "local:test/decision", "decision", 82);
    assert_eq!(accepted["affected_count"], 0);
    let local = call(&store, "quality.grades", json!({"prefix":"local:test/"})).unwrap();
    assert_eq!(local["grades"][0]["kind"], "decision");
    assert_eq!(local["grades"][0]["grade"], 82);
}

#[test]
fn current_and_formerly_curated_task_rows_pass_the_v2_seed_guard() {
    let (store, _dir) = fixture();
    let catalog = fusiform_module::seed::catalog().unwrap();
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
    let retired: Value =
        serde_json::from_str(include_str!("../../fixtures/retired-curated-models.json")).unwrap();
    let mut task_rows = 0;
    let mut decision_rows = 0;
    for row in curated["models"]
        .as_array()
        .unwrap()
        .iter()
        .chain(retired["models"].as_array().unwrap())
    {
        let kind = if row["facts"]["model.kind"] == "decision" {
            Some("decision")
        } else {
            row["facts"]["model.type"].as_str().filter(|kind| {
                matches!(
                    *kind,
                    "transcription" | "speech" | "image" | "video" | "search"
                )
            })
        };
        let Some(kind) = kind else {
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
        if kind == "decision" {
            decision_rows += 1;
        } else {
            task_rows += 1;
        }
    }
    assert_eq!((task_rows, decision_rows), (14, 5));
}
