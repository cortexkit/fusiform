use super::*;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::BoundaryKind;
use fusiform_store::NewEra;
use serde_json::{json, Value};

#[test]
fn quality_grade_commit_refreshes_named_provider_and_cached_pages() {
    let (store, _dir) = fixture(true);
    let status_version = || serve_status(&store, b"{}").unwrap().catalog_version;
    let version_before = status_version();
    let args: Value = serde_json::from_str(
        r#"{"model":"lab/shared","elo":1234.500,"eq":1789,"reason":"new evaluation"}"#,
    )
    .unwrap();
    let call = |name: &str, arguments: Value| {
        serve_tool_call(
            &store,
            &serde_json::to_vec(&json!({"name":name,"arguments":arguments})).unwrap(),
        )
    };
    let preview =
        serde_json::to_value(call("quality.grade", args.clone()).expect("quality.grade previews"))
            .unwrap();
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["affected_count"], 3);
    assert_eq!(
        preview["affected_ids"],
        json!([
            "google/antigravity-gemini-4-argon",
            "google/gemini-4-argon",
            "reseller/shared"
        ])
    );
    assert!(preview["current"].is_null());
    assert!(store
        .quality_grade_history("lab/shared")
        .unwrap()
        .is_empty());
    assert_eq!(status_version(), version_before);
    assert!(
        !get(&store, json!({"paged":true})).models["reseller/shared"].contains_key("quality.elo")
    );
    let key_before = WholeCatalogKey::new(
        &store,
        &CatalogGetRequest::default(),
        SourceId::ModelsDev,
        REPLY_FRAME_LIMIT_BYTES,
    )
    .unwrap();
    let mut commit = args;
    commit["dry_run"] = json!(false);
    assert_eq!(
        serde_json::to_value(call("quality.grade", commit).unwrap()).unwrap()["dry_run"],
        false
    );
    assert_eq!(store.quality_grade_history("lab/shared").unwrap().len(), 1);
    let version_after_grade = status_version();
    assert!(version_after_grade > version_before);
    let key_after = WholeCatalogKey::new(
        &store,
        &CatalogGetRequest::default(),
        SourceId::ModelsDev,
        REPLY_FRAME_LIMIT_BYTES,
    )
    .unwrap();
    assert!(
        key_before != key_after,
        "grade writes must invalidate page-plan and oversized-response caches"
    );
    for query in [
        json!({"provider_id":"reseller","model_id":"shared"}),
        json!({"provider_id":"reseller"}),
        json!({"paged":true}),
    ] {
        let response = get(&store, query);
        assert_eq!(
            response.models["reseller/shared"]["quality.elo"].to_string(),
            "1234.500"
        );
        assert!(response.quality_scores.is_some());
    }
    let withdrawal = json!({"model":"lab/shared","reason":"retracted"});
    assert_eq!(
        serde_json::to_value(call("quality.withdraw", withdrawal).unwrap()).unwrap()["dry_run"],
        true
    );
    assert_eq!(store.quality_grade_history("lab/shared").unwrap().len(), 1);
    assert_eq!(status_version(), version_after_grade);
    call(
        "quality.withdraw",
        json!({"model":"lab/shared","reason":"retracted","dry_run":false}),
    )
    .unwrap();
    assert_eq!(store.quality_grade_history("lab/shared").unwrap().len(), 2);
    assert!(status_version() > version_after_grade);
    assert!(
        !get(&store, json!({"paged":true})).models["reseller/shared"].contains_key("quality.elo")
    );
}

