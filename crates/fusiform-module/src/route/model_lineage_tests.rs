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

type DifferenceRows = Vec<(String, [String; 2])>;

fn deserialize_difference_rows<'de, D>(deserializer: D) -> Result<DifferenceRows, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Rows;
    impl<'de> serde::de::Visitor<'de> for Rows {
        type Value = DifferenceRows;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an object mapping seed identities to lineage transitions")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut rows = Vec::new();
            while let Some(row) = map.next_entry()? {
                rows.push(row);
            }
            Ok(rows)
        }
    }
    // Map order is part of the audit's readable diff: existing reviewed rows
    // stay in place, and new classifications are appended in sorted order.
    deserializer.deserialize_map(Rows)
}

fn serialize_difference_rows<S>(rows: &DifferenceRows, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;

    let mut map = serializer.serialize_map(Some(rows.len()))?;
    for (key, transition) in rows {
        map.serialize_entry(key, transition)?;
    }
    map.end()
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct DifferenceGroup {
    reason: String,
    #[serde(
        deserialize_with = "deserialize_difference_rows",
        serialize_with = "serialize_difference_rows"
    )]
    rows: DifferenceRows,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct LineageCounts {
    coverage: BTreeMap<String, usize>,
    newly_classified: usize,
    disagreements: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct LineageAudit {
    groups: Vec<DifferenceGroup>,
    counts: LineageCounts,
}

const NEWLY_CLASSIFIED_REASON: &str =
    "The globs missed reseller, nested, case-varied or related-family ids";

fn compare_lineage_seed(
    seed: &Value,
    audit: &LineageAudit,
    response: &CatalogGetResponse,
    update: bool,
) -> Result<LineageAudit, String> {
    let mut allowed = BTreeMap::new();
    for group in &audit.groups {
        if group.reason.trim().is_empty() {
            return Err("routing difference group needs a reason".into());
        }
        for (key, transition) in &group.rows {
            if allowed.insert(key.clone(), transition.clone()).is_some() {
                return Err(format!("duplicate allowlist row: {key}"));
            }
        }
    }
    let mut counts = LineageCounts::default();
    let mut errors = Vec::new();
    let mut seed_keys = BTreeSet::new();
    let mut additions = BTreeMap::new();
    let mut updated = audit.clone();
    for (provider, data) in seed.as_object().unwrap() {
        for model in data["models"].as_object().unwrap().keys() {
            let key = format!("{provider}/{model}");
            seed_keys.insert(key.clone());
            let expected = ROUTING_GLOBS_2026_10_29
                .iter()
                .find(|(_, globs)| globs.iter().any(|glob| matches_reference_glob(&key, glob)))
                .unwrap()
                .0;
            let Some(facts) = response.models.get(&key) else {
                errors.push(format!("missing seed row: {key}"));
                continue;
            };
            let actual = facts
                .get("model.lineage")
                .map(|value| value.as_str().unwrap());
            if actual == Some("other") {
                errors.push(format!("{key}: other must be absence"));
            }
            let actual = actual.unwrap_or("other");
            *counts.coverage.entry(actual.into()).or_default() += 1;
            if expected != actual {
                let transition = [expected.into(), actual.into()];
                match allowed.remove(&key) {
                    Some(reviewed) if reviewed == transition => {}
                    None if update && expected == "other" => {
                        additions.insert(key, transition);
                    }
                    _ => errors.push(format!(
                        "unreviewed lineage difference: {key}: {transition:?}; add a hand-written reason group"
                    )),
                }
                if expected == "other" {
                    counts.newly_classified += 1;
                } else {
                    counts.disagreements += 1;
                }
            }
        }
    }
    for (key, transition) in allowed {
        // A seed refresh can remove a row, but a changed transition on a row
        // still present in the seed must not silently lose its reviewed reason.
        if update && !seed_keys.contains(&key) {
            for group in &mut updated.groups {
                group.rows.retain(|(row, _)| row != &key);
            }
        } else {
            errors.push(format!("stale routing difference: {key}: {transition:?}"));
        }
    }
    if update {
        updated.counts = counts;
        if !additions.is_empty() {
            if let Some(group) = updated
                .groups
                .iter_mut()
                .find(|group| group.reason.starts_with(NEWLY_CLASSIFIED_REASON))
            {
                group.rows.extend(additions);
            } else {
                errors.push(format!(
                    "missing newly-classified reason group for rows: {additions:?}"
                ));
            }
        }
    } else {
        for lineage in counts
            .coverage
            .keys()
            .chain(audit.counts.coverage.keys())
            .collect::<BTreeSet<_>>()
        {
            let expected = audit.counts.coverage.get(lineage);
            let actual = counts.coverage.get(lineage);
            if expected != actual {
                errors.push(format!("lineage coverage mismatch: {lineage}: expected {expected:?}, actual {actual:?}"));
            }
        }
        if counts.newly_classified != audit.counts.newly_classified {
            errors.push(format!(
                "newly_classified count mismatch: expected {}, actual {}",
                audit.counts.newly_classified, counts.newly_classified
            ));
        }
        if counts.disagreements != audit.counts.disagreements {
            errors.push(format!(
                "disagreements count mismatch: expected {}, actual {}",
                audit.counts.disagreements, counts.disagreements
            ));
        }
    }
    if errors.is_empty() {
        Ok(updated)
    } else {
        Err(errors.join("\n"))
    }
}

fn render_lineage_audit(audit: &LineageAudit) -> String {
    use std::fmt::Write;

    // Preserve the fixture's group indentation and compact transitions so a
    // refresh changes only the affected rows and counts, not every array.
    let mut rendered = String::from("{\n  \"groups\": [\n");
    for (index, group) in audit.groups.iter().enumerate() {
        if index > 0 {
            rendered.push_str(",\n");
        }
        writeln!(
            rendered,
            "  {{\n    \"reason\": {},\n    \"rows\": {{",
            serde_json::to_string(&group.reason).unwrap()
        )
        .unwrap();
        for (index, (key, [before, after])) in group.rows.iter().enumerate() {
            if index > 0 {
                rendered.push_str(",\n");
            }
            write!(
                rendered,
                "      {}: [{}, {}]",
                serde_json::to_string(key).unwrap(),
                serde_json::to_string(before).unwrap(),
                serde_json::to_string(after).unwrap()
            )
            .unwrap();
        }
        rendered.push_str("\n    }\n  }");
    }
    rendered.push_str("\n  ],\n  \"counts\": ");
    rendered.push_str(
        &serde_json::to_string_pretty(&audit.counts)
            .unwrap()
            .replace('\n', "\n  "),
    );
    rendered.push_str("\n}\n");
    rendered
}

fn planted_lineage_audit() -> (Value, LineageAudit, CatalogGetResponse) {
    let seed = json!({"reseller": {"models": {"new": {}}}});
    let audit = LineageAudit {
        groups: vec![DifferenceGroup {
            reason: format!("{NEWLY_CLASSIFIED_REASON}; the upstream family establishes lineage."),
            rows: vec![("reseller/gone".into(), ["other".into(), "gpt".into()])],
        }],
        counts: LineageCounts {
            coverage: BTreeMap::from([("gpt".into(), 1)]),
            newly_classified: 1,
            disagreements: 0,
        },
    };
    let (store, _dir) = empty_store();
    let mut response = get(&store, json!({}));
    response.models.insert(
        "reseller/new".into(),
        BTreeMap::from([("model.lineage".into(), json!("qwen"))]),
    );
    (seed, audit, response)
}

#[test]
fn lineage_audit_update_adds_newly_classified_and_drops_seed_absent_rows() {
    let (seed, audit, response) = planted_lineage_audit();
    let updated = compare_lineage_seed(&seed, &audit, &response, true).unwrap();
    assert_eq!(updated.groups[0].reason, audit.groups[0].reason);
    assert_eq!(
        updated.groups[0].rows,
        vec![("reseller/new".into(), ["other".into(), "qwen".into()])]
    );
    assert_eq!(
        updated.counts.coverage,
        BTreeMap::from([("qwen".into(), 1)])
    );
    assert_eq!(updated.counts.newly_classified, 1);
    assert_eq!(updated.counts.disagreements, 0);
    let rendered = render_lineage_audit(&updated);
    assert!(rendered.contains("\"reseller/new\": [\"other\", \"qwen\"]"));
    let reparsed = serde_json::from_str(&rendered).unwrap();
    assert_eq!(
        compare_lineage_seed(&seed, &reparsed, &response, false).unwrap(),
        updated
    );
    let again = compare_lineage_seed(&seed, &reparsed, &response, true).unwrap();
    assert_eq!(render_lineage_audit(&again), rendered);
}

#[test]
fn lineage_audit_update_rejects_every_unreviewed_disagreement_by_name() {
    let (_, mut audit, mut response) = planted_lineage_audit();
    audit.groups[0].rows.clear();
    let seed = json!({"openai": {"models": {"new": {}}}, "anthropic": {"models": {"new": {}}}});
    for key in ["openai/new", "anthropic/new"] {
        response.models.insert(
            key.into(),
            BTreeMap::from([("model.lineage".into(), json!("kimi"))]),
        );
    }
    let error = compare_lineage_seed(&seed, &audit, &response, true).unwrap_err();
    assert!(
        error.contains("unreviewed lineage difference: openai/new"),
        "{error}"
    );
    assert!(
        error.contains("unreviewed lineage difference: anthropic/new"),
        "{error}"
    );
}

#[test]
fn lineage_audit_strict_names_unlisted_stale_and_count_mismatches() {
    let (seed, mut audit, response) = planted_lineage_audit();
    audit.counts.newly_classified = 99;
    audit.counts.disagreements = 99;
    let error = compare_lineage_seed(&seed, &audit, &response, false).unwrap_err();
    for expected in [
        "unreviewed lineage difference: reseller/new",
        "stale routing difference: reseller/gone",
        "lineage coverage mismatch: qwen",
        "lineage coverage mismatch: gpt",
        "newly_classified count mismatch",
        "disagreements count mismatch",
    ] {
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn lineage_audit_update_preserves_reviewed_disagreements_and_rejects_changed_rows() {
    let (_, mut audit, mut response) = planted_lineage_audit();
    let seed = json!({"openai": {"models": {"reviewed": {}}}});
    audit.groups.push(DifferenceGroup {
        reason: "The upstream family, rather than the provider, establishes lineage.".into(),
        rows: vec![("openai/reviewed".into(), ["gpt".into(), "kimi".into()])],
    });
    response.models.insert(
        "openai/reviewed".into(),
        BTreeMap::from([("model.lineage".into(), json!("kimi"))]),
    );
    let updated = compare_lineage_seed(&seed, &audit, &response, true).unwrap();
    assert_eq!(updated.groups[1], audit.groups[1]);
    assert_eq!(updated.counts.disagreements, 1);
    compare_lineage_seed(&seed, &updated, &response, false).unwrap();

    response
        .models
        .get_mut("openai/reviewed")
        .unwrap()
        .insert("model.lineage".into(), json!("qwen"));
    let error = compare_lineage_seed(&seed, &updated, &response, true).unwrap_err();
    assert!(
        error.contains("unreviewed lineage difference: openai/reviewed"),
        "{error}"
    );
    response
        .models
        .get_mut("openai/reviewed")
        .unwrap()
        .insert("model.lineage".into(), json!("gpt"));
    let error = compare_lineage_seed(&seed, &updated, &response, true).unwrap_err();
    assert!(
        error.contains("stale routing difference: openai/reviewed"),
        "{error}"
    );
}

#[test]
fn lineage_audit_update_requires_existing_reason_and_keeps_schema_closed() {
    let (seed, mut audit, response) = planted_lineage_audit();
    audit.groups.clear();
    let error = compare_lineage_seed(&seed, &audit, &response, true).unwrap_err();
    assert!(
        error.contains("missing newly-classified reason group"),
        "{error}"
    );
    assert!(error.contains("reseller/new"), "{error}");
    for path in [vec![], vec!["counts"], vec!["groups", "0"]] {
        let mut document = serde_json::to_value(planted_lineage_audit().1).unwrap();
        let mut object = &mut document;
        for key in path {
            object = if key == "0" {
                &mut object[0]
            } else {
                &mut object[key]
            };
        }
        object
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), json!(true));
        assert!(serde_json::from_value::<LineageAudit>(document).is_err());
    }
}

#[test]
fn lineage_seed_parity_with_dated_routing_globs_has_only_explicit_differences() {
    let audit: LineageAudit =
        serde_json::from_str(include_str!("model_lineage_routing_differences.json")).unwrap();
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
    let update = std::env::var("UPDATE_LINEAGE_AUDIT").is_ok_and(|value| value == "1");
    let checked = compare_lineage_seed(&seed, &audit, &response, update).unwrap();
    let rendered = render_lineage_audit(&checked);
    if update {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/route/model_lineage_routing_differences.json"
        );
        std::fs::write(path, rendered).unwrap();
        eprintln!("lineage audit regenerated at {path} — review the diff");
    } else {
        let fixture = include_str!("model_lineage_routing_differences.json");
        if rendered != fixture {
            let difference = rendered
                .lines()
                .zip(fixture.lines())
                .enumerate()
                .find(|(_, (actual, expected))| actual != expected)
                .map(|(index, lines)| (index + 1, lines));
            panic!("lineage audit formatting must be stable: first differing line {difference:?}; rendered lines={}, fixture lines={}", rendered.lines().count(), fixture.lines().count());
        }
    }
    println!("seed lineage counts: {:?}", checked.counts);
}
