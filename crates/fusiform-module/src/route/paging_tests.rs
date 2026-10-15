use super::*;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, Correction, ObservationOutcome, TokenClass};
use fusiform_store::{ingest::plan_ingest, NewEra, NewObservation};
use serde_json::{json, Value};

fn empty_store() -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("paging.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    (store, dir)
}

fn observation(store: &CatalogStore, at: i64) -> i64 {
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(at),
            outcome: ObservationOutcome::Seeded,
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap()
}

fn fixture() -> (CatalogStore, tempfile::TempDir) {
    let (store, dir) = empty_store();
    let mut upstream = serde_json::Map::new();
    for i in 0..6 {
        let provider = format!("p{i:02}");
        upstream.insert(
            provider.clone(),
            json!({
                "id": provider, "name": provider,
                "models": {"m": {"id":"m", "name":"Synthetic model", "family":"x".repeat(400),
                    "limit":{"context":1000}, "cost":{"input":1,"output":2}}}
            }),
        );
    }
    upstream.insert(
        "google".into(),
        json!({
            "id":"google", "name":"Synthetic alias target", "models":{
                "gemini-3.8-flash":{"id":"gemini-3.8-flash","name":"Synthetic model",
                    "family":"gemini","limit":{"context":1000},"cost":{"input":1,"output":2}}}
        }),
    );
    let catalog =
        fusiform_core::normalize::normalize_models_dev(&serde_json::to_vec(&upstream).unwrap())
            .unwrap()
            .catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    let obs = observation(&store, 1000);
    store.advance_catalog_version(obs, Timestamp(1000)).unwrap();
    for i in 0..6 {
        let provider = format!("p{i:02}");
        store
            .append_eras(&[
                NewEra {
                    source: SourceId::ModelsDev,
                    provider_id: provider.clone(),
                    model_id: "m".into(),
                    fact_key: FactKey::rate(TokenClass::Input),
                    value_json: r#"{"state":"stated_zero"}"#.into(),
                    boundary_at: Timestamp(8000),
                    boundary_kind: BoundaryKind::Corrected(Correction {
                        fields: vec![FieldId::Rate {
                            class: TokenClass::Input,
                        }],
                        affected_from: Timestamp(2000),
                        affected_until: Timestamp(8000),
                        reason: "synthetic correction".into(),
                    }),
                    observation_id: None,
                },
                NewEra {
                    source: SourceId::ModelsDev,
                    provider_id: provider,
                    model_id: "m".into(),
                    fact_key: FactKey::rate(TokenClass::Output),
                    value_json: r#"{"state":"stated_zero"}"#.into(),
                    boundary_at: Timestamp(8000),
                    boundary_kind: BoundaryKind::Observed,
                    observation_id: None,
                },
            ])
            .unwrap();
    }
    let obs = observation(&store, 8000);
    store.advance_catalog_version(obs, Timestamp(8000)).unwrap();
    (store, dir)
}

fn get(store: &CatalogStore, args: Value, budget: usize) -> CatalogGetResponse {
    serve_catalog_get_with_budget(store, &serde_json::to_vec(&args).unwrap(), budget).unwrap()
}

