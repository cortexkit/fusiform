use std::collections::{BTreeMap, BTreeSet};

use fusiform_core::SourceId;
use fusiform_module::route::{serve_catalog_get, serve_history, serve_tool_call};
use fusiform_protocol::{HistoryRefusalReason, HistorySegment, HistoryState};
use fusiform_store::CatalogStore;
use serde_json::{json, Value};

fn provider_label_fixture() -> (CatalogStore, tempfile::TempDir) {
    use fusiform_core::{BoundaryKind, Timestamp};
    use fusiform_store::{FactKey, NewEra};

    let (store, dir) = super::golden_payload::store();
    let row = |key: &str, value: &str, at| NewEra {
        source: SourceId::ModelsDev,
        provider_id: "fixture-label".into(),
        model_id: "upstream".into(),
        fact_key: FactKey::from_stored(key.into()),
        value_json: value.into(),
        boundary_at: Timestamp(at),
        boundary_kind: BoundaryKind::Asserted,
        observation_id: None,
    };
    store
        .append_eras(&[
            row("existence", "\"present\"", BOOT),
            row("provider.name", "\"Fixture\"", BOOT),
            row("existence", "\"absent\"", BOOT + 100),
        ])
        .unwrap();
    (store, dir)
}

fn label_card() -> fusiform_store::NewCuratedModel {
    fusiform_store::NewCuratedModel {
        provider_id: "fixture-label".into(),
        model_id: "curated".into(),
        facts: [("limit.context".into(), json!(100))].into(),
        auth_methods: Vec::new(),
        source_ref: "https://example.test/card".into(),
        established_by: None,
        established_at_ms: BOOT,
        review_by_ms: BOOT + 1_000,
    }
}

#[test]
fn served_history_respects_a_dated_alias_before_a_curated_model() {
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
    use fusiform_core::{ObservationOutcome, Timestamp};
    use fusiform_store::{ImportedTables, NewObservation, ReleaseArrival, StoreRelease};

    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open_for_store(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_str().unwrap().into(),
        },
    })
    .unwrap();
    let aliases = json!({"aliases":[{
        "provider_id":"fixture-label", "model_id":"curated",
        "target":{"provider_id":"fixture-label", "model_id":"upstream"},
        "basis":"the fixture originally forwarded the request", "source_ref":"https://example.test/alias",
        "established_at_ms":BOOT, "review_by_ms":BOOT + 1000
    }]}).to_string();
    let release = |seq, at, doc: Option<String>| {
        let row = json!({"document":"model-aliases", "document_json":doc, "recorded_at_ms":at});
        ReleaseArrival {
            published_at_ms: at,
            release: StoreRelease {
                release: seq,
                state_hash: "a".repeat(64),
                curation_hash: "b".repeat(64),
                applied_at_ms: at,
            },
            rows: ImportedTables::from([(
                "store_document_era".into(),
                vec![row.as_object().unwrap().clone()],
            )]),
            upstream: None,
        }
    };
    store
        .apply_store_releases(
            Some(&ImportedTables::new()),
            &[
                release(1, BOOT, Some(aliases)),
                release(2, BOOT + 100, None),
            ],
        )
        .unwrap();
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(BOOT),
            outcome: ObservationOutcome::Seeded,
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let mut card = label_card();
    // Write a new card at the transition so history samples the instant
    // when the alias ends and the curated card becomes the dated answer.
    store
        .append_curated_models(&[card.clone()], &[], BOOT)
        .unwrap();
    card.facts.insert("limit.context".into(), json!(200));
    store
        .append_curated_models(&[card], &[], BOOT + 100)
        .unwrap();
    let read = |at| {
        serve_catalog_get(
            &store,
            &serde_json::to_vec(&json!({
                "provider_id":"fixture-label", "model_id":"curated", "at_ms":at
            }))
            .unwrap(),
        )
    };
    assert!(read(BOOT)
        .unwrap_err()
        .message
        .contains("aliases apply to current reads only"));
    assert_eq!(
        read(BOOT + 100).unwrap().models["fixture-label/curated"]["limit.context"],
        200
    );
    let history = serve_history(&store, br#"{"provider_id":"fixture-label","model_id":"curated","fact_key":"limit.context","served":true}"#).unwrap();
    let segments = history.served.unwrap();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].reason, Some(HistoryRefusalReason::NoCoverage));
    assert_eq!(segments[0].until_ms, Some(BOOT + 100));
    assert_eq!(segments[1].value, Some(json!(200)));
}

#[test]
fn served_history_ends_provider_label_when_donor_disappears() {
    let (store, _dir) = provider_label_fixture();
    store
        .append_curated_models(&[label_card()], &[], BOOT)
        .unwrap();
    let response = serve_history(&store, br#"{"provider_id":"fixture-label","model_id":"curated","fact_key":"provider.name","served":true}"#).unwrap();
    let segments = response.served.unwrap();
    let label = segments
        .iter()
        .find(|segment| segment.value == Some(json!("Fixture")))
        .unwrap();
    assert_eq!(label.from_ms, BOOT);
    assert_eq!(label.until_ms, Some(BOOT + 100));
    assert_eq!(segments.last().unwrap().state, HistoryState::Absent);
}

#[test]
fn served_history_partitions_provider_label_at_donor_correction_endpoints() {
    use fusiform_core::{BoundaryKind, Correction, FieldId, Timestamp};
    use fusiform_store::{FactKey, NewEra};

    let (store, _dir) = provider_label_fixture();
    store
        .append_curated_models(&[label_card()], &[], BOOT)
        .unwrap();
    store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "fixture-label".into(),
            model_id: "upstream".into(),
            fact_key: FactKey::existence(),
            value_json: "\"absent\"".into(),
            boundary_at: Timestamp(BOOT + 110),
            boundary_kind: BoundaryKind::Corrected(Correction {
                fields: vec![FieldId::Existence],
                affected_from: Timestamp(BOOT + 20),
                affected_until: Timestamp(BOOT + 30),
                reason: "invalid donor presence record".into(),
            }),
            observation_id: None,
        }])
        .unwrap();
    let response = serve_history(&store, br#"{"provider_id":"fixture-label","model_id":"curated","fact_key":"provider.name","served":true}"#).unwrap();
    let segments = response.served.unwrap();
    let at = |instant| segments.iter().find(|s| s.from_ms == instant).unwrap();
    assert_eq!(at(BOOT).until_ms, Some(BOOT + 20));
    assert_eq!(at(BOOT + 20).state, HistoryState::Absent);
    assert_eq!(at(BOOT + 20).until_ms, Some(BOOT + 31));
    assert_eq!(at(BOOT + 31).value, Some(json!("Fixture")));
}

