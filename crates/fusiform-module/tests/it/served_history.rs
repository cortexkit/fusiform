use std::collections::{BTreeMap, BTreeSet};

use fusiform_core::SourceId;
use fusiform_module::route::{serve_catalog_get, serve_history, serve_tool_call};
use fusiform_protocol::{HistoryRefusalReason, HistorySegment, HistoryState};
use fusiform_store::CatalogStore;
use serde_json::{json, Value};

const BOOT: i64 = 1_800_000_000_000;

fn overlays(store: &CatalogStore) {
    fusiform_module::fact_overrides::apply_to_store(
        store,
        fusiform_module::fact_overrides::overrides(),
        BOOT,
    )
    .unwrap();
    let (mut table, rejects) = fusiform_module::curated_models::load_table_reporting_rejects(
        include_str!("../../data/curated-models.json"),
    );
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

fn assert_parity(
    store: &CatalogStore,
    provider: &str,
    model: &str,
    key: &str,
    segment: &HistorySegment,
    at: i64,
) {
    let read = serve_catalog_get(
        store,
        &serde_json::to_vec(&json!({
            "provider_id":provider, "model_id":model, "at_ms":at
        }))
        .unwrap(),
    );
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

pub(super) fn golden_cases() -> serde_json::Map<String, Value> {
    let (ordinary, _ordinary_dir) = super::golden_payload::store();
    let (cache, _cache_dir) = super::cache_lifetimes::store(None);
    overlays(&cache);
    let mut cases = serde_json::Map::new();
    for (label, store, arguments) in [
        (
            "catalog.history — served upstream rate",
            &ordinary,
            json!({"provider_id":"anthropic", "model_id":"claude-sonnet-4-5", "fact_key":"rate.input", "served":true}),
        ),
        (
            "catalog.history — served override addition",
            &cache,
            json!({"provider_id":"openai", "model_id":"gpt-realtime-2.1", "fact_key":"rate.cached_input_audio", "served":true}),
        ),
        (
            "catalog.history — served cache lifetime",
            &cache,
            json!({"provider_id":"anthropic", "model_id":"claude-opus-5-5", "fact_key":"rate.cache_write.ttl.1h", "served":true}),
        ),
        (
            "catalog.history — historical rate keys",
            &cache,
            json!({"provider_id":"openai", "model_id":"gpt-realtime-2.1", "fact_prefix":"rate.cached_", "served":true}),
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
    let key = "rate.output";
    let replacement = history(&store, "google", "gemini-3.1-flash-image", key);
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
            era(
                "google",
                "gemini-3.1-flash-image",
                key,
                json!({"state":"priced", "currency":"USD", "exponent":9, "units":4_000_000_000i64}),
            ),
            era("typesafe", "jev-1.13.0", "existence", json!("present")),
            era(
                "typesafe",
                "jev-1.13.0",
                "rate.input",
                json!({"state":"priced", "currency":"USD", "exponent":9, "units":99}),
            ),
        ])
        .unwrap();
    let replacement = history(&store, "google", "gemini-3.1-flash-image", key);
    let last = replacement.last().unwrap();
    assert_eq!(last.from_ms, changed_at);
    assert_eq!(
        last.source,
        Some(fusiform_protocol::HistorySource::Upstream)
    );
    assert_eq!(last.value.as_ref().unwrap()["units"], 4_000_000_000i64);
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