#[test]
fn pages_cover_whole_providers_in_order_and_scope_disclosures() {
    let (store, _dir) = fixture();
    for at in [None, Some(5000)] {
        let mut args = json!({});
        if let Some(at) = at {
            args["at_ms"] = at.into();
        }
        let full = get(&store, args.clone(), usize::MAX);
        if at.is_some() {
            assert_eq!(full.withheld.len(), 6);
            assert_eq!(full.uncertain.len(), 6);
        } else {
            assert!(!full.curated.is_empty());
            assert!(!full.aliased.is_empty());
        }
        args["paged"] = true.into();
        let budget = 2600;
        let mut models = BTreeMap::new();
        let mut seen = Vec::new();
        let mut withheld = Vec::new();
        let mut uncertain = Vec::new();
        let mut curated = Vec::new();
        let mut aliased = Vec::new();
        let mut pages = 0;
        let mut resolved = None;
        loop {
            let page = get(&store, args.clone(), budget);
            assert_eq!(page.catalog_version, full.catalog_version);
            assert_eq!(
                *resolved.get_or_insert(page.resolved_at_ms),
                page.resolved_at_ms
            );
            let providers: BTreeSet<_> = page
                .models
                .keys()
                .map(|m| model_provider(m).to_string())
                .collect();
            assert!(!providers.is_empty());
            assert!(serde_json::to_vec(&page).unwrap().len() <= budget || providers.len() == 1);
            assert_eq!(
                serde_json::to_vec(&page).unwrap(),
                serde_json::to_vec(&ToolResponse::Catalog(page.clone())).unwrap()
            );
            if let Some(token) = &page.next_page_token {
                let cursor = parse_page_token(token).unwrap();
                let all: BTreeSet<_> = full
                    .models
                    .keys()
                    .map(|m| model_provider(m).to_string())
                    .collect();
                let next = all.iter().find(|p| *p > &cursor.provider).unwrap();
                let mut expanded_providers = providers.clone();
                expanded_providers.insert(next.clone());
                let mut expanded = full.clone();
                expanded.resolved_at_ms = page.resolved_at_ms;
                scope_page(&mut expanded, &expanded_providers);
                expanded.next_page_token = all.iter().any(|p| p > next).then(|| {
                    format!(
                        "v1:{}:{}:{}",
                        page.catalog_version, page.resolved_at_ms, next
                    )
                });
                assert!(
                    serde_json::to_vec(&expanded).unwrap().len() > budget,
                    "greedy packing must not leave room for the next provider"
                );
            }
            assert!(page
                .withheld
                .iter()
                .all(|r| providers.contains(model_provider(&r.model))));
            assert!(page
                .uncertain
                .iter()
                .all(|r| providers.contains(&r.provider_id)));
            assert!(page
                .curated
                .iter()
                .all(|r| providers.contains(model_provider(&r.model))));
            assert!(page
                .retired
                .iter()
                .all(|r| providers.contains(model_provider(&r.model))));
            assert!(page
                .aliased
                .iter()
                .all(|r| providers.contains(model_provider(&r.model))));
            assert!(page
                .billing_rules
                .iter()
                .all(|r| providers.contains(model_provider(&r.model))));
            seen.extend(providers);
            for (model, facts) in page.models {
                assert!(models.insert(model, facts).is_none());
            }
            withheld.extend(page.withheld);
            uncertain.extend(page.uncertain);
            curated.extend(page.curated);
            aliased.extend(page.aliased);
            pages += 1;
            match page.next_page_token {
                Some(token) => args["page_token"] = token.into(),
                None => break,
            }
        }
        assert!(pages >= 2);
        assert!(
            seen.windows(2).all(|pair| pair[0] < pair[1]),
            "providers exactly once in order"
        );
        assert_eq!(
            serde_json::to_vec(&models).unwrap(),
            serde_json::to_vec(&full.models).unwrap()
        );
        assert_eq!(withheld, full.withheld);
        assert_eq!(uncertain, full.uncertain);
        curated.sort_by(|a, b| a.model.cmp(&b.model));
        let mut full_curated = full.curated;
        full_curated.sort_by(|a, b| a.model.cmp(&b.model));
        assert_eq!(curated, full_curated);
        aliased.sort_by(|a, b| a.model.cmp(&b.model));
        let mut full_aliased = full.aliased;
        full_aliased.sort_by(|a, b| a.model.cmp(&b.model));
        assert_eq!(aliased, full_aliased);
    }
}

#[test]
fn disclosure_only_providers_survive_and_oversized_providers_are_not_split() {
    let (store, _dir) = fixture();
    let full = get(
        &store,
        json!({"at_ms":5000,"fact_prefixes":["rate.input"]}),
        usize::MAX,
    );
    assert_eq!(full.withheld.len(), 6);
    let mut args = json!({"paged":true,"at_ms":5000,"fact_prefixes":["rate.input"]});
    let mut withheld = Vec::new();
    let mut pages = 0;
    loop {
        let page = get(&store, args.clone(), 1);
        let providers: BTreeSet<_> = page
            .models
            .keys()
            .map(|m| model_provider(m))
            .chain(page.withheld.iter().map(|r| model_provider(&r.model)))
            .collect();
        assert_eq!(
            providers.len(),
            1,
            "a provider exceeding the budget must be returned alone"
        );
        assert!(serde_json::to_vec(&page).unwrap().len() > 1);
        withheld.extend(page.withheld);
        pages += 1;
        match page.next_page_token {
            Some(token) => args["page_token"] = token.into(),
            None => break,
        }
    }
    assert!(pages >= 6);
    assert_eq!(withheld, full.withheld);
}