#[test]
fn served_history_refuses_typos_but_covers_removed_curated_models() {
    let (store, _dir) = provider_label_fixture();
    store
        .append_curated_models(&[label_card()], &[], BOOT)
        .unwrap();
    store.append_curated_models(&[], &[], BOOT + 200).unwrap();

    for selector in [
        json!({"fact_key":"limit.context","served":true}),
        json!({"fact_prefix":"limit.","served":true}),
        json!({"fact_prefix":"limit."}),
    ] {
        let request = |model: &str| {
            let mut args = selector.clone();
            args["provider_id"] = json!("fixture-label");
            args["model_id"] = json!(model);
            serde_json::to_vec(&args).unwrap()
        };
        assert_eq!(
            serve_history(&store, &request("typo")).unwrap_err().code,
            "no_coverage"
        );
        let removed = serve_history(&store, &request("curated")).unwrap();
        if let Some(keys) = removed.fact_keys {
            assert_eq!(keys, ["limit.context"]);
        } else {
            assert!(removed
                .served
                .unwrap()
                .iter()
                .any(|segment| segment.value == Some(json!(100))));
        }
    }
}

const BOOT: i64 = 1_800_000_000_000;

#[test]
fn served_history_excludes_judgments_but_dated_get_replays_them() {
    use fusiform_core::{BoundaryKind, Timestamp};
    use fusiform_store::{FactKey, NewEra};

    let (store, _dir) = provider_label_fixture();
    let provider = "fixture-label";
    let model = "upstream";
    let identity = format!("{provider}/{model}");
    for (key, value, at) in [
        ("model.canonical_id", json!(identity), BOOT),
        ("limit.context", json!(100), BOOT + 40),
    ] {
        store
            .append_eras(&[NewEra {
                source: SourceId::ModelsDev,
                provider_id: provider.into(),
                model_id: model.into(),
                fact_key: FactKey::from_stored(key.into()),
                value_json: value.to_string(),
                boundary_at: Timestamp(at),
                boundary_kind: BoundaryKind::Asserted,
                observation_id: None,
            }])
            .unwrap();
    }
    store
        .append_model_mark(&identity, true, "operator", "fixture", BOOT + 20)
        .unwrap();
    store
        .append_model_mark(&identity, false, "operator", "fixture", BOOT + 60)
        .unwrap();
    let args = |mut request: Value| {
        request["provider_id"] = json!(provider);
        request["model_id"] = json!(model);
        serde_json::to_vec(&request).unwrap()
    };
    for (at, expected) in [(19, None), (20, Some(json!(true))), (60, None)] {
        let read = serve_catalog_get(&store, &args(json!({"at_ms": BOOT + at}))).unwrap();
        assert_eq!(
            read.models[&identity].get("quality.decommissioned"),
            expected.as_ref()
        );
    }
    for selector in [
        json!({"fact_key":"quality.decommissioned","served":true}),
        json!({"fact_key":"quality.elo.effort.high"}),
        json!({"fact_prefix":"quality.","served":true}),
        json!({"fact_prefix":"workload.code."}),
    ] {
        let error = serve_history(&store, &args(selector)).unwrap_err();
        assert_eq!(error.code, "bad_request");
        assert!(error.message.contains("quality.grades"), "{error:?}");
    }

    let history = serve_history(&store, &args(json!({"fact_prefix":"","served":true}))).unwrap();
    let keys = history.fact_keys.unwrap();
    assert!(keys.iter().any(|key| key == "limit.context"));
    assert!(keys
        .iter()
        .all(|key| !key.starts_with("quality.") && !key.starts_with("workload.")));
    assert!(!history
        .served_by_key
        .unwrap()
        .contains_key("quality.decommissioned"));
}

// Use the shipped catalog as background load: a tiny store hides queries that
// scan other models once for every boundary of the requested model.
fn oscillating_store() -> (CatalogStore, tempfile::TempDir) {
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
    use fusiform_core::{BoundaryKind, ObservationOutcome, Timestamp};
    use fusiform_store::{ingest::plan_ingest, NewObservation};
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
    let mut document: Value =
        serde_json::from_str(fusiform_core::normalize::legacy_models_dev_snapshot_for_tests())
            .unwrap();
    for index in 0..229 {
        for (model, count) in [
            ("deepseek/deepseek-v4.1-flash", 229),
            ("moonshotai/kimi-k3", 224),
        ] {
            if index < count {
                let row = &mut document["openrouter"]["models"][model];
                assert!(row.is_object(), "seed is missing {model}");
                row["cost"]["input"] = json!(0.1 + (index % 2) as f64 * 0.1);
                row["cost"]["output"] = json!(0.3 + (index % 2) as f64 * 0.2);
            }
        }
        let at = Timestamp(1_000 + index * 1_000);
        let observation = store
            .record_observation(&NewObservation {
                source: SourceId::ModelsDev,
                observed_at: at,
                outcome: ObservationOutcome::Seeded,
                normalized_hash: None,
                raw_hash: None,
                etag: None,
                duration_ms: None,
                detail: None,
            })
            .unwrap();
        let catalog = fusiform_core::normalize_legacy_models_dev_for_tests_with(
            &serde_json::to_vec(&document).unwrap(),
            &fusiform_module::input_audio::evidence(),
        )
        .unwrap()
        .catalog;
        let plan = plan_ingest(
            &store,
            &catalog,
            at,
            BoundaryKind::Asserted,
            Some(observation),
        )
        .unwrap();
        store.append_eras(&plan.eras).unwrap();
    }
    (store, dir)
}

