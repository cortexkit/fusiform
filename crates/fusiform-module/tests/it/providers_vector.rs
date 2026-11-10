//! A reviewed file lets consumers adopt provider changes without a live fetch.

use std::collections::{BTreeMap, BTreeSet};

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::RawProvider;
use fusiform_core::{BoundaryKind, Timestamp};
use fusiform_store::CatalogStore;
use serde::{Deserialize, Serialize};

const SEED: &str = include_str!("../../data/models-dev-seed.json");
const META: &str = include_str!("../../data/models-dev-seed.meta.json");
const HAND_KEPT: &str = include_str!("../../data/curated-providers.json");
const VECTOR_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../test-vectors/providers-v1.json"
);
const REGENERATE: &str = "UPDATE_GOLDEN=1 cargo test --locked -p fusiform-module --test it providers_vector::providers_match_the_committed_vector";
const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;

type ServedModels = BTreeMap<String, BTreeMap<String, serde_json::Value>>;
type ProviderOutputs = BTreeMap<String, BTreeSet<String>>;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ProviderKind {
    WebSearch,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HandKeptProvider {
    id: String,
    name: String,
    doc: String,
    source_ref: String,
    kind: Option<ProviderKind>,
    established_at_ms: i64,
    review_by_ms: i64,
}

#[derive(Deserialize)]
struct HandKeptDoc {
    schema: String,
    providers: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct Provider {
    id: String,
    name: String,
    doc: String,
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<ProviderKind>,
    env: Vec<String>,
    output_modalities: Vec<String>,
}

#[derive(Serialize)]
struct ProvidersVector {
    format: &'static str,
    snapshot_fetched_at: String,
    providers: Vec<Provider>,
}

fn hand_kept() -> Vec<HandKeptProvider> {
    let (providers, rejects) = load_hand_kept_reporting_rejects(HAND_KEPT);
    assert!(rejects.is_empty(), "curated provider rejects: {rejects:?}");
    let mut ids = BTreeSet::new();
    for row in &providers {
        assert!(
            [&row.id, &row.name, &row.doc, &row.source_ref]
                .iter()
                .all(|field| !field.trim().is_empty()),
            "curated provider identity, name, docs and source must be nonempty"
        );
        assert!(ids.insert(&row.id), "duplicate hand-kept id: {}", row.id);
    }
    providers
}

fn load_hand_kept_reporting_rejects(input: &str) -> (Vec<HandKeptProvider>, Vec<String>) {
    let doc: HandKeptDoc = serde_json::from_str(input).expect("curated provider table parses");
    assert_eq!(doc.schema, "curated-providers/v1");
    let mut providers = Vec::new();
    let mut rejects = Vec::new();
    for (index, row) in doc.providers.into_iter().enumerate() {
        let id = row["id"].as_str().unwrap_or("<missing id>").to_owned();
        match serde_json::from_value(row) {
            Ok(provider) => providers.push(provider),
            Err(error) => rejects.push(format!("row {index} ({id}): {error}")),
        }
    }
    (providers, rejects)
}

fn seed() -> BTreeMap<String, RawProvider> {
    serde_json::from_str(SEED).expect("the embedded models.dev seed parses")
}

fn served_models() -> ServedModels {
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
    let catalog = fusiform_core::normalize_models_dev(SEED.as_bytes())
        .unwrap()
        .catalog;
    let plan = fusiform_store::ingest::plan_ingest(
        &store,
        &catalog,
        Timestamp(1_000),
        BoundaryKind::Seed,
        None,
    )
    .unwrap();
    store.append_eras(&plan.eras).unwrap();

    // Read the real serving path so new row-producing tables cannot bypass the
    // provider fence. Retired-inclusive reads also retain held and retired ids.
    let mut models = BTreeMap::new();
    let mut token: Option<String> = None;
    loop {
        let mut request = serde_json::json!({"paged": true, "include_retired": true});
        if let Some(next) = token {
            request["page_token"] = next.into();
        }
        let page = fusiform_module::route::serve_catalog_get(
            &store,
            &serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        models.extend(page.models);
        token = page.next_page_token;
        if token.is_none() {
            return models;
        }
    }
}

fn served_outputs(models: ServedModels) -> ProviderOutputs {
    let mut outputs = ProviderOutputs::new();
    for (identity, facts) in models {
        let (provider, _) = identity.split_once('/').expect("provider/model identity");
        if let Some(modalities) = facts.get("capability.output_modalities") {
            outputs.entry(provider.into()).or_default().extend(
                modalities
                    .as_array()
                    .expect("output modalities are an array")
                    .iter()
                    .map(|value| value.as_str().expect("output modality is a string").into()),
            );
        }
    }
    outputs
}

fn assert_no_upstream_ids(seed: &BTreeMap<String, RawProvider>, rows: &[HandKeptProvider]) {
    for row in rows {
        assert!(
            !seed.contains_key(&row.id),
            "hand-kept id {} is already in models.dev; remove the curated row so upstream wins",
            row.id
        );
    }
}

fn review_problems_at(rows: &[HandKeptProvider], now_ms: i64) -> Vec<String> {
    rows.iter()
        .filter_map(|row| {
            if row.established_at_ms <= 0
                || row.review_by_ms.checked_sub(row.established_at_ms) != Some(REVIEW_INTERVAL_MS)
            {
                Some(format!(
                    "{}: review_by_ms must be exactly 60 days after established_at_ms",
                    row.id
                ))
            } else if row.review_by_ms < now_ms {
                Some(format!(
                    "{}: re-verify {} and re-date established_at_ms and review_by_ms (60 days later), or remove the curated row",
                    row.id, row.source_ref
                ))
            } else {
                None
            }
        })
        .collect()
}

fn vector(
    seed: BTreeMap<String, RawProvider>,
    rows: Vec<HandKeptProvider>,
    snapshot_fetched_at: String,
    served_outputs: &ProviderOutputs,
) -> ProvidersVector {
    assert_no_upstream_ids(&seed, &rows);
    let mut providers: Vec<Provider> = seed
        .into_iter()
        .map(|(id, raw)| {
            let mut outputs = BTreeSet::new();
            for model in raw.models.values() {
                if let Some(modalities) = &model.modalities {
                    outputs.extend(modalities.output.iter().cloned());
                }
                if model.model_type.as_deref() == Some("embedding") {
                    outputs.insert("embedding".into());
                }
            }
            Provider {
                id,
                name: raw.name.expect("seed providers have published names"),
                doc: raw.doc.expect("seed providers have documentation URLs"),
                source: "models.dev",
                kind: None,
                env: raw.env,
                output_modalities: outputs.into_iter().collect(),
            }
        })
        .chain(rows.into_iter().map(|row| Provider {
            output_modalities: if row.kind == Some(ProviderKind::WebSearch) {
                Vec::new()
            } else {
                served_outputs
                    .get(&row.id)
                    .map(|outputs| outputs.iter().cloned().collect())
                    .unwrap_or_default()
            },
            id: row.id,
            name: row.name,
            doc: row.doc,
            source: "hand_kept",
            kind: row.kind,
            env: Vec::new(),
        }))
        .collect();
    providers.sort_by(|a, b| a.id.cmp(&b.id));
    ProvidersVector {
        format: "providers-v1",
        snapshot_fetched_at,
        providers,
    }
}

fn render() -> String {
    // SeedMeta intentionally keeps only the epoch instant. The vector publishes
    // the refresh script's readable timestamp verbatim, not a generation time.
    let meta: serde_json::Value = serde_json::from_str(META).expect("seed metadata parses");
    let fetched_at = meta["fetched_at"]
        .as_str()
        .expect("seed metadata has fetched_at");
    let mut text = serde_json::to_string_pretty(&vector(
        seed(),
        hand_kept(),
        fetched_at.into(),
        &served_outputs(served_models()),
    ))
    .expect("the provider vector serializes");
    text.push('\n');
    text
}

#[test]
fn providers_match_the_committed_vector() {
    let rendered = render();
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(VECTOR_PATH, &rendered).expect("write the provider vector");
        eprintln!("provider vector regenerated at {VECTOR_PATH} — review the diff");
        return;
    }
    let committed = std::fs::read_to_string(VECTOR_PATH).unwrap_or_else(|error| {
        panic!("read {VECTOR_PATH}: {error}; regenerate with:\n  {REGENERATE}")
    });
    assert!(
        committed == rendered,
        "the provider vector changed. A consumer pins these bytes.\nRegenerate on the Linux runner with:\n  {REGENERATE}\nand review test-vectors/providers-v1.json"
    );
}

#[test]
fn every_served_model_provider_is_in_the_vector() {
    let committed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(VECTOR_PATH).unwrap()).unwrap();
    let providers: BTreeSet<_> = committed["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    let missing: BTreeSet<_> = served_models()
        .into_keys()
        .filter_map(|identity| {
            let (provider, _) = identity.split_once('/').expect("provider/model identity");
            (!providers.contains(provider)).then(|| provider.to_owned())
        })
        .collect();
    assert!(
        missing.is_empty(),
        "served model providers missing from the provider vector: {missing:?}"
    );
}

#[test]
fn hand_kept_ids_are_absent_from_the_upstream_seed() {
    assert_no_upstream_ids(&seed(), &hand_kept());
}

#[test]
fn hand_kept_providers_are_within_their_review_dates() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_millis() as i64;
    let problems = review_problems_at(&hand_kept(), now);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn review_deadline_requires_reverification_or_removal() {
    for mut row in hand_kept() {
        assert!(review_problems_at(&[row.clone()], row.review_by_ms).is_empty());
        let problems = review_problems_at(&[row.clone()], row.review_by_ms + 1);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains(&row.id));
        assert!(problems[0].contains(&row.source_ref));
        assert!(problems[0].contains("re-verify"));
        assert!(problems[0].contains("or remove"));
        row.review_by_ms -= 1;
        let problems = review_problems_at(&[row.clone()], row.established_at_ms);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("exactly 60 days"));
    }
}

