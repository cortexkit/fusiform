//! A reviewed file lets consumers adopt provider changes without a live fetch.

use std::collections::{BTreeMap, BTreeSet};

use fusiform_core::normalize::RawProvider;
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

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HandKeptProvider {
    id: String,
    name: String,
    doc: String,
    source_ref: String,
    established_at_ms: i64,
    review_by_ms: i64,
}

#[derive(Deserialize)]
struct HandKeptDoc {
    schema: String,
    providers: Vec<HandKeptProvider>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct Provider {
    id: String,
    name: String,
    doc: String,
    source: &'static str,
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
    let doc: HandKeptDoc = serde_json::from_str(HAND_KEPT).expect("curated provider table parses");
    assert_eq!(doc.schema, "curated-providers/v1");
    let mut ids = BTreeSet::new();
    for row in &doc.providers {
        assert!(
            [&row.id, &row.name, &row.doc, &row.source_ref]
                .iter()
                .all(|field| !field.trim().is_empty()),
            "curated provider identity, name, docs and source must be nonempty"
        );
        assert!(ids.insert(&row.id), "duplicate hand-kept id: {}", row.id);
    }
    doc.providers
}

fn seed() -> BTreeMap<String, RawProvider> {
    serde_json::from_str(SEED).expect("the embedded models.dev seed parses")
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
                env: raw.env,
                output_modalities: outputs.into_iter().collect(),
            }
        })
        .chain(rows.into_iter().map(|row| Provider {
            id: row.id,
            name: row.name,
            doc: row.doc,
            source: "hand_kept",
            env: Vec::new(),
            output_modalities: Vec::new(),
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
    let mut text = serde_json::to_string_pretty(&vector(seed(), hand_kept(), fetched_at.into()))
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
        established_at_ms: 1,
        review_by_ms: 1 + REVIEW_INTERVAL_MS,
    };
    let vector = vector(seed, vec![hand_kept], "2000-01-01T00:00:00Z".into());
    assert_eq!(vector.format, "providers-v1");
    assert_eq!(vector.snapshot_fetched_at, "2000-01-01T00:00:00Z");
    let ids: Vec<_> = vector.providers.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, ["a-empty", "m-hand-kept", "z-synthetic"]);
    assert!(vector.providers[0].env.is_empty());
    assert!(vector.providers[0].output_modalities.is_empty());
    assert_eq!(vector.providers[1].source, "hand_kept");
    assert!(vector.providers[1].env.is_empty());
    assert!(vector.providers[1].output_modalities.is_empty());
    assert_eq!(
        vector.providers[2],
        Provider {
            id: "z-synthetic".into(),
            name: "Synthetic".into(),
            doc: "https://example.test/docs".into(),
            source: "models.dev",
            env: vec!["SECOND_KEY".into(), "FIRST_KEY".into(), "SECOND_KEY".into()],
            output_modalities: vec![
                "audio".into(),
                "embedding".into(),
                "future-output".into(),
                "text".into()
            ],
        }
    );
}