#[test]
#[ignore = "realistic-store timing; run explicitly in release mode"]
fn served_history_many_boundaries_timing() {
    let (store, _dir) = oscillating_store();
    let mut slow = Vec::new();
    for (provider, model, expected_hash) in [
        (
            "openrouter",
            "deepseek/deepseek-v4.1-flash",
            "1ffa586f4000a5671e406f7e3d5e2ad10ba85389b426027f23d6155c036c44ad",
        ),
        (
            "openrouter",
            "moonshotai/kimi-k3",
            "c90083b5a13dd6d062ec480c0a51c240c616c844f5f3a2e4cd6962f362589d20",
        ),
        (
            "anthropic",
            "claude-sonnet-4-5",
            "197064223bdcc5930dfa10d2328a3d0ba172188cdf7a7caf35795a2d9a3f4846",
        ),
    ] {
        let body = serde_json::to_vec(&json!({
            "provider_id":provider, "model_id":model, "fact_prefix":"rate.", "served":true
        }))
        .unwrap();
        let start = std::time::Instant::now();
        let response = serve_history(&store, &body).unwrap();
        let elapsed = start.elapsed();
        let bytes = serde_json::to_vec(&response).unwrap();
        // Captured from the unoptimized resolver, not computed by another
        // invocation of the implementation under test.
        assert_eq!(blake3::hash(&bytes).to_hex().as_str(), expected_hash);
        let segments: usize = response
            .served_by_key
            .as_ref()
            .unwrap()
            .values()
            .map(Vec::len)
            .sum();
        eprintln!("history timing {provider}/{model}: {elapsed:?}, {} boundaries, {segments} segments, {} bytes, blake3 {}",
            store.model_history_boundaries(SourceId::ModelsDev, provider, model).unwrap().len(),
            bytes.len(), blake3::hash(&bytes));
        if elapsed.as_secs_f64() >= 1.0 {
            slow.push(format!("{provider}/{model}: {elapsed:?}"));
        }
    }
    assert!(slow.is_empty(), "slow histories: {slow:?}");
}

#[test]
fn served_history_many_boundaries_with_all_dated_layers() {
    use fusiform_core::{BoundaryKind, Correction, FieldId, Timestamp, TokenClass};
    use fusiform_protocol::HistorySource;
    use fusiform_store::{FactKey, NewCacheLifetime, NewEra};
    let (store, _dir) = super::cache_lifetimes::store(None);
    overlays(&store);
    let provider = "typesafe";
    let model = "jev-1.13.0";
    let rate = |units| json!({"state":"priced", "currency":"USD", "exponent":9, "units":units});
    let era = |key: &str, value: Value, at| NewEra {
        source: SourceId::ModelsDev,
        provider_id: provider.into(),
        model_id: model.into(),
        fact_key: FactKey::from_stored(key.into()),
        value_json: value.to_string(),
        boundary_at: Timestamp(at),
        boundary_kind: BoundaryKind::Asserted,
        observation_id: None,
    };
    let start = BOOT + 1_000;
    let mut eras = vec![era("existence", json!("present"), start)];
    for index in 0..229 {
        eras.push(era(
            "rate.input",
            rate(1_000 + index % 2 * 1_000),
            start + index * 1_000,
        ));
        eras.push(era(
            "rate.cache_write",
            rate(1_250 + index % 2 * 1_250),
            start + index * 1_000,
        ));
    }
    eras.push(era("existence", json!("absent"), start + 229_000));
    let mut correction = era("rate.input", rate(100), start + 240_000);
    correction.boundary_kind = BoundaryKind::Corrected(Correction {
        fields: vec![FieldId::Rate {
            class: TokenClass::Input,
        }],
        affected_from: Timestamp(start + 4_000),
        affected_until: Timestamp(start + 6_500),
        reason: "synthetic correction window".into(),
    });
    eras.push(correction);
    store.append_eras(&eras).unwrap();
    let mut claim = store
        .fact_overrides_at(BOOT, None)
        .unwrap()
        .into_iter()
        .find_map(|era| era.claim)
        .unwrap();
    claim.provider_id = provider.into();
    claim.model_id = model.into();
    claim.fact_key = "rate.cached_input_audio".into();
    claim.value = rate(42);
    claim.replaces = Value::Null;
    store
        .append_fact_overrides(&[claim], start + 2_000)
        .unwrap();
    let rule = store
        .cache_lifetimes_at(i64::MAX, Some("anthropic"))
        .unwrap()
        .into_iter()
        .find(|era| era.model_id == "claude-opus-5-5")
        .unwrap()
        .rule
        .unwrap();
    store
        .append_cache_lifetimes(
            &[NewCacheLifetime {
                provider_id: provider.into(),
                model_id: model.into(),
                rule,
            }],
            start + 3_000,
        )
        .unwrap();
    store.append_cache_lifetimes(&[], start + 230_000).unwrap();
    let mut curated = store
        .curated_model_eras(provider, model)
        .unwrap()
        .last()
        .unwrap()
        .claim
        .clone()
        .unwrap();
    curated.facts.insert("rate.input".into(), rate(999));
    store
        .append_curated_models(&[curated], &[], start + 231_000)
        .unwrap();
    let response = serve_history(
        &store,
        &serde_json::to_vec(&json!({
            "provider_id":provider, "model_id":model, "fact_prefix":"rate.", "served":true
        }))
        .unwrap(),
    )
    .unwrap();
    let mut sources = BTreeSet::new();
    let mut refusals = BTreeSet::new();
    let mut checked = 0;
    for (key, segments) in response.served_by_key.unwrap() {
        for segment in segments {
            if let Some(source) = segment.source {
                sources.insert(format!("{source:?}"));
            }
            if let Some(reason) = segment.reason {
                refusals.insert(format!("{reason:?}"));
            }
            assert_parity(&store, provider, model, &key, &segment, segment.from_ms);
            if let Some(until) = segment.until_ms {
                assert_parity(&store, provider, model, &key, &segment, until - 1);
            }
            checked += 1;
        }
    }
    assert!(checked > 450, "only {checked} segments checked");
    for source in [
        HistorySource::Upstream,
        HistorySource::Override,
        HistorySource::Curated,
        HistorySource::CacheLifetime,
    ] {
        assert!(
            sources.contains(&format!("{source:?}")),
            "missing {source:?}"
        );
    }
    assert_eq!(
        refusals,
        BTreeSet::from(["Correction".into(), "NoCoverage".into()])
    );
    eprintln!("many-boundary layered parity: {checked} segments");
}