#[test]
fn continuation_after_era_write_is_refused() {
    let (store, _dir) = fixture();
    let first = get(&store, json!({"paged":true}), 2600);
    let token = first.next_page_token.unwrap();
    let catalog = fusiform_core::normalize::normalize_models_dev(
        br#"{"new":{"id":"new","name":"new","models":{"m":{"id":"m","name":"new"}}}}"#,
    )
    .unwrap()
    .catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(9000), BoundaryKind::Seed, None).unwrap();
    assert!(!plan.eras.is_empty());
    store.append_eras(&plan.eras).unwrap();
    let obs = observation(&store, 9000);
    store.advance_catalog_version(obs, Timestamp(9000)).unwrap();
    let error = serve_catalog_get_with_budget(
        &store,
        &serde_json::to_vec(&json!({"paged":true,"page_token":token})).unwrap(),
        2600,
    )
    .unwrap_err();
    assert_eq!(error.code, fusiform_protocol::CODE_REFUSED);
    assert!(error.message.contains("restart from page one"));
}

#[test]
fn invalid_paging_requests_are_bad_requests() {
    let (store, _dir) = fixture();
    for args in [
        json!({"page_token":"v1:8000:5000:p00"}),
        json!({"paged":true,"provider_id":"p00"}),
        json!({"paged":true,"model_id":"m"}),
        json!({"paged":true,"auth_method":"apikey"}),
    ] {
        let error = serve_catalog_get(&store, &serde_json::to_vec(&args).unwrap()).unwrap_err();
        assert_eq!(error.code, fusiform_protocol::CODE_BAD_REQUEST);
    }
    for token in [
        "",
        "p00",
        "v2:8000:5000:p00",
        "v1:abc:5000:p00",
        "v1:8000:05000:p00",
        "v1:8000:5000:",
        "v1:8000:5000",
        "v1:9223372036854775808:5000:p00",
    ] {
        let error = serve_catalog_get(
            &store,
            &serde_json::to_vec(&json!({"paged":true,"page_token":token})).unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.code, fusiform_protocol::CODE_BAD_REQUEST, "{token}");
    }
    assert_eq!(
        parse_page_token("v1:8000:5000:provider:with:colons")
            .unwrap()
            .provider,
        "provider:with:colons"
    );
}

#[test]
fn embedded_seed_paging_measurement_and_unpaged_compatibility() {
    let (store, _dir) = empty_store();
    crate::seed::seed_if_empty(&store).unwrap();
    let full = get(&store, json!({}), CATALOG_PAGE_BYTES);
    let full_bytes = serde_json::to_vec(&full).unwrap();
    assert!(
        full_bytes.len() > 4 * 1024 * 1024,
        "control: unpaged reads still serve above the future frame limit"
    );
    let explicit = get(&store, json!({"paged":false}), 1);
    let mut expected = full.clone();
    expected.resolved_at_ms = explicit.resolved_at_ms;
    assert_eq!(
        serde_json::to_vec(&expected).unwrap(),
        serde_json::to_vec(&explicit).unwrap()
    );
    let mut args = json!({"paged":true});
    let mut models = BTreeMap::new();
    let mut pages = 0;
    let mut largest = 0;
    let mut resolved = None;
    loop {
        let page = get(&store, args.clone(), CATALOG_PAGE_BYTES);
        let size = serde_json::to_vec(&page).unwrap().len();
        assert!(size <= CATALOG_PAGE_BYTES);
        assert_eq!(
            *resolved.get_or_insert(page.resolved_at_ms),
            page.resolved_at_ms
        );
        largest = largest.max(size);
        pages += 1;
        models.extend(page.models);
        match page.next_page_token {
            Some(token) => args["page_token"] = token.into(),
            None => break,
        }
    }
    assert_eq!(models, full.models);
    assert!(pages >= 2);
    println!(
        "embedded seed: unpaged={} bytes, pages={pages}, largest={largest} bytes",
        full_bytes.len()
    );
}