#[test]
fn quality_grade_refuses_unknown_invalid_and_missing_coverage() {
    let (store, _dir) = fixture(true);
    let call = |name: &str, arguments: Value| {
        serve_tool_call(
            &store,
            &serde_json::to_vec(&json!({"name":name,"arguments":arguments})).unwrap(),
        )
    };
    let unknown = call(
        "quality.grade",
        json!({"model":"missing/model","elo":1,"eq":2,"reason":"evaluation","dry_run":false}),
    )
    .unwrap_err();
    assert_eq!(unknown.code, "bad_request");
    assert!(unknown.message.contains("missing/model") && unknown.message.contains("canonical"));
    for token in ["-1", "0", "1e9999", "null", "\"NaN\"", "\"Infinity\""] {
        for field in ["elo", "eq"] {
            let mut arguments =
                json!({"model":"lab/shared","elo":1,"eq":2,"reason":"evaluation","dry_run":false});
            arguments[field] = serde_json::from_str(token).unwrap();
            assert_eq!(
                call("quality.grade", arguments).unwrap_err().code,
                "bad_request",
                "{field}={token}"
            );
        }
    }
    let malformed = br#"{"name":"quality.grade","arguments":{"model":"lab/shared","elo":NaN,"eq":2,"reason":"invalid"}}"#;
    assert_eq!(
        serve_tool_call(&store, malformed).unwrap_err().code,
        "bad_request"
    );
    assert_eq!(
        call(
            "quality.withdraw",
            json!({"model":"lab/shared","reason":"withdraw"})
        )
        .unwrap_err()
        .code,
        "no_coverage"
    );
    assert_eq!(
        call("quality.grades", json!({"model":"lab/shared"}))
            .unwrap_err()
            .code,
        "no_coverage"
    );
    assert!(store
        .quality_grade_history("lab/shared")
        .unwrap()
        .is_empty());
    assert!(store
        .quality_grade_history("missing/model")
        .unwrap()
        .is_empty());
}

#[test]
fn quality_grades_lists_current_or_newest_first_history_and_response_dates() {
    let (store, _dir) = fixture(true);
    seed_grades(&store);
    let grade = fusiform_store::QualityGrade::Chat {
        elo: serde_json::from_str("1400.000").unwrap(),
        eq: serde_json::from_str("1600").unwrap(),
    };
    store
        .append_quality_grade(
            "reseller/own",
            Some(&grade),
            "operator",
            "re-evaluation",
            456,
        )
        .unwrap();
    assert_eq!(
        get(&store, json!({"provider_id":"reseller"}))
            .quality_scores
            .unwrap()
            .graded_at_ms,
        456
    );
    assert_eq!(
        get(
            &store,
            json!({"provider_id":"reseller","model_id":"shared"})
        )
        .quality_scores
        .unwrap()
        .graded_at_ms,
        123
    );
    let response = quality::grades(&store, b"{}").unwrap();
    assert_eq!(response.grades.len(), 4);
    let history = quality::grades(&store, br#"{"model":"reseller/own"}"#).unwrap();
    assert_eq!(history.grades.len(), 2);
    assert_eq!(history.grades[0].reason, "re-evaluation");
    assert_eq!(
        history.grades[0].elo.as_ref().unwrap().to_string(),
        "1400.000"
    );
    store
        .append_quality_grade("reseller/own", None, "operator", "withdrawn", 789)
        .unwrap();
    let history = quality::grades(&store, br#"{"model":"reseller/own"}"#).unwrap();
    assert!(history.grades[0].withdrawn);
    assert!(history.grades[0].elo.is_none() && history.grades[0].eq.is_none());
    assert_eq!(quality::grades(&store, b"{}").unwrap().grades.len(), 3);
    assert_eq!(
        get(&store, json!({"provider_id":"reseller"}))
            .quality_scores
            .unwrap()
            .graded_at_ms,
        123
    );
}

const GRADES: &str = r#"{"graded_by":"operator","graded_at_ms":123,"purpose":"routing","rows":[
    {"model":"lab/shared","elo":1284,"eq":1599.8},
    {"model":"reseller/own","elo":1000.00,"eq":900.00},
    {"model":"reseller/shared","elo":99,"eq":99},
    {"model":"google/antigravity-gemini-4-argon","elo":99,"eq":99}
]}"#;