#[test]
fn served_history_clamped_curated_boundary_matches_sql_tie_break() {
    let (store, _dir) = super::cache_lifetimes::store(None);
    overlays(&store);
    let provider = "typesafe";
    let model = "jev-1.13.0";
    let boundary = BOOT + 1_000;
    let mut claim = store
        .curated_model_eras(provider, model)
        .unwrap()
        .last()
        .unwrap()
        .claim
        .clone()
        .unwrap();
    claim.facts.get_mut("rate.input").unwrap()["units"] = json!(111_000_000);
    store
        .append_curated_models(&[claim.clone()], &[], boundary)
        .unwrap();

    // Upstream fact eras cannot tie: their fact/instant key is unique. A
    // regressing boot clock can clamp curated claims to the same boundary.
    // SQL chooses the greatest (boundary, id); the prepared read reverses the
    // history's ascending (boundary, id) order, choosing that same newest id.
    claim.facts.get_mut("rate.input").unwrap()["units"] = json!(222_000_000);
    store
        .append_curated_models(&[claim], &[], boundary - 500)
        .unwrap();
    let eras = store.curated_model_eras(provider, model).unwrap();
    let tied: Vec<_> = eras
        .iter()
        .filter(|era| era.boundary_at_ms == boundary)
        .collect();
    assert_eq!(
        tied.len(),
        2,
        "the fixture must retain both equal-boundary claims"
    );
    assert_eq!(
        tied[0].claim.as_ref().unwrap().facts["rate.input"]["units"],
        111_000_000
    );
    assert_eq!(
        tied[1].claim.as_ref().unwrap().facts["rate.input"]["units"],
        222_000_000
    );

    let before = dated_get_with_sql_parity(&store, provider, model, boundary - 1).unwrap();
    assert_eq!(
        before.models[&format!("{provider}/{model}")]["rate.input"]["units"],
        42_000_000
    );
    for at in [boundary, boundary + 1] {
        let read = dated_get_with_sql_parity(&store, provider, model, at).unwrap();
        assert_eq!(
            read.models[&format!("{provider}/{model}")]["rate.input"]["units"],
            222_000_000
        );
    }
    for segment in history(&store, provider, model, "rate.input") {
        assert_parity(
            &store,
            provider,
            model,
            "rate.input",
            &segment,
            segment.from_ms,
        );
        if let Some(until) = segment.until_ms {
            assert_parity(&store, provider, model, "rate.input", &segment, until - 1);
        }
    }
}

fn overlays(store: &CatalogStore) {
    fusiform_module::fact_overrides::apply_to_store(
        store,
        fusiform_module::fact_overrides::overrides(),
        BOOT,
    )
    .unwrap();
    let mut doc: Value =
        serde_json::from_str(include_str!("../../data/curated-models.json")).unwrap();
    let mut archived: Value =
        serde_json::from_str(include_str!("../../fixtures/retired-curated-models.json")).unwrap();
    doc["models"]
        .as_array_mut()
        .unwrap()
        .append(archived["models"].as_array_mut().unwrap());
    let (mut table, rejects) =
        fusiform_module::curated_models::load_table_reporting_rejects(&doc.to_string());
    assert!(rejects.is_empty(), "{rejects:?}");
    table.held.clear();
    fusiform_module::curated_models::apply_to_store(store, &table, BOOT).unwrap();
}

fn history(store: &CatalogStore, provider: &str, model: &str, key: &str) -> Vec<HistorySegment> {
    serve_history(
        store,
        &serde_json::to_vec(&json!({
            "provider_id":provider, "model_id":model, "fact_key":key, "served":true
        }))
        .unwrap(),
    )
    .unwrap()
    .served
    .unwrap()
}