#[test]
fn unknown_provider_kind_is_a_counted_reject() {
    let row = serde_json::json!({
        "id": "search", "name": "Search", "doc": "https://example.test/docs",
        "source_ref": "https://example.test/docs", "kind": "web_search",
        "established_at_ms": 1, "review_by_ms": 1 + REVIEW_INTERVAL_MS
    });
    let mut unknown = row.clone();
    unknown["id"] = "unknown".into();
    unknown["kind"] = "web_serach".into();
    let mut model = row.clone();
    model["id"] = "model".into();
    model.as_object_mut().unwrap().remove("kind");
    let doc = serde_json::json!({
        "schema": "curated-providers/v1", "providers": [row, unknown, model]
    });
    let (providers, rejects) = load_hand_kept_reporting_rejects(&doc.to_string());
    assert_eq!(
        rejects.len(),
        1,
        "unknown kinds must be counted: {rejects:?}"
    );
    assert!(rejects[0].contains("unknown"), "{:?}", rejects[0]);
    assert!(rejects[0].contains("web_serach"), "{:?}", rejects[0]);
    assert_eq!(
        providers
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        ["search", "model"]
    );
}

#[test]
fn web_search_kind_is_preserved_without_model_outputs() {
    let row = HandKeptProvider {
        id: "search".into(),
        name: "Search".into(),
        doc: "https://example.test/docs".into(),
        source_ref: "https://example.test/docs".into(),
        kind: Some(ProviderKind::WebSearch),
        established_at_ms: 1,
        review_by_ms: 1 + REVIEW_INTERVAL_MS,
    };
    let outputs = BTreeMap::from([("search".into(), BTreeSet::from(["text".into()]))]);
    let vector = vector(
        BTreeMap::new(),
        vec![row],
        "seed-timestamp".into(),
        &outputs,
    );
    let serialized = serde_json::to_value(&vector).unwrap();
    assert_eq!(
        serialized["providers"],
        serde_json::json!([{
            "id": "search", "name": "Search", "doc": "https://example.test/docs",
            "source": "hand_kept", "kind": "web_search", "env": [], "output_modalities": []
        }])
    );
}

