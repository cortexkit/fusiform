use super::*;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::BoundaryKind;
use fusiform_store::NewEra;
use serde_json::{json, Value};

fn empty_store() -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("lineage.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    (store, dir)
}

fn fixture(canonical_alias: bool) -> (CatalogStore, tempfile::TempDir) {
    let (store, dir) = empty_store();
    for (provider, model, family, canonical) in [
        (
            "reseller",
            "shared",
            Some("deepseek-thinking"),
            Some("alibaba/not-the-origin"),
        ),
        ("reseller", "lab-only", None, Some("moonshotai/artifact")),
        (
            "reseller",
            "unknown",
            Some("unmapped"),
            Some("openai/artifact"),
        ),
        ("reseller", "gpt-name-is-not-evidence", None, None),
        ("openai", "first-party", None, None),
        (
            "openai",
            "unknown-canonical",
            None,
            Some("unmapped/artifact"),
        ),
        (
            "google",
            "gemini-4-argon",
            Some("glm"),
            canonical_alias.then_some("openai/artifact"),
        ),
    ] {
        let mut facts = vec![("existence", json!("present"))];
        if let Some(family) = family {
            facts.push(("model.family", json!(family)));
        }
        if let Some(canonical) = canonical {
            facts.push(("model.canonical_id", json!(canonical)));
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
fn lineage_reseller_uses_family_without_storage() {
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
        assert_eq!(
            response.models["reseller/shared"]["model.lineage"],
            "deepseek"
        );
    }
    assert_eq!(store.era_count(SourceId::ModelsDev).unwrap(), eras);
    assert_eq!(store.catalog_version().unwrap(), version);
    let (stored, _) = store
        .read_model(SourceId::ModelsDev, "reseller", "shared", None)
        .unwrap();
    assert!(!stored
        .unwrap()
        .facts
        .keys()
        .any(|key| key.as_str() == "model.lineage"));
}

#[test]
fn lineage_canonical_lab_first_party_fallback_and_unmapped_rows() {
    let (store, _dir) = fixture(true);
    let response = get(&store, json!({}));
    assert_eq!(
        response.models["reseller/lab-only"]["model.lineage"],
        "kimi"
    );
    assert_eq!(
        response.models["openai/first-party"]["model.lineage"],
        "gpt"
    );
    for identity in [
        "reseller/unknown",
        "reseller/gpt-name-is-not-evidence",
        "openai/unknown-canonical",
    ] {
        assert!(
            !response.models[identity].contains_key("model.lineage"),
            "{identity}"
        );
    }
}

#[test]
fn lineage_obeys_prefix_filters_and_is_not_served_at_ms() {
    let (store, _dir) = fixture(true);
    for scope in [
        json!({}),
        json!({"provider_id":"reseller"}),
        json!({"provider_id":"reseller","model_id":"shared"}),
    ] {
        for prefixes in [json!(["model.lineage"]), json!(["model."])] {
            let mut args = scope.clone();
            args["fact_prefixes"] = prefixes.clone();
            let response = get(&store, args);
            assert_eq!(
                response.models["reseller/shared"]["model.lineage"],
                "deepseek"
            );
            assert!(response
                .models
                .values()
                .all(|facts| facts.keys().all(|key| {
                    prefixes
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|p| key.starts_with(p.as_str().unwrap()))
                })));
        }
        for extra in [
            json!({"at_ms":1000}),
            json!({"fact_prefixes":["rate."]}),
            json!({"fact_prefixes":[]}),
        ] {
            let mut args = scope.clone();
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let response = get(&store, args);
            assert!(response
                .models
                .values()
                .all(|facts| !facts.contains_key("model.lineage")));
        }
    }
}

#[test]
fn lineage_alias_uses_exact_entry_or_its_canonical_identity_and_planes_match_plain() {
    let (store, _dir) = fixture(true);
    for auth in [Value::Null, json!("apikey")] {
        let response = get(
            &store,
            json!({"provider_id":"google", "model_id":"antigravity-gemini-4-argon", "auth_method":auth, "fact_prefixes":["model.lineage"]}),
        );
        assert_eq!(
            response.models["google/antigravity-gemini-4-argon"]["model.lineage"],
            "gemini"
        );
    }
    let mut doc: Value =
        serde_json::from_str(include_str!("../../data/model-lineage.json")).unwrap();
    // A target's exact override must not become an alias's own classification.
    doc["models"]["entries"] = json!({"google/gemini-4-argon":"grok"});
    let _classifications = crate::model_lineage::plant(&doc.to_string());
    for canonical in [true, false] {
        let (store, _dir) = fixture(canonical);
        for auth in [Value::Null, json!("apikey")] {
            let response = get(
                &store,
                json!({"provider_id":"google", "model_id":"antigravity-gemini-4-argon", "auth_method":auth, "fact_prefixes":["model.lineage"]}),
            );
            let facts = &response.models["google/antigravity-gemini-4-argon"];
            if canonical {
                assert_eq!(facts["model.lineage"], "gpt");
            } else {
                assert!(!facts.contains_key("model.lineage"));
            }
        }
    }
}

// Snapshot of the routing service's production ID globs, supplied on 2026-10-29
// and retained as an independent baseline for the parity test below.
// This dated reference preserves first-match order, including known mistakes;
// production lineage classification never uses these name patterns.
const ROUTING_GLOBS_2026_10_29: &[(&str, &[&str])] = &[
    (
        "claude",
        &["*/claude-*", "anthropic/*", "google/antigravity-claude-*"],
    ),
    ("gpt", &["openai/*", "*/gpt-*", "*/o1*", "*/o3*", "*/o4*"]),
    ("gemini", &["*/gemini-*", "google/*"]),
    ("grok", &["*/grok-*", "xai/*"]),
    ("qwen", &["*/qwen*", "alibaba-*/*"]),
    ("kimi", &["*/kimi*", "moonshotai/*", "kimi-for-coding/*"]),
    ("glm", &["*/glm-*", "zai/*"]),
    ("deepseek", &["*/deepseek-*", "deepseek/*"]),
    ("minimax", &["*/minimax-*", "minimax/*"]),
    ("mistral", &["*/mistral-*", "mistral/*"]),
    ("llama", &["*/llama-*", "meta/*"]),
    ("other", &["*"]),
];

fn matches_reference_glob(value: &str, pattern: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(mut rest) = value.strip_prefix(parts.next().unwrap()) else {
        return false;
    };
    let parts: Vec<_> = parts.collect();
    if parts.is_empty() {
        return rest.is_empty();
    }
    for (index, part) in parts.iter().enumerate() {
        if index == parts.len() - 1 {
            return rest.ends_with(part);
        }
        let Some((_, after)) = rest.split_once(part) else {
            return false;
        };
        rest = after;
    }
    unreachable!()
}

#[test]
fn lineage_seed_parity_with_dated_routing_globs_has_only_explicit_differences() {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct DifferenceGroup {
        reason: String,
        rows: BTreeMap<String, [String; 2]>,
    }
    let groups: Vec<DifferenceGroup> =
        serde_json::from_str(include_str!("model_lineage_routing_differences.json")).unwrap();
    let mut allowed = BTreeMap::new();
    for group in groups {
        assert!(!group.reason.trim().is_empty());
        for (key, transition) in group.rows {
            assert!(
                allowed.insert(key, transition).is_none(),
                "duplicate allowlist row"
            );
        }
    }
    // These checks pin the glob interpreter's slash, case and prefix behavior,
    // independently of the curated map and the expected difference fixture.
    assert!(matches_reference_glob(
        "reseller/nested/o1-unrelated",
        "*/o1*"
    ));
    assert!(matches_reference_glob(
        "alibaba-token-plan/deepseek-v3.2",
        "alibaba-*/*"
    ));
    assert!(!matches_reference_glob(
        "reseller/Claude-sonnet",
        "*/claude-*"
    ));
    assert!(!matches_reference_glob("reseller/not-gpt", "*/gpt-*"));

    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    let (store, _dir) = empty_store();
    crate::seed::seed_if_empty(&store).unwrap();
    // The parity audit needs every seed row, not a frame-limited first page.
    let response = serve_catalog_get_with_limits(&store, b"{}", usize::MAX, usize::MAX).unwrap();
    let mut coverage: BTreeMap<String, usize> = BTreeMap::new();
    let mut newly_classified = 0;
    let mut disagreements = 0;
    for (provider, data) in seed.as_object().unwrap() {
        for model in data["models"].as_object().unwrap().keys() {
            let key = format!("{provider}/{model}");
            let expected = ROUTING_GLOBS_2026_10_29
                .iter()
                .find(|(_, globs)| globs.iter().any(|glob| matches_reference_glob(&key, glob)))
                .unwrap()
                .0;
            let facts = response
                .models
                .get(&key)
                .unwrap_or_else(|| panic!("missing seed row: {key}"));
            let actual = facts
                .get("model.lineage")
                .map(|value| value.as_str().unwrap());
            assert_ne!(actual, Some("other"), "{key}: other must be absence");
            let actual = actual.unwrap_or("other");
            *coverage.entry(actual.into()).or_default() += 1;
            if expected != actual {
                assert_eq!(
                    allowed.remove(&key),
                    Some([expected.into(), actual.into()]),
                    "unreviewed lineage difference: {key}"
                );
                if expected == "other" {
                    newly_classified += 1;
                } else {
                    disagreements += 1;
                }
            }
        }
    }
    assert!(allowed.is_empty(), "stale routing differences: {allowed:?}");
    assert_eq!(newly_classified, 1458);
    assert_eq!(disagreements, 129);
    assert_eq!(
        coverage,
        BTreeMap::from([
            ("claude".into(), 850),
            ("deepseek".into(), 643),
            ("gemini".into(), 801),
            ("glm".into(), 755),
            ("gpt".into(), 1558),
            ("grok".into(), 275),
            ("kimi".into(), 406),
            ("llama".into(), 313),
            ("minimax".into(), 299),
            ("mistral".into(), 324),
            ("qwen".into(), 1087),
            ("other".into(), 1625),
        ])
    );
    println!("seed lineage coverage: {coverage:?}; newly classified={newly_classified}; disagreements={disagreements}");
}