fn dated_get_with_sql_parity(
    store: &CatalogStore,
    provider: &str,
    model: &str,
    at: i64,
) -> Result<fusiform_protocol::CatalogGetResponse, fusiform_module::route::RouteError> {
    let named = serve_catalog_get(
        store,
        &serde_json::to_vec(&json!({
            "provider_id":provider, "model_id":model, "at_ms":at
        }))
        .unwrap(),
    );
    // A provider-scoped read still selects its rows and historical overlays in
    // SQL. Both paths then use the same renderer, so this comparison checks the
    // prepared resolver rather than comparing two uses of that resolver.
    let sql = serve_catalog_get(
        store,
        &serde_json::to_vec(&json!({"provider_id":provider, "at_ms":at})).unwrap(),
    );
    let label = format!("{provider}/{model} at {at}");
    match sql {
        Err(error) => {
            let named_error = named
                .as_ref()
                .expect_err("named read succeeded while SQL refused");
            assert_eq!(named_error.code, error.code, "SQL refusal for {label}");
        }
        Ok(sql) => {
            let identity = format!("{provider}/{model}");
            let read = named.as_ref().ok();
            assert_eq!(
                read.and_then(|read| read.models.get(&identity)),
                sql.models.get(&identity),
                "SQL model row for {label}"
            );
            let withheld: Vec<_> = read
                .into_iter()
                .flat_map(|read| &read.withheld)
                .filter(|row| row.model == identity)
                .collect();
            let sql_withheld: Vec<_> = sql
                .withheld
                .iter()
                .filter(|row| row.model == identity)
                .collect();
            assert_eq!(
                serde_json::to_value(withheld).unwrap(),
                serde_json::to_value(sql_withheld).unwrap(),
                "SQL withholding for {label}"
            );
            let uncertain: Vec<_> = read
                .into_iter()
                .flat_map(|read| &read.uncertain)
                .filter(|row| row.provider_id == provider && row.model_id == model)
                .collect();
            let sql_uncertain: Vec<_> = sql
                .uncertain
                .iter()
                .filter(|row| row.provider_id == provider && row.model_id == model)
                .collect();
            assert_eq!(
                serde_json::to_value(uncertain).unwrap(),
                serde_json::to_value(sql_uncertain).unwrap(),
                "SQL uncertainty for {label}"
            );
        }
    }
    named
}

fn assert_parity(
    store: &CatalogStore,
    provider: &str,
    model: &str,
    key: &str,
    segment: &HistorySegment,
    at: i64,
) {
    let read = dated_get_with_sql_parity(store, provider, model, at);
    let label = format!("{provider}/{model} {key} at {at}");
    match read {
        Err(error) => {
            assert_eq!(
                error.code, "no_coverage",
                "unexpected dated refusal for {label}: {error:?}"
            );
            assert_eq!(segment.state, HistoryState::Refused, "{label}");
            assert_eq!(
                segment.reason,
                Some(HistoryRefusalReason::NoCoverage),
                "{label}"
            );
            assert!(
                segment.value.is_none() && segment.correction.is_none(),
                "{label}"
            );
        }
        Ok(read) => {
            if !read
                .curated
                .iter()
                .any(|row| row.model == format!("{provider}/{model}"))
            {
                let expected: Vec<_> = store
                    .uncertain_facts_at(SourceId::ModelsDev, fusiform_core::Timestamp(at))
                    .unwrap()
                    .into_iter()
                    .filter(|((p, m, _), _)| p == provider && m == model)
                    .map(|((provider_id, model_id, key), (prior, boundary))| {
                        fusiform_protocol::UncertainFactWire {
                            provider_id,
                            model_id,
                            fact_key: key.as_str().into(),
                            superseded_after_ms: prior.0,
                            superseded_by_ms: boundary.0,
                        }
                    })
                    .collect();
                assert_eq!(
                    serde_json::to_value(&read.uncertain).unwrap(),
                    serde_json::to_value(expected).unwrap(),
                    "uncertainty for {label}"
                );
            }
            if let Some(withheld) = read
                .withheld
                .iter()
                .find(|w| w.model == format!("{provider}/{model}") && w.fact_key == key)
            {
                assert_eq!(segment.state, HistoryState::Refused, "{label}");
                assert_eq!(
                    segment.reason,
                    Some(HistoryRefusalReason::Correction),
                    "{label}"
                );
                assert_eq!(
                    segment.correction.as_ref(),
                    withheld.corrections.first(),
                    "{label}"
                );
                assert_eq!(segment.corrections, withheld.corrections, "{label}");
                assert!(segment.value.is_none(), "{label}");
            } else {
                let value = read
                    .models
                    .get(&format!("{provider}/{model}"))
                    .and_then(|facts| facts.get(key));
                assert_eq!(
                    segment.state,
                    if value.is_some() {
                        HistoryState::Value
                    } else {
                        HistoryState::Absent
                    },
                    "{label}"
                );
                assert_eq!(segment.value.as_ref(), value, "{label}");
                assert!(
                    segment.reason.is_none() && segment.correction.is_none(),
                    "{label}"
                );
            }
        }
    }
}