#[test]
fn vector_preserves_metadata_and_unions_outputs_without_inference() {
    let seed = serde_json::from_str(
        r#"{
            "z-synthetic": {
                "name": "Synthetic", "doc": "https://example.test/docs",
                "env": ["SECOND_KEY", "FIRST_KEY", "SECOND_KEY"],
                "models": {
                    "chat": {"modalities": {"output": ["text", "audio", "text"]}},
                    "embed": {"type": "embedding", "modalities": {"output": ["text"]}},
                    "unknown-output": {"modalities": {"output": ["future-output"]}},
                    "unstated": {}
                }
            },
            "a-empty": {"name": "Empty", "doc": "https://example.test/empty", "models": {}}
        }"#,
    )
    .unwrap();
    let hand_kept = HandKeptProvider {
        id: "m-hand-kept".into(),
        name: "Hand-kept synthetic".into(),
        doc: "https://example.test/hand-kept".into(),
        source_ref: "https://example.test/hand-kept".into(),
        kind: None,
        established_at_ms: 1,
        review_by_ms: 1 + REVIEW_INTERVAL_MS,
    };
    let no_models = HandKeptProvider {
        id: "n-no-models".into(),
        ..hand_kept.clone()
    };
    let models = serde_json::from_str(
        r#"{
            "m-hand-kept/chat": {"capability.output_modalities": ["text", "audio", "text"]},
            "m-hand-kept/namespace/other": {"capability.output_modalities": ["future-output", "text"]},
            "m-hand-kept/unstated": {"existence": "absent"},
            "unrelated/model": {"capability.output_modalities": ["image"]}
        }"#,
    )
    .unwrap();
    let vector = vector(
        seed,
        vec![hand_kept, no_models],
        "2000-01-01T00:00:00Z".into(),
        &served_outputs(models),
    );
    assert_eq!(vector.format, "providers-v1");
    assert_eq!(vector.snapshot_fetched_at, "2000-01-01T00:00:00Z");
    let ids: Vec<_> = vector.providers.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        ids,
        ["a-empty", "m-hand-kept", "n-no-models", "z-synthetic"]
    );
    assert!(vector.providers[0].env.is_empty());
    assert!(vector.providers[0].output_modalities.is_empty());
    assert_eq!(vector.providers[1].source, "hand_kept");
    assert!(vector.providers[1].env.is_empty());
    assert_eq!(
        vector.providers[1].output_modalities,
        ["audio", "future-output", "text"]
    );
    assert!(vector.providers[2].env.is_empty());
    assert!(vector.providers[2].output_modalities.is_empty());
    assert_eq!(
        vector.providers[3],
        Provider {
            id: "z-synthetic".into(),
            name: "Synthetic".into(),
            doc: "https://example.test/docs".into(),
            source: "models.dev",
            kind: None,
            env: vec!["SECOND_KEY".into(), "FIRST_KEY".into(), "SECOND_KEY".into()],
            output_modalities: vec![
                "audio".into(),
                "embedding".into(),
                "future-output".into(),
                "text".into()
            ],
        }
    );
    let serialized = serde_json::to_value(&vector).unwrap();
    for provider in serialized["providers"].as_array().unwrap() {
        assert!(provider.get("kind").is_none(), "{provider}");
    }
}