#[test]
fn quality_grade_media_roundtrip_history_and_type_refusals() {
    let (store, _dir) = fixture(true);
    for (model, canonical) in [("transcribe", Some("lab/transcribe")), ("own-audio", None)] {
        let mut facts = vec![
            ("existence", json!("present")),
            ("model.type", json!("transcription")),
        ];
        if let Some(canonical) = canonical {
            facts.push(("model.canonical_id", json!(canonical)));
        }
        store
            .append_eras(
                &facts
                    .into_iter()
                    .map(|(key, value)| NewEra {
                        source: SourceId::ModelsDev,
                        provider_id: "reseller".into(),
                        model_id: model.into(),
                        fact_key: FactKey::from_stored(key.into()),
                        value_json: value.to_string(),
                        boundary_at: Timestamp(1000),
                        boundary_kind: BoundaryKind::Seed,
                        observation_id: None,
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();
    }
    let call = |name: &str, arguments: Value| {
        serve_tool_call(
            &store,
            &serde_json::to_vec(&json!({"name":name,"arguments":arguments})).unwrap(),
        )
    };
    let args: Value =
        serde_json::from_str(r#"{"model":"lab/transcribe","grade":87.500,"reason":"evaluation"}"#)
            .unwrap();
    let before = store.catalog_version().unwrap();
    let preview = serde_json::to_value(call("quality.grade", args.clone()).unwrap()).unwrap();
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["new"]["grade"].to_string(), "87.500");
    assert_eq!(preview["affected_ids"], json!(["reseller/transcribe"]));
    assert_eq!(store.catalog_version().unwrap(), before);
    assert!(store
        .quality_grade_history("lab/transcribe")
        .unwrap()
        .is_empty());
    let mut commit = args;
    commit["dry_run"] = json!(false);
    call("quality.grade", commit.clone()).unwrap();
    assert!(store.catalog_version().unwrap() > before);
    for query in [
        json!({}),
        json!({"provider_id":"reseller"}),
        json!({"provider_id":"reseller","model_id":"transcribe"}),
        json!({"paged":true}),
    ] {
        let response = get(&store, query);
        let facts = &response.models["reseller/transcribe"];
        assert_eq!(facts["quality.grade"].to_string(), "87.500");
        assert!(!facts.contains_key("quality.elo") && !facts.contains_key("quality.eq"));
        assert!(response.quality_scores.is_some());
    }
    let past = get(&store, json!({"at_ms":1000}));
    assert!(!past.models["reseller/transcribe"].contains_key("quality.grade"));
    assert!(past.quality_scores.is_none());
    let filtered = get(&store, json!({"fact_prefixes":["model."]}));
    assert!(filtered.quality_scores.is_none());
    call(
        "quality.grade",
        json!({"model":"reseller/own-audio","grade":12,"reason":"own key","dry_run":false}),
    )
    .unwrap();
    assert_eq!(
        get(&store, json!({})).models["reseller/own-audio"]["quality.grade"],
        12
    );
    for (arguments, kind) in [
        (
            json!({"model":"lab/shared","grade":1,"reason":"wrong kind"}),
            "chat",
        ),
        (
            json!({"model":"lab/transcribe","elo":1,"eq":2,"reason":"wrong kind"}),
            "transcription",
        ),
        (
            json!({"model":"lab/transcribe","elo":1,"reason":"wrong kind"}),
            "transcription",
        ),
        (
            json!({"model":"lab/transcribe","grade":1,"elo":2,"eq":3,"reason":"mixed"}),
            "transcription",
        ),
    ] {
        let error = call("quality.grade", arguments).unwrap_err();
        assert_eq!(error.code, "bad_request");
        assert!(error.message.contains(kind), "{}", error.message);
        assert!(error.message.contains("model.type"), "{}", error.message);
    }
    let unknown = call(
        "quality.grade",
        json!({"model":"missing/media","grade":1,"reason":"unknown"}),
    )
    .unwrap_err();
    assert_eq!(unknown.code, "bad_request");
    assert!(unknown.message.contains("unknown") && unknown.message.contains("model.type"));
    for token in ["0", "-1", "1e9999", "null", "\"NaN\""] {
        commit["grade"] = serde_json::from_str(token).unwrap();
        assert_eq!(
            call("quality.grade", commit.clone()).unwrap_err().code,
            "bad_request"
        );
    }
    call(
        "quality.withdraw",
        json!({"model":"lab/transcribe","reason":"withdrawn","dry_run":false}),
    )
    .unwrap();
    assert!(!get(&store, json!({})).models["reseller/transcribe"].contains_key("quality.grade"));
    call(
        "quality.grade",
        json!({"model":"lab/transcribe","grade":99,"reason":"regraded","dry_run":false}),
    )
    .unwrap();
    let history =
        serde_json::to_value(call("quality.grades", json!({"model":"lab/transcribe"})).unwrap())
            .unwrap();
    assert_eq!(history["grades"].as_array().unwrap().len(), 3);
    assert_eq!(history["grades"][0]["grade"], 99);
    assert_eq!(history["grades"][1]["withdrawn"], true);
    assert_eq!(history["grades"][2]["grade"].to_string(), "87.500");
}

fn fixture(canonical: bool) -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("quality.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    for row in store.quality_grades().unwrap() {
        store
            .append_quality_grade(&row.model, None, "operator", "fixture isolation", 0)
            .unwrap();
    }
    for (provider, model, has_canonical) in [
        ("reseller", "shared", canonical),
        ("reseller", "own", false),
        ("google", "gemini-4-argon", canonical),
    ] {
        let mut facts = vec![
            ("existence", json!("present")),
            ("rate.input", json!({"state":"stated_zero"})),
        ];
        if has_canonical {
            facts.push(("model.canonical_id", json!("lab/shared")));
        }
        let eras: Vec<_> = facts
            .into_iter()
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
            .collect();
        store.append_eras(&eras).unwrap();
    }
    (store, dir)
}

fn get(store: &CatalogStore, args: Value) -> CatalogGetResponse {
    serve_catalog_get(store, &serde_json::to_vec(&args).unwrap()).unwrap()
}

fn seed_grades(store: &CatalogStore) {
    let doc: Value = serde_json::from_str(GRADES).unwrap();
    for row in doc["rows"].as_array().unwrap() {
        let grade = fusiform_store::QualityGrade::Chat {
            elo: row["elo"].as_number().unwrap().clone(),
            eq: row["eq"].as_number().unwrap().clone(),
        };
        store
            .append_quality_grade(
                row["model"].as_str().unwrap(),
                Some(&grade),
                "operator",
                "fixture",
                123,
            )
            .unwrap();
    }
}

#[test]
fn quality_scores_join_served_identity_without_upstream_eras() {
    let (store, _dir) = fixture(true);
    seed_grades(&store);
    let eras = store.era_count(SourceId::ModelsDev).unwrap();
    let version = store.catalog_version().unwrap();
    for args in [
        json!({}),
        json!({"provider_id":"reseller"}),
        json!({"provider_id":"reseller","model_id":"shared"}),
        json!({"paged":true}),
    ] {
        let response = get(&store, args);
        let facts = &response.models["reseller/shared"];
        assert_eq!(facts["quality.elo"].to_string(), "1284");
        assert_eq!(facts["quality.eq"].to_string(), "1599.8");
        let disclosure = response.quality_scores.unwrap();
        assert_eq!(disclosure.graded_by, "operator");
        assert_eq!(disclosure.graded_at_ms, 123);
    }
    let response = get(&store, json!({"provider_id":"reseller","model_id":"own"}));
    assert_eq!(
        response.models["reseller/own"]["quality.elo"].to_string(),
        "1000.00"
    );
    assert_eq!(
        response.models["reseller/own"]["quality.eq"].to_string(),
        "900.00"
    );
    assert_eq!(store.era_count(SourceId::ModelsDev).unwrap(), eras);
    assert_eq!(store.catalog_version().unwrap(), version);
    let (stored, _) = store
        .read_model(SourceId::ModelsDev, "reseller", "shared", None)
        .unwrap();
    assert!(!stored
        .unwrap()
        .facts
        .keys()
        .any(|key| key.as_str().starts_with("quality.")));
}

#[test]
fn quality_scores_obey_fact_filters_and_point_in_time_exclusion() {
    let (store, _dir) = fixture(true);
    seed_grades(&store);
    for args in [
        json!({"at_ms":1000}),
        json!({"fact_prefixes":["rate."]}),
        json!({"fact_prefixes":[]}),
        json!({"provider_id":"reseller","model_id":"shared","at_ms":1000}),
    ] {
        let response = get(&store, args);
        assert!(response.quality_scores.is_none());
        assert!(response
            .models
            .values()
            .all(|facts| !facts.keys().any(|key| key.starts_with("quality."))));
    }
    for prefixes in [json!(["quality."]), json!(["quality.elo"])] {
        let response = get(&store, json!({"fact_prefixes":prefixes}));
        assert_eq!(
            response.models["reseller/own"]["quality.elo"].to_string(),
            "1000.00"
        );
        assert_eq!(
            response.models["reseller/shared"]["quality.elo"].to_string(),
            "1284"
        );
        assert!(response
            .models
            .values()
            .all(|facts| facts.keys().all(|key| key.starts_with("quality."))));
        assert!(response.quality_scores.is_some());
    }
}

#[test]
fn quality_scores_empty_table_serves_no_facts_or_disclosure() {
    let (store, _dir) = fixture(true);
    let response = get(&store, json!({}));
    assert!(response.quality_scores.is_none());
    assert!(response
        .models
        .values()
        .all(|facts| !facts.keys().any(|key| key.starts_with("quality."))));
    assert!(serde_json::to_value(response)
        .unwrap()
        .get("quality_scores")
        .is_none());
    let filtered = get(&store, json!({"fact_prefixes":["quality."]}));
    assert!(filtered.models.is_empty());
    assert!(filtered.curated.is_empty());
    assert!(filtered.aliased.is_empty());
    assert!(filtered.quality_scores.is_none());
}

#[test]
fn quality_scores_aliases_require_canonical_identity_and_planes_match_plain_reads() {
    for canonical in [true, false] {
        let (store, _dir) = fixture(canonical);
        seed_grades(&store);
        for auth in [Value::Null, json!("apikey")] {
            let response = get(
                &store,
                json!({"provider_id":"google","model_id":"antigravity-gemini-4-argon","auth_method":auth,"fact_prefixes":["quality."]}),
            );
            let facts = &response.models["google/antigravity-gemini-4-argon"];
            if canonical {
                assert_eq!(facts["quality.elo"].to_string(), "1284");
                assert!(response.quality_scores.is_some());
            } else {
                assert!(facts.is_empty());
                assert!(response.quality_scores.is_none());
            }
        }
    }
}

#[test]
fn quality_scores_pages_without_graded_rows_omit_disclosure() {
    let (store, _dir) = fixture(false);
    seed_grades(&store);
    let mut response = get(&store, json!({}));
    scope_page(
        &mut response,
        &BTreeSet::from(["google".into(), "reseller".into()]),
    );
    let first = page_catalog(response.clone(), None, 1).unwrap();
    assert!(first.models.keys().all(|key| key.starts_with("google/")));
    assert!(first.quality_scores.is_none());
    let cursor = parse_page_token(first.next_page_token.as_deref().unwrap()).unwrap();
    let second = page_catalog(response.clone(), Some(&cursor), 1).unwrap();
    assert!(second.models.contains_key("reseller/own"));
    assert!(second.quality_scores.is_some());
    scope_page(&mut response, &BTreeSet::from(["missing".into()]));
    assert!(response.quality_scores.is_none());
}