#[test]
fn history_retains_both_applicable_correction_disclosures() {
    use fusiform_core::{BoundaryKind, Correction, FieldId, Timestamp, TokenClass};
    use fusiform_store::{FactKey, NewEra};

    let (store, _dir) = super::golden_payload::store();
    let provider = "anthropic";
    let model = "claude-sonnet-4-5";
    let key = FactKey::rate(TokenClass::Input);
    let source = SourceId::ModelsDev;
    let now = Timestamp(10_000);
    let current = store
        .recorded_value_at(source, provider, model, &key, now)
        .unwrap()
        .unwrap();
    store
        .append_eras(&[NewEra {
            source,
            provider_id: provider.into(),
            model_id: model.into(),
            fact_key: key.clone(),
            value_json: current.value_json,
            boundary_at: now,
            observation_id: None,
            boundary_kind: BoundaryKind::Corrected(Correction {
                fields: vec![FieldId::Rate {
                    class: TokenClass::Input,
                }],
                affected_from: Timestamp(5000),
                affected_until: Timestamp(6000),
                reason: "independent defect".into(),
            }),
        }])
        .unwrap();
    let exact = serve_history(&store, br#"{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","fact_key":"rate.input","served":true}"#).unwrap();
    let prefix = serve_history(&store, br#"{"provider_id":"anthropic","model_id":"claude-sonnet-4-5","fact_prefix":"rate.","served":true}"#).unwrap();
    let exact = exact.served.unwrap();
    assert_eq!(exact, prefix.served_by_key.unwrap()[key.as_str()]);
    let refused: Vec<_> = exact
        .iter()
        .filter(|s| s.reason == Some(HistoryRefusalReason::Correction))
        .collect();
    let windows: Vec<_> = refused.iter().map(|s| (s.from_ms, s.until_ms)).collect();
    assert_eq!(
        windows,
        [(3000, Some(5000)), (5000, Some(6001)), (6001, Some(9001))]
    );
    let overlap = serde_json::to_value(refused[1]).unwrap();
    let corrections = overlap["corrections"].as_array().unwrap();
    assert_eq!(corrections.len(), 2);
    assert_eq!(corrections[0], overlap["correction"]);
    assert_eq!(corrections[1]["reason"], "independent defect");
    for segment in refused {
        assert_parity(
            &store,
            provider,
            model,
            key.as_str(),
            segment,
            segment.from_ms,
        );
    }
}

pub(super) fn golden_cases() -> serde_json::Map<String, Value> {
    let (ordinary, _ordinary_dir) = super::golden_payload::store();
    let (cache, _cache_dir) = super::cache_lifetimes::store(None);
    let (speed, _speed_dir) = super::golden_payload::speed_twin_store();
    let (image, _image_dir) = super::fact_overrides::historical_flash_store();
    let mut cases = serde_json::Map::new();
    for (label, store, arguments) in [
        (
            "catalog.history — served upstream rate",
            &ordinary,
            json!({"provider_id":"anthropic", "model_id":"claude-sonnet-4-5", "fact_key":"rate.input", "served":true}),
        ),
        (
            "catalog.history — served override addition",
            &image,
            json!({"provider_id":"google", "model_id":"gemini-3.1-flash-image", "fact_key":"rate.output_image", "served":true}),
        ),
        (
            "catalog.history — served cache lifetime",
            &cache,
            json!({"provider_id":"anthropic", "model_id":"claude-opus-5-5", "fact_key":"rate.cache_write.ttl.1h", "served":true}),
        ),
        (
            "catalog.history — historical rate keys",
            &image,
            json!({"provider_id":"google", "model_id":"gemini-3.1-flash-image", "fact_prefix":"rate.output", "served":true}),
        ),
        (
            "catalog.history — served Moonshot Highspeed rate",
            &speed,
            json!({"provider_id":"moonshotai", "model_id":"kimi-k3.1-pro", "fact_key":"rate.input.mode.fast", "served":true}),
        ),
    ] {
        let request = json!({"name":"catalog.history", "arguments":arguments});
        let response = serve_tool_call(store, &serde_json::to_vec(&request).unwrap()).unwrap();
        cases.insert(
            label.into(),
            json!({"request":request, "response":response}),
        );
    }
    cases
}

#[test]
fn served_history_preserves_legacy_golden_bytes_and_validates_selectors() {
    let golden: Value =
        serde_json::from_str(include_str!("../../fixtures/served-payloads.json")).unwrap();
    let (store, _dir) = super::golden_payload::store();
    let mut checked = 0;
    for case in golden.as_object().unwrap().values() {
        let request = &case["request"];
        if request["name"] != "catalog.history"
            || request["arguments"].get("served").is_some()
            || request["arguments"].get("fact_prefix").is_some()
        {
            continue;
        }
        let old = serde_json::to_vec(&case["response"]).unwrap();
        let response = serve_tool_call(&store, &serde_json::to_vec(request).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_vec(&serde_json::to_value(response).unwrap()).unwrap(),
            old
        );
        let mut explicit_false = request.clone();
        explicit_false["arguments"]["served"] = false.into();
        let response =
            serve_tool_call(&store, &serde_json::to_vec(&explicit_false).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_vec(&serde_json::to_value(response).unwrap()).unwrap(),
            old
        );
        checked += 1;
    }
    assert!(checked >= 3, "legacy history cases: {checked}");
    for selector in [
        json!({}),
        json!({"fact_key":"rate.input", "fact_prefix":"rate."}),
    ] {
        let mut request = json!({"provider_id":"anthropic", "model_id":"claude-sonnet-4-5"});
        request
            .as_object_mut()
            .unwrap()
            .extend(selector.as_object().unwrap().clone());
        let error = serve_history(&store, &serde_json::to_vec(&request).unwrap()).unwrap_err();
        assert_eq!(error.code, "bad_request");
        assert!(error.message.contains("exactly one"));
    }
}

#[test]
fn served_history_parity_for_every_golden_store_model_and_rate_key() {
    let fixtures = [
        super::golden_payload::store(),
        super::cache_lifetimes::store(None),
        super::curated_wire::store(),
        super::fact_overrides::historical_flash_store(),
    ];
    let mut models = 0;
    let mut pairs = 0;
    let mut segments_checked = 0;
    let mut endpoint_reads = 0;
    let mut sources = BTreeSet::new();
    let mut refusals = BTreeSet::new();
    let mut replacements = 0;
    for (store, _dir) in fixtures {
        overlays(&store);
        let begins = fusiform_module::route::serve_status(&store, b"{}")
            .unwrap()
            .history_begins_at_ms
            .unwrap();
        assert_eq!(begins, 1_000);
        let conn = rusqlite::Connection::open(store.path()).unwrap();
        // An independent inventory prevents a missing prefix-list entry from
        // silently reducing the parity test's model/key coverage.
        let mut inventory: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
        let mut stmt = conn.prepare(
            "SELECT provider_id, model_id, fact_key FROM era WHERE fact_key LIKE 'rate.%'
             UNION SELECT provider_id, model_id, fact_key FROM fact_override_era WHERE fact_key LIKE 'rate.%'
             UNION SELECT provider_id, model_id, facts.key FROM curated_model_era, json_each(facts_json) facts
             WHERE facts.key LIKE 'rate.%'",
        ).unwrap();
        for row in stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
        {
            let (provider, model, key) = row.unwrap();
            inventory.entry((provider, model)).or_default().insert(key);
        }
        // TTL keys are derived, not stored as fact keys. Ask real dated reads
        // at every recorded rule boundary as well as the final instant.
        let mut stmt = conn
            .prepare("SELECT provider_id, model_id, boundary_at_ms FROM cache_lifetime_era")
            .unwrap();
        for row in stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
        {
            let (provider, model, boundary) = row.unwrap();
            for at in [boundary, i64::MAX] {
                if let Ok(read) = serve_catalog_get(
                    &store,
                    &serde_json::to_vec(
                        &json!({"provider_id":provider, "model_id":model, "at_ms":at}),
                    )
                    .unwrap(),
                ) {
                    if let Some(facts) = read.models.get(&format!("{provider}/{model}")) {
                        inventory
                            .entry((provider.clone(), model.clone()))
                            .or_default()
                            .extend(facts.keys().filter(|key| key.starts_with("rate.")).cloned());
                    }
                }
            }
        }
        for ((provider, model), expected_keys) in inventory {
            models += 1;
            let response = serve_history(
                &store,
                &serde_json::to_vec(&json!({
                    "provider_id":provider, "model_id":model, "fact_prefix":"rate.", "served":true
                }))
                .unwrap(),
            )
            .unwrap();
            let keys = response.fact_keys.unwrap();
            assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(
                expected_keys.is_subset(&keys.iter().cloned().collect()),
                "missing keys for {provider}/{model}"
            );
            let by_key = response.served_by_key.unwrap();
            assert_eq!(keys, by_key.keys().cloned().collect::<Vec<_>>());
            for (key, segments) in by_key {
                pairs += 1;
                assert_eq!(segments.first().unwrap().from_ms, begins);
                assert_eq!(segments.last().unwrap().until_ms, None);
                for adjacent in segments.windows(2) {
                    assert_eq!(adjacent[0].until_ms, Some(adjacent[1].from_ms));
                    assert!(adjacent[0].from_ms < adjacent[1].from_ms);
                    let mut left = adjacent[0].clone();
                    left.from_ms = adjacent[1].from_ms;
                    left.until_ms = adjacent[1].until_ms;
                    assert_ne!(left, adjacent[1], "equal neighbors must merge");
                }
                for segment in &segments {
                    segments_checked += 1;
                    if let Some(source) = segment.source {
                        sources.insert(format!("{source:?}"));
                    }
                    if let Some(reason) = segment.reason {
                        refusals.insert(format!("{reason:?}"));
                    }
                    if segment
                        .override_detail
                        .as_ref()
                        .is_some_and(|claim| !claim.replaces.is_null())
                    {
                        replacements += 1;
                    }
                    assert_parity(&store, &provider, &model, &key, segment, segment.from_ms);
                    endpoint_reads += 1;
                    if let Some(until) = segment.until_ms {
                        assert_parity(&store, &provider, &model, &key, segment, until - 1);
                        endpoint_reads += 1;
                    }
                }
            }
        }
    }
    assert_eq!(
        sources,
        BTreeSet::from([
            "Upstream".into(),
            "Override".into(),
            "Curated".into(),
            "CacheLifetime".into()
        ])
    );
    assert_eq!(
        refusals,
        BTreeSet::from(["Correction".into(), "NoCoverage".into()])
    );
    assert!(replacements > 0);
    eprintln!("served history parity: {models} fixture-models, {pairs} model/rate-key pairs, {segments_checked} segments, {endpoint_reads} dated endpoint reads, {replacements} replacement segments");
}

#[test]
fn served_history_correction_edges_curated_first_era_and_cache_rule_boundary() {
    let (store, _dir) = super::golden_payload::store();
    let corrected = history(&store, "anthropic", "claude-sonnet-4-5", "rate.input");
    assert_eq!(
        corrected
            .iter()
            .map(|s| (s.from_ms, s.until_ms, s.state))
            .collect::<Vec<_>>(),
        vec![
            (1_000, Some(3_000), HistoryState::Value),
            (3_000, Some(9_001), HistoryState::Refused),
            (9_001, None, HistoryState::Value),
        ]
    );
    overlays(&store);
    let curated = history(&store, "typesafe", "jev-1.13.0", "rate.input");
    let first = store.curated_model_eras("typesafe", "jev-1.13.0").unwrap()[0].boundary_at_ms;
    assert_eq!(curated[0].state, HistoryState::Refused);
    assert_eq!(curated[0].reason, Some(HistoryRefusalReason::NoCoverage));
    assert_eq!(curated[0].until_ms, Some(first));
    assert_eq!(curated[1].from_ms, first);
    assert_eq!(
        curated[1].source,
        Some(fusiform_protocol::HistorySource::Curated)
    );
    for segment in &curated {
        assert_parity(
            &store,
            "typesafe",
            "jev-1.13.0",
            "rate.input",
            segment,
            segment.from_ms,
        );
    }
    let (cache, _cache_dir) = super::cache_lifetimes::store(None);
    let ttl = history(
        &cache,
        "anthropic",
        "claude-opus-5-5",
        "rate.cache_write.ttl.1h",
    );
    let rule = cache
        .cache_lifetimes_at(i64::MAX, Some("anthropic"))
        .unwrap()
        .into_iter()
        .find(|row| row.model_id == "claude-opus-5-5")
        .unwrap();
    assert_eq!(ttl[0].state, HistoryState::Absent);
    assert_eq!(ttl[0].until_ms, Some(rule.boundary_at_ms));
    assert_eq!(ttl[1].from_ms, rule.boundary_at_ms);
    assert_eq!(
        ttl[1].source,
        Some(fusiform_protocol::HistorySource::CacheLifetime)
    );
}

#[test]
fn served_history_alias_mapping_is_current_not_dated() {
    let golden: Value =
        serde_json::from_str(include_str!("../../fixtures/served-payloads.json")).unwrap();
    let case = golden
        .as_object()
        .unwrap()
        .values()
        .find(|case| {
            case["request"]["name"] == "catalog.history"
                && case["response"]["inherited_from"]["basis"] == "alias"
        })
        .unwrap();
    let (store, _dir) = super::golden_payload::store();
    let mut arguments = case["request"]["arguments"].clone();
    arguments["served"] = true.into();
    let response = serve_history(&store, &serde_json::to_vec(&arguments).unwrap()).unwrap();
    assert_eq!(response.served, Some(Vec::new()));
    assert_eq!(
        response.inherited_from.unwrap().mapping.as_deref(),
        Some("current")
    );
}

#[test]
fn served_history_tracks_upstream_guards_existence_and_removed_ttl_keys() {
    use fusiform_core::{BoundaryKind, Timestamp};
    use fusiform_store::{FactKey, NewEra};
    let (store, _dir) = super::curated_wire::store();
    overlays(&store);
    let key = "limit.output";
    let replacement = history(&store, "google", "gemini-4-argon", key);
    assert_eq!(
        replacement.last().unwrap().source,
        Some(fusiform_protocol::HistorySource::Override)
    );
    assert!(!replacement
        .last()
        .unwrap()
        .override_detail
        .as_ref()
        .unwrap()
        .replaces
        .is_null());
    let changed_at = BOOT + 1_000;
    let era = |provider: &str, model: &str, key: &str, value: Value| NewEra {
        source: SourceId::ModelsDev,
        provider_id: provider.into(),
        model_id: model.into(),
        fact_key: FactKey::from_stored(key.into()),
        value_json: value.to_string(),
        boundary_at: Timestamp(changed_at),
        boundary_kind: BoundaryKind::Asserted,
        observation_id: None,
    };
    let mut matching = era(
        "typesafe",
        "jev-1.13.0",
        "rate.input",
        json!({
            "state":"priced", "currency":"USD", "exponent":9, "units":42_000_000,
            "unit_provenance":{"kind":"stated"}
        }),
    );
    matching.boundary_at = Timestamp(changed_at - 1);
    let mut withdrawn = era("typesafe", "jev-1.13.0", "existence", json!("absent"));
    withdrawn.boundary_at = Timestamp(changed_at - 1);
    store.append_eras(&[matching, withdrawn]).unwrap();
    let matching = history(&store, "typesafe", "jev-1.13.0", "rate.input");
    assert_eq!(
        matching.last().unwrap().source,
        Some(fusiform_protocol::HistorySource::Curated)
    );
    store
        .append_eras(&[
            era("google", "gemini-4-argon", key, json!(65_535)),
            era("typesafe", "jev-1.13.0", "existence", json!("present")),
            era(
                "typesafe",
                "jev-1.13.0",
                "rate.input",
                json!({"state":"priced", "currency":"USD", "exponent":9, "units":99}),
            ),
        ])
        .unwrap();
    let replacement = history(&store, "google", "gemini-4-argon", key);
    let last = replacement.last().unwrap();
    assert_eq!(last.from_ms, changed_at);
    assert_eq!(
        last.source,
        Some(fusiform_protocol::HistorySource::Upstream)
    );
    assert_eq!(last.value.as_ref().unwrap(), &json!(65_535));
    // Existence, rather than an era for the requested key, ends the curated value.
    let curated = history(&store, "typesafe", "jev-1.13.0", "rate.output");
    assert_eq!(curated.last().unwrap().from_ms, changed_at);
    assert_eq!(curated.last().unwrap().state, HistoryState::Absent);

    let (cache, _cache_dir) = super::cache_lifetimes::store(None);
    cache.append_cache_lifetimes(&[], BOOT).unwrap();
    let removed = history(
        &cache,
        "anthropic",
        "claude-opus-5-5",
        "rate.cache_write.ttl.1h",
    );
    assert_eq!(removed.last().unwrap().from_ms, BOOT);
    assert_eq!(removed.last().unwrap().state, HistoryState::Absent);
    let listing = serve_history(&cache, br#"{"provider_id":"anthropic","model_id":"claude-opus-5-5","fact_prefix":"rate.cache_write.ttl."}"#).unwrap();
    assert_eq!(
        listing.fact_keys.unwrap(),
        ["rate.cache_write.ttl.1h", "rate.cache_write.ttl.5m"]
    );
    assert!(listing.served_by_key.is_none());
    for segment in &removed {
        assert_parity(
            &cache,
            "anthropic",
            "claude-opus-5-5",
            "rate.cache_write.ttl.1h",
            segment,
            segment.from_ms,
        );
    }
}
