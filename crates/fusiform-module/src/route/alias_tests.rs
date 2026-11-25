//! Curated aliases, driven through `serve_tool_call` with the SHIPPED table.
//!
//! Through the route rather than the helpers, so a route that stops calling
//! the alias logic reddens a test: a helper test passes against a correct
//! helper nobody calls, which is how the `--rates` inheritance defect above
//! shipped.

use super::*;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::BoundaryKind;
use fusiform_store::NewEra;

const ALIAS: &str = "google/antigravity-gemini-3.8-flash";
const TARGET: &str = "google/gemini-3.8-flash";

fn store() -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    })
    .unwrap();
    (store, dir)
}

fn era_at(model: &str, key: &str, value: &str, at: i64) -> NewEra {
    NewEra {
        source: SourceId::ModelsDev,
        provider_id: "google".to_string(),
        model_id: model.to_string(),
        fact_key: FactKey::from_stored(key.to_string()),
        value_json: value.to_string(),
        boundary_at: Timestamp(at),
        boundary_kind: BoundaryKind::Seed,
        observation_id: None,
    }
}

/// A google row with a family, a limit, reasoning options and a price.
fn seed_model(store: &CatalogStore, model: &str, units: i64) {
    store
        .append_eras(&[
            era_at(model, "existence", r#""present""#, 1_000),
            era_at(model, "model.family", r#""gemini-flash""#, 1_000),
            era_at(model, "limit.context", "1048576", 1_000),
            era_at(
                model,
                "capability.reasoning_options",
                r#"["low","medium","high"]"#,
                1_000,
            ),
            era_at(
                model,
                "rate.input",
                &format!(r#"{{"state":"priced","units":{units},"exponent":9,"currency":"USD"}}"#),
                1_000,
            ),
        ])
        .unwrap();
}

fn get(store: &CatalogStore, args: &str) -> Result<CatalogGetResponse, RouteError> {
    let body = format!(r#"{{"name":"catalog.get","arguments":{args}}}"#);
    match serve_tool_call(store, body.as_bytes())? {
        ToolResponse::Catalog(c) => Ok(c),
        other => panic!("catalog.get must answer with a catalog: {other:?}"),
    }
}

fn named(prefixes: &str) -> String {
    format!(r#"{{"provider_id":"google","model_id":"antigravity-gemini-3.8-flash"{prefixes}}}"#)
}

#[test]
fn hydrogen_antigravity_alias_serves_its_route_and_borrowed_facts() {
    let (store, _d) = store();
    seed_model(&store, "gemini-4-hydrogen", 300_000_000);
    store
        .append_eras(&[
            era_at("gemini-4-hydrogen", "limit.output", "65536", 1_000),
            era_at(
                "gemini-4-hydrogen",
                "capability.reasoning_options",
                r#"[{"type":"effort","values":["minimal","low","medium","high","xhigh"]}]"#,
                2_000,
            ),
        ])
        .unwrap();
    let args = r#"{"provider_id":"google","model_id":"antigravity-gemini-4-hydrogen"}"#;
    let got = get(&store, args).unwrap();
    let facts = &got.models["google/antigravity-gemini-4-hydrogen"];
    assert_eq!(facts["limit.context"], 1_048_576);
    // The route enforces the target's limit; it has no output override.
    assert_eq!(facts["limit.output"], 65_536);
    assert_eq!(
        facts["capability.reasoning_options"],
        serde_json::json!([{"type":"effort","values":["low", "medium", "high", "xhigh"]}])
    );
    assert_eq!(
        facts["route.reasoning_wire_ids"],
        serde_json::json!({
            "low":"gemini-4-hydrogen-low", "medium":"gemini-4-hydrogen-medium",
            "high":"gemini-4-hydrogen-high", "xhigh":"gemini-4-hydrogen-xhigh"
        })
    );
    assert_eq!(facts["route.reasoning_default"], "high");
    assert_eq!(facts["rate.input"]["units"], 300_000_000);
    assert_eq!(
        facts["rate.input"]["inherited_from"]["model_id"],
        "gemini-4-hydrogen"
    );
    assert_eq!(facts["rate.input"]["inherited_from"]["basis"], "alias");
    assert_eq!(got.aliased[0].target_model_id, "gemini-4-hydrogen");
    let direct = get(
        &store,
        r#"{"provider_id":"google","model_id":"gemini-4-hydrogen"}"#,
    )
    .unwrap();
    assert_eq!(
        direct.models["google/gemini-4-hydrogen"]["limit.output"],
        65_536
    );
}

#[test]
fn hydrogen_antigravity_upstream_row_wins_over_alias() {
    let (store, _d) = store();
    seed_model(&store, "gemini-4-hydrogen", 300_000_000);
    seed_model(&store, "antigravity-gemini-4-hydrogen", 7);
    let got = get(
        &store,
        r#"{"provider_id":"google","model_id":"antigravity-gemini-4-hydrogen"}"#,
    )
    .unwrap();
    let rate = &got.models["google/antigravity-gemini-4-hydrogen"]["rate.input"];
    assert!(got.aliased.is_empty());
    assert_eq!(rate["units"], 7);
    assert!(rate.get("inherited_from").is_none());
}

#[test]
fn a_named_alias_read_serves_the_target_under_the_alias_key() {
    let (store, _d) = store();
    seed_model(&store, "gemini-3.8-flash", 300_000_000);

    let got = get(&store, &named("")).expect("an alias id must be served");
    let facts = got
        .models
        .get(ALIAS)
        .unwrap_or_else(|| panic!("the entry must sit under the alias key: {got:?}"));
    assert!(
        !got.models.contains_key(TARGET),
        "the target must not be served under its own key on a read that \
         named the alias: {got:?}"
    );
    assert_eq!(facts["limit.context"], 1_048_576);
    assert_eq!(
        facts["capability.reasoning_options"],
        serde_json::json!(["low", "medium", "high"])
    );
    assert_eq!(facts["model.family"], "gemini-flash");

    let rate = &facts["rate.input"];
    assert_eq!(rate["units"], 300_000_000);
    assert_eq!(
        rate["inherited_from"],
        serde_json::json!({
            "provider_id": "google",
            "model_id": "gemini-3.8-flash",
            "family": "gemini-flash",
            "basis": "alias",
        }),
        "a price through an alias is the target's list price and must say so"
    );

    assert_eq!(
        got.aliased,
        vec![fusiform_protocol::AliasedModelWire {
            model: ALIAS.to_string(),
            target_provider_id: "google".to_string(),
            target_model_id: "gemini-3.8-flash".to_string(),
            source_ref: crate::aliases::aliases()[&(
                "google".to_string(),
                "antigravity-gemini-3.8-flash".to_string()
            )]
                .source_ref
                .clone(),
        }]
    );

    // CONTROL: the target read directly carries no alias marker, so the
    // marker above is the alias path's doing rather than the store's.
    let direct = get(
        &store,
        r#"{"provider_id":"google","model_id":"gemini-3.8-flash"}"#,
    )
    .unwrap();
    assert!(direct.aliased.is_empty());
    assert!(direct.models[TARGET]["rate.input"]
        .get("inherited_from")
        .is_none());
}

#[test]
fn an_alias_without_a_sourced_name_never_inherits_the_target_name() {
    let (store, _d) = store();
    seed_model(&store, "gemini-3.8-flash", 300_000_000);
    store
        .append_eras(&[era_at(
            "gemini-3.8-flash",
            "model.name",
            r#""Target label""#,
            1_000,
        )])
        .unwrap();
    let mut alias = crate::aliases::aliases()[&(
        "google".to_string(),
        "antigravity-gemini-3.8-flash".to_string(),
    )]
        .clone();
    alias.model_name = None;
    let got = aliased_response(
        &store,
        SourceId::ModelsDev,
        "google",
        "antigravity-gemini-3.8-flash",
        &alias,
        None,
        &crate::curation::embedded(),
    )
    .unwrap()
    .unwrap();
    assert!(!got.models[ALIAS].contains_key("model.name"));
    let target = get(
        &store,
        r#"{"provider_id":"google","model_id":"gemini-3.8-flash"}"#,
    )
    .unwrap();
    assert_eq!(target.models[TARGET]["model.name"], "Target label");
}

const RETIRED: &str = "google/antigravity-gemini-3.7-flash";

/// A retired alias stays an identity on retired-inclusive reads. A consumer
/// that tracks identities refuses a refresh where a known one vanished, so
/// deleting the row instead stalled that consumer on a stale catalog.
#[test]
fn a_retired_alias_is_reported_absent_not_dropped() {
    let (store, _d) = store();
    seed_model(&store, "gemini-3.8-flash", 1);

    for args in [
        r#"{"include_retired":true}"#,
        r#"{"provider_id":"google","include_retired":true}"#,
        r#"{"paged":true,"include_retired":true}"#,
    ] {
        let bulk = get(&store, args).unwrap();
        assert_eq!(
            bulk.models.get(RETIRED).map(|f| f["existence"].clone()),
            Some(serde_json::json!("absent")),
            "{args}: {:?}",
            bulk.models.keys().collect::<Vec<_>>()
        );
    }

    // Without include_retired a bulk read omits it, like any withdrawn row.
    let plain = get(&store, r#"{"provider_id":"google"}"#).unwrap();
    assert!(!plain.models.contains_key(RETIRED));

    // A named read reports it retired rather than unknown.
    let named = get(
        &store,
        r#"{"provider_id":"google","model_id":"antigravity-gemini-3.7-flash"}"#,
    )
    .expect("a retired alias is known, not no_coverage");
    assert!(named.models.is_empty());
    assert_eq!(
        named
            .retired
            .iter()
            .map(|r| r.model.as_str())
            .collect::<Vec<_>>(),
        vec![RETIRED]
    );

    // A point-in-time read never lists it: an alias has no past.
    let at = get(&store, r#"{"include_retired":true,"at_ms":1500}"#).unwrap();
    assert!(!at.models.contains_key(RETIRED));
}

/// A real upstream row for a retired alias id wins over the retirement.
#[test]
fn a_real_row_wins_over_a_retired_alias() {
    let (store, _d) = store();
    seed_model(&store, "gemini-3.8-flash", 1);
    seed_model(&store, "antigravity-gemini-3.7-flash", 1);
    let bulk = get(&store, r#"{"provider_id":"google","include_retired":true}"#).unwrap();
    assert_eq!(bulk.models[RETIRED]["existence"], "present");
}

#[test]
fn wire_facts_are_not_projected_onto_an_alias() {
    let (store, _d) = store();
    seed_model(&store, "gemini-3.8-flash", 300_000_000);
    for key in ["wire.npm", "wire.api", "wire.shape"] {
        store
            .append_eras(&[era_at(
                "gemini-3.8-flash",
                key,
                r#"{"value":"target-only","from":"model"}"#,
                2_000,
            )])
            .unwrap();
    }
    // Both named and bulk reads must keep the alias's backend unknown.
    for args in [named(""), r#"{"provider_id":"google"}"#.to_owned()] {
        let response = get(&store, &args).unwrap();
        assert!(response.models.contains_key(ALIAS));
        for key in ["wire.npm", "wire.api", "wire.shape"] {
            assert!(!response.models[ALIAS].contains_key(key), "alias has {key}");
        }
    }
    let target = get(
        &store,
        r#"{"provider_id":"google","model_id":"gemini-3.8-flash"}"#,
    )
    .unwrap();
    for key in ["wire.npm", "wire.api", "wire.shape"] {
        assert_eq!(target.models[TARGET][key]["value"], "target-only");
    }
}

#[test]
fn structured_output_is_not_projected_onto_an_alias() {
    let (store, _d) = store();
    seed_model(&store, "gemini-3.8-flash", 300_000_000);
    store
        .append_eras(&[era_at(
            "gemini-3.8-flash",
            "capability.structured_output",
            "true",
            2_000,
        )])
        .unwrap();

    let aliased = get(&store, &named("")).expect("the alias is served");
    let alias_facts = &aliased.models[ALIAS];
    assert!(
        !alias_facts.contains_key("capability.structured_output"),
        "an alias cannot claim route-specific schema enforcement"
    );

    let target = get(
        &store,
        r#"{"provider_id":"google","model_id":"gemini-3.8-flash"}"#,
    )
    .expect("the target is served directly");
    assert_eq!(
        target.models[TARGET]["capability.structured_output"],
        serde_json::json!(true),
        "the target row retains the upstream's published claim"
    );
}

#[test]
fn model_type_is_projected_onto_an_alias_verbatim() {
    let (store, _d) = store();
    seed_model(&store, "gemini-3.8-flash", 300_000_000);
    store
        .append_eras(&[era_at(
            "gemini-3.8-flash",
            "model.type",
            r#""future-kind""#,
            2_000,
        )])
        .unwrap();
    for args in [named(""), r#"{"provider_id":"google"}"#.to_owned()] {
        let response = get(&store, &args).expect("the alias is served");
        assert_eq!(response.models[ALIAS]["model.type"], "future-kind");
    }
}

#[test]
fn a_real_row_for_the_alias_id_wins() {
    let (store, _d) = store();
    seed_model(&store, "gemini-3.8-flash", 300_000_000);
    seed_model(&store, "antigravity-gemini-3.8-flash", 7);

    let got = get(&store, &named("")).unwrap();
    assert!(
        got.aliased.is_empty(),
        "a real row must not be disclosed as aliased: {got:?}"
    );
    let rate = &got.models[ALIAS]["rate.input"];
    assert_eq!(rate["units"], 7, "the real row's own price must be served");
    assert!(rate.get("inherited_from").is_none());

    // The bulk read must not add a second entry for it either.
    let bulk = get(&store, r#"{"provider_id":"google"}"#).unwrap();
    assert_eq!(bulk.models[ALIAS]["rate.input"]["units"], 7);
    assert!(
        !bulk.aliased.iter().any(|a| a.model == ALIAS),
        "{:?}",
        bulk.aliased
    );
}

#[test]
fn an_absent_target_serves_nothing_and_names_it() {
    let (store, _d) = store();
    // The provider exists, so this cannot pass by refusing the provider.
    seed_model(&store, "gemini-3.7-flash", 1);

    let err = get(&store, &named("")).expect_err("an alias of nothing must refuse");
    assert_eq!(err.code, fusiform_protocol::CODE_NO_COVERAGE);
    assert!(err.message.contains(TARGET), "{}", err.message);

    let bulk = get(&store, r#"{"provider_id":"google"}"#).unwrap();
    assert!(!bulk.models.contains_key(ALIAS), "{bulk:?}");
    assert!(!bulk.aliased.iter().any(|a| a.model == ALIAS));
}

#[test]
fn a_retired_target_serves_nothing() {
    let (store, _d) = store();
    seed_model(&store, "gemini-3.8-flash", 1);
    seed_model(&store, "gemini-3.7-flash", 1);
    store
        .append_eras(&[era_at(
            "gemini-3.8-flash",
            "existence",
            r#""absent""#,
            2_000,
        )])
        .unwrap();

    let err = get(&store, &named("")).expect_err("an alias of a retired row must refuse");
    assert_eq!(err.code, fusiform_protocol::CODE_NO_COVERAGE);
    assert!(err.message.contains(TARGET), "{}", err.message);

    let bulk = get(&store, r#"{"provider_id":"google"}"#).unwrap();
    assert!(!bulk.models.contains_key(ALIAS), "{bulk:?}");
}

#[test]
fn a_point_in_time_read_never_aliases() {
    let (store, _d) = store();
    seed_model(&store, "gemini-3.8-flash", 1);

    let err =
        get(&store, &named(r#","at_ms":1500"#)).expect_err("an at read of an alias id must refuse");
    assert_eq!(err.code, fusiform_protocol::CODE_NO_COVERAGE);
    assert!(
        err.message.contains("current reads only"),
        "{}",
        err.message
    );

    let bulk = get(&store, r#"{"provider_id":"google","at_ms":1500}"#).unwrap();
    assert!(bulk.aliased.is_empty(), "{bulk:?}");
    assert!(!bulk.models.contains_key(ALIAS));
}

#[test]
fn a_bulk_read_includes_the_alias() {
    let (store, _d) = store();
    seed_model(&store, "gemini-3.8-flash", 300_000_000);

    for args in [r#"{}"#, r#"{"provider_id":"google"}"#] {
        let bulk = get(&store, args).unwrap();
        let facts = bulk
            .models
            .get(ALIAS)
            .unwrap_or_else(|| panic!("{args}: the alias must be in a bulk read: {bulk:?}"));
        assert_eq!(facts["rate.input"]["inherited_from"]["basis"], "alias");
        assert!(
            bulk.models.contains_key(TARGET),
            "the target keeps its own entry"
        );
        assert_eq!(
            bulk.aliased
                .iter()
                .map(|a| a.model.as_str())
                .collect::<Vec<_>>(),
            vec![ALIAS],
            "{args}: only aliases whose target is present are disclosed"
        );
    }

    // A provider filter that is not the alias's provider excludes it.
    seed_model(&store, "gemini-3.7-flash", 1);
    store
        .append_eras(&[NewEra {
            provider_id: "anthropic".to_string(),
            ..era_at("claude-x", "existence", r#""present""#, 1_000)
        }])
        .unwrap();
    let other = get(&store, r#"{"provider_id":"anthropic"}"#).unwrap();
    assert!(other.aliased.is_empty(), "{other:?}");
}

#[test]
fn a_rate_filter_still_gets_rates_through_the_alias() {
    let (store, _d) = store();
    seed_model(&store, "gemini-3.8-flash", 300_000_000);

    let got = get(&store, &named(r#","fact_prefixes":["rate."]"#)).unwrap();
    let facts = &got.models[ALIAS];
    assert_eq!(facts["rate.input"]["units"], 300_000_000);
    assert_eq!(
        facts["rate.input"]["inherited_from"]["family"], "gemini-flash",
        "the family is read from model.* facts, which the filter must not \
         take away from the alias path"
    );
    assert!(
        facts.keys().all(|k| k.starts_with("rate.")),
        "the widening must not leak into a --rates answer: {facts:?}"
    );

    let bulk = get(
        &store,
        r#"{"provider_id":"google","fact_prefixes":["rate."]}"#,
    )
    .unwrap();
    assert_eq!(
        bulk.models[ALIAS]["rate.input"]["inherited_from"]["family"],
        "gemini-flash"
    );
}

#[test]
fn history_of_an_alias_names_the_target() {
    let (store, _d) = store();
    seed_model(&store, "gemini-3.8-flash", 1);

    for fact in ["rate.input", "limit.context"] {
        let body = format!(
            r#"{{"name":"catalog.history","arguments":{{"provider_id":"google","model_id":"antigravity-gemini-3.8-flash","fact_key":"{fact}"}}}}"#
        );
        let ToolResponse::History(h) = serve_tool_call(&store, body.as_bytes())
            .unwrap_or_else(|e| panic!("{fact}: history of an alias id must answer: {e:?}"))
        else {
            panic!("history must answer with a history");
        };
        assert!(h.eras.is_empty(), "fusiform never observed the alias id");
        let origin = h.inherited_from.expect("the target must be named");
        assert_eq!(origin.provider_id, "google");
        assert_eq!(origin.model_id.as_deref(), Some("gemini-3.8-flash"));
        assert_eq!(origin.basis, "alias");
        assert_eq!(origin.family, "gemini-flash");
    }
}
