use super::*;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::BoundaryKind;
use fusiform_store::NewEra;
use serde_json::{json, Value};

const GRADES: &str = r#"{"graded_by":"operator","graded_at_ms":123,"purpose":"routing","rows":[
    {"model":"lab/shared","elo":1284,"eq":1599.8},
    {"model":"reseller/own","elo":1000.00,"eq":null},
    {"model":"reseller/shared","elo":99,"eq":99},
    {"model":"google/antigravity-gemini-4-argon","elo":99,"eq":99}
]}"#;

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

#[test]
fn quality_scores_join_served_canonical_identity_and_own_key_without_storage() {
    let _grades = crate::quality_scores::plant(GRADES);
    let (store, _dir) = fixture(true);
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
    assert!(!response.models["reseller/own"].contains_key("quality.eq"));
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
    let _grades = crate::quality_scores::plant(GRADES);
    let (store, _dir) = fixture(true);
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
    let _grades = crate::quality_scores::plant(
        r#"{"graded_by":"operator","graded_at_ms":0,"purpose":"routing","rows":[]}"#,
    );
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
    let _grades = crate::quality_scores::plant(GRADES);
    for canonical in [true, false] {
        let (store, _dir) = fixture(canonical);
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
    let _grades = crate::quality_scores::plant(GRADES);
    let (store, _dir) = fixture(false);
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
