use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::v2::NormalizeV2Outcome;
use fusiform_core::normalize::NormalizedModel;
use fusiform_core::upstream_facts::{facts_of, Existence, FactKey};
use fusiform_core::{normalize_models_dev_v2_with, BoundaryKind};
use fusiform_protocol::{served_fact_key_matches, SERVED_FACTS};
use fusiform_store::serve::CatalogQuery;
use fusiform_store::NewEra;
use serde::{de::IgnoredAny, Deserialize};
use serde_json::{json, Value};

use super::*;
use crate::route::{serve_tool_call, ToolResponse};

const SOURCE: SourceId = SourceId::ModelsDev;
const SEED_AT: Timestamp = Timestamp(1_000);
const SWITCH_AT: Timestamp = Timestamp(2_000);
type Facts = BTreeMap<String, String>;
type Identity = (String, String);
type RecordedModels = BTreeMap<Identity, Facts>;
type ServedModels = BTreeMap<String, BTreeMap<String, Value>>;
const NAMED_WITNESSES: &[&str] = &[
    "openai/gpt-realtime-2.1",
    "openai/gpt-transcribe",
    "openai/gpt-live-transcribe",
    "google/gemini-3.5-transcribe",
    "google/gemini-3.8-flash-tts",
    "google/gemini-omni-1.2-flash",
    "google/gemini-decision-1",
];

#[derive(Deserialize)]
struct Fixture {
    conflicts: BTreeSet<Conflict>,
    fixture_format: u32,
    providers: BTreeMap<String, Provider>,
}

#[derive(Deserialize)]
struct Provider {
    api_base_quarantined: bool,
    models: BTreeMap<String, Facts>,
    npm_quarantined: bool,
    provider_name: Option<String>,
}

#[derive(Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
struct Conflict {
    provider: String,
    model: String,
    fact: String,
    values: Vec<String>,
}

// An absent fact differs from a fact whose stored JSON is the string "null".
type Change = (Option<String>, Option<String>);
type FactId = (String, String, String);
type Changes = BTreeMap<FactId, Change>;
type ExplainedRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
);
type HeldRow = (String, String, String, Option<String>, Option<String>);

#[derive(Deserialize)]
struct Explained {
    causes: Vec<(String, Option<String>, String)>,
    normalized: Vec<ExplainedRow>,
    held_rates: Vec<HeldRow>,
    served_wire: Vec<ExplainedRow>,
    v1_provider_defaults: Vec<(String, bool, bool)>,
}

impl Explained {
    fn expect(&self, rows: &[ExplainedRow]) -> Changes {
        let mut changes = Changes::new();
        for (provider, model, fact, old, new, cause) in rows {
            let label = (provider.clone(), model.clone(), fact.clone());
            let (_, source, reason) = self
                .causes
                .iter()
                .find(|(name, _, _)| name == cause)
                .unwrap_or_else(|| panic!("unknown explanation {cause} for {label:?}"));
            assert!(!reason.is_empty());
            assert!(
                source
                    .as_ref()
                    .is_some_and(|s| s.starts_with("translate-397db8d.ts:"))
                    || source.is_none()
                        && matches!(
                            cause.as_str(),
                            "snapshot_capability" | "snapshot_bedrock_tier"
                        ),
                "{label:?} must cite the projection or be a snapshot difference"
            );
            assert_ne!(old, new, "{label:?} is not a change");
            assert!(
                changes.insert(label, (old.clone(), new.clone())).is_none(),
                "duplicate explained change"
            );
        }
        changes
    }

    fn expected_holds(&self) -> Changes {
        let mut changes = Changes::new();
        for (provider, model, fact, old, new) in &self.held_rates {
            assert!(fact.starts_with("rate.") && old.is_some() && new.is_none());
            assert!(
                changes
                    .insert(
                        (provider.clone(), model.clone(), fact.clone()),
                        (old.clone(), new.clone())
                    )
                    .is_none(),
                "duplicate held rate"
            );
        }
        changes
    }
}

fn data_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../fusiform-core/tests/data/models-dev-v2")
}

fn saved_gzip(name: &str) -> Vec<u8> {
    let path = data_dir().join(name);
    let output = Command::new("gzip").arg("-dc").arg(&path).output().unwrap();
    assert!(
        output.status.success(),
        "decompress {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn fixture() -> Fixture {
    let fixture: Fixture = serde_json::from_slice(&saved_gzip("v1-fixture.json.gz")).unwrap();
    assert_eq!(fixture.fixture_format, 1);
    fixture
}

fn explained() -> Explained {
    serde_json::from_slice(&saved_gzip("explained-differences.json.gz")).unwrap()
}

fn parse_v2(bytes: &[u8]) -> NormalizeV2Outcome {
    normalize_models_dev_v2_with(bytes, &crate::input_audio::evidence()).unwrap()
}

fn empty_store() -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_str().unwrap().into(),
        },
    })
    .unwrap();
    (store, dir)
}

fn seed(fixture: &Fixture) -> (CatalogStore, tempfile::TempDir) {
    let (store, dir) = empty_store();
    let observation_id = store
        .record_observation(&NewObservation {
            source: SOURCE,
            observed_at: SEED_AT,
            outcome: ObservationOutcome::Seeded,
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let mut eras = Vec::new();
    for (provider_id, provider) in &fixture.providers {
        for (model_id, facts) in &provider.models {
            let existence = (
                FactKey::existence(),
                Existence::Present.as_json().to_owned(),
            );
            for (fact_key, value_json) in std::iter::once(existence).chain(
                facts
                    .iter()
                    .map(|(key, value)| (FactKey::from_stored(key.clone()), value.clone())),
            ) {
                eras.push(NewEra {
                    source: SOURCE,
                    provider_id: provider_id.clone(),
                    model_id: model_id.clone(),
                    fact_key,
                    value_json,
                    boundary_at: SEED_AT,
                    boundary_kind: BoundaryKind::Seed,
                    observation_id: None,
                });
            }
        }
    }
    assert_eq!(store.append_eras(&eras).unwrap(), eras.len());
    store
        .advance_catalog_version(observation_id, SEED_AT)
        .unwrap();
    (store, dir)
}

fn apply_v2(store: &CatalogStore, signals: &Signals, bytes: Vec<u8>, at: Timestamp) -> TickReport {
    apply_with_normalizer(
        &ApplyCtx {
            store,
            signals,
            source: SOURCE,
        },
        FetchOutcome::Body {
            bytes,
            etag: None,
            duration: Duration::ZERO,
        },
        at.0,
        false,
        |store, source| store.catalog_counts(source, None),
        |bytes| {
            let outcome = normalize_models_dev_with(bytes, &crate::input_audio::evidence(), at)?;
            let counts = outcome.finding_counts();
            Ok((outcome.catalog, counts))
        },
    )
    .unwrap()
}

fn served(store: &CatalogStore, prefixes: &[&str]) -> ServedModels {
    let mut cursor: Option<String> = None;
    let mut models = BTreeMap::new();
    loop {
        let mut arguments = json!({"paged": true, "fact_prefixes": prefixes});
        if let Some(token) = cursor {
            arguments["page_token"] = Value::String(token);
        }
        let request =
            serde_json::to_vec(&json!({"name":"catalog.get", "arguments":arguments})).unwrap();
        let ToolResponse::Catalog(response) = serve_tool_call(store, &request).unwrap() else {
            panic!("expected catalog.get response");
        };
        for (model, facts) in response.models {
            assert!(
                models.insert(model, facts).is_none(),
                "model repeated across pages"
            );
        }
        cursor = response.next_page_token;
        if cursor.is_none() {
            return models;
        }
    }
}

fn served_model(store: &CatalogStore, provider: &str, model: &str) -> BTreeMap<String, Value> {
    let request = serde_json::to_vec(&json!({"name":"catalog.get", "arguments":{
        "provider_id":provider,"model_id":model
    }}))
    .unwrap();
    let ToolResponse::Catalog(mut response) = serve_tool_call(store, &request).unwrap() else {
        panic!("expected catalog.get response");
    };
    response
        .models
        .remove(&format!("{provider}/{model}"))
        .unwrap()
}

#[test]
fn owner_media_date_schedule_opens_an_era_for_the_same_bytes() {
    // The publisher announced the price change for 2027-01-01 UTC.
    const JAN_1_2027: Timestamp = Timestamp(1_798_761_600_000);
    let bytes = saved_gzip("api-v2-20261125.json.gz");
    let (store, _dir) = empty_store();
    let signals = Signals::new();
    let before = Timestamp(JAN_1_2027.0 - 1);
    apply_v2(&store, &signals, bytes.clone(), before);
    let amount = |at, key| {
        let (row, _) = store
            .read_model(SOURCE, "google", "gemini-3.8-flash-tts", Some(at))
            .unwrap();
        row.and_then(|row| {
            row.facts
                .get(&FactKey::from_stored(key))
                .and_then(|rate| serde_json::from_str::<Value>(rate).unwrap()["units"].as_i64())
        })
    };
    assert_eq!(amount(before, "rate.input".into()), Some(500_000_000));
    assert_eq!(
        amount(before, "rate.output_audio".into()),
        Some(9_000_000_000)
    );
    let changed = apply_v2(&store, &signals, bytes.clone(), JAN_1_2027);
    assert!(changed.eras_written > 0);
    assert_eq!(amount(before, "rate.input".into()), Some(500_000_000));
    assert_eq!(amount(JAN_1_2027, "rate.input".into()), Some(1_000_000_000));
    assert_eq!(
        amount(JAN_1_2027, "rate.output_audio".into()),
        Some(18_000_000_000)
    );
    let replay = apply_v2(&store, &signals, bytes, Timestamp(JAN_1_2027.0 + 1));
    assert_eq!(replay.eras_written, 0);
}

#[test]
fn owner_media_regressions_on_the_saved_snapshot_after_curation() {
    let bytes = saved_gzip("api-v2-20261125.json.gz");
    let (store, _dir) = empty_store();
    apply_v2(&store, &Signals::new(), bytes, SWITCH_AT);
    let mut differences = Vec::new();
    for (provider, model, expected) in [
        (
            "openai",
            "gpt-realtime-2.1",
            json!({"model.type":"realtime","rate.input_audio":{"units":32_000_000_000_i64},"rate.output_audio":{"units":64_000_000_000_i64},"rate.cached_input_audio":{"units":400_000_000}}),
        ),
        (
            "openai",
            "gpt-transcribe",
            json!({"model.type":"transcription","rate.input_audio.per_second":{"units":4_500_000,"per":60}}),
        ),
        (
            "openai",
            "gpt-live-transcribe",
            json!({"model.type":"transcription","rate.input_audio.per_second":{"units":17_000_000,"per":60}}),
        ),
        (
            "google",
            "gemini-3.5-transcribe",
            json!({"model.type":"transcription","limit.audio_tokens_per_second":25}),
        ),
        (
            "google",
            "gemini-3.5-transcribe-live",
            json!({"limit.audio_tokens_per_second":25}),
        ),
        (
            "google",
            "gemini-3.8-flash-tts",
            json!({"model.type":"speech","limit.output_audio_tokens_per_second":25}),
        ),
        (
            "google",
            "gemini-omni-1.2-flash",
            json!({"model.type":"video","wire.family":"google.interactions","capability.aspect_ratios":["9:16","16:9"],"capability.output_audio":true,"limit.output_duration_ms":15000,"limit.output_video_tokens_per_second.resolution.360p":1931,"limit.output_video_tokens_per_second.resolution.720p":5792,"limit.output_video_tokens_per_second.resolution.1080p":8688,"limit.output_video_tokens_per_second.resolution.4k":17376}),
        ),
        (
            "google",
            "gemini-decision-1",
            json!({"model.type":"decision","model.kind":"decision"}),
        ),
        (
            "openai",
            "gpt-image-2.6-flare",
            json!({"wire.family":"openai.images"}),
        ),
        (
            "elevenlabs",
            "eleven_flash_v2_5",
            json!({"wire.family":"elevenlabs.text_to_speech","wire.api":{"value":"https://api.elevenlabs.io/v1"}}),
        ),
        (
            "cloudflare-workers-ai",
            "@cf/cloudflare/clairic",
            json!({"model.kind":"extraction"}),
        ),
        (
            "cloudflare-workers-ai",
            "@cf/cloudflare/clef",
            json!({"model.kind":"decision"}),
        ),
        (
            "cloudflare-workers-ai",
            "@cf/cloudflare/clef-flash",
            json!({"model.kind":"decision"}),
        ),
        ("typesafe", "jev-1.13.0", json!({"model.kind":"decision"})),
        ("typesafe", "jev-1.14.0", json!({"model.kind":"decision"})),
    ] {
        let facts = served_model(&store, provider, model);
        for (key, expected) in expected.as_object().unwrap() {
            let actual = facts.get(key).unwrap_or(&Value::Null);
            let matches = if let Some(fields) = expected.as_object() {
                fields
                    .iter()
                    .all(|(field, value)| actual.get(field) == Some(value))
            } else {
                actual == expected
            };
            if !matches {
                differences.push(format!(
                    "{provider}/{model}: {key}: expected {expected}, got {actual}"
                ));
            }
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}

#[test]
fn owner_media_replaced_curated_rows_keep_their_existing_prices_and_facts() {
    use crate::curated_models;

    let old = fixture();
    let (store, _dir) = seed(&old);
    let bytes = saved_gzip("api-v2-20261125.json.gz");
    let identities = published_identities(&bytes);
    let document = curated_models::document_with_retired_test_models().to_string();
    let curated = curated_models::load_reporting_rejects(&document).0;
    let rows: Vec<_> = curated
        .keys()
        .filter(|key| identities.contains(*key))
        .collect();
    assert_eq!(rows.len(), 14);
    let before: Vec<_> = {
        let _plan = curated_models::plant(&document);
        rows.iter()
            .map(|(p, m)| served_model(&store, p, m))
            .collect()
    };
    apply_v2(&store, &Signals::new(), bytes, Timestamp(1_795_629_037_000));
    for ((p, m), old) in rows.into_iter().zip(before) {
        let new = served_model(&store, p, m);
        assert_eq!(new["existence"], "present");
        assert!(!curated_models::models().contains_key(&(p.clone(), m.clone())));
        for (key, old) in &old {
            let actual = new.get(key);
            if key.starts_with("rate.") && old["state"] == "priced" {
                let amount = |rate: &Value| {
                    assert_eq!(rate["state"], "priced", "{p}/{m}: {key}");
                    let units = i128::from(rate["units"].as_i64().unwrap());
                    let exponent = rate["exponent"].as_u64().unwrap() as u32;
                    let per = i128::from(rate["per"].as_u64().unwrap_or(1));
                    (units, 10_i128.pow(exponent) * per)
                };
                let (n, d) = amount(old);
                let (n2, d2) = amount(actual.unwrap_or(&Value::Null));
                assert_eq!(old["currency"], new[key]["currency"]);
                assert_eq!(n * d2, n2 * d, "{p}/{m}: {key}");
            } else if key == "wire.api" {
                assert_eq!(old["value"], new[key]["value"], "{p}/{m}: {key}");
            } else if key == "model.name"
                && matches!(m.as_str(), "@cf/cloudflare/clef-flash" | "gpt-transcribe")
            {
                assert_eq!(
                    if m == "gpt-transcribe" {
                        "GPT-Transcribe"
                    } else {
                        "Clef Flash"
                    },
                    new[key]
                );
            } else if matches!(
                key.as_str(),
                "limit.temperature_min" | "limit.temperature_max"
            ) && new.get("capability.temperature") == Some(&Value::Bool(false))
            {
                assert!(actual.is_none(), "{p}/{m}: {key}");
            } else {
                assert_eq!(actual, Some(old), "{p}/{m}: {key}");
            }
        }
    }
}

#[test]
fn models_dev_v2_regression_keeps_nvidia_budget_sentinels() {
    let bytes = saved_gzip("api-v2-20261125.json.gz");
    let old = fixture();
    let outcome = parse_v2(&bytes);
    for id in [
        "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning",
        "nvidia/nemotron-3-ultra-550b-a55b",
    ] {
        let model = outcome
            .catalog
            .models()
            .find(|m| m.key.provider_id == "nvidia" && m.key.model_id == id)
            .unwrap();
        let (key, actual) = facts_of(model)
            .values
            .into_iter()
            .find(|(key, _)| key.as_str() == "capability.reasoning_options")
            .unwrap();
        let expected = &old.providers["nvidia"].models[id][key.as_str()];
        assert!(expected.contains(r#""min":-1"#));
        assert_eq!(&actual, expected, "nvidia/{id}");
    }
}

#[test]
fn models_dev_v2_regression_refuses_unknown_mode_names() {
    use fusiform_core::normalize::{v2::FindingKey, RefusedFactScope};

    let base =
        json!({"type":"chat", "capabilities":{}, "api":{}, "cost":{"text":{"input":{"price":2}}}});
    let mut named = base.clone();
    named["experimental"] = json!({"modes":{"standby":{"cost":{"text":{"input":{"price":1}}}}}});
    let mut over = base.clone();
    over["cost"]["text"]["input"]["overrides"] = json!([
        {"when":{"service_tier":{"equals":"standby"}}, "price":1}
    ]);
    let mut multiplier = base;
    multiplier["cost"]["multipliers"] = json!([
        {"when":{"service_tier":{"equals":"standby"}}, "factor":{"text":0.5}}
    ]);
    let doc = json!({"p":{"name":"Provider","models":{"named":named,"override":over,"multiplier":multiplier}}});
    let outcome = parse_v2(&serde_json::to_vec(&doc).unwrap());
    for model in outcome.catalog.models() {
        let facts = facts_of(model).values;
        let has_fact = |expected| facts.iter().any(|(key, _)| key.as_str() == expected);
        assert!(has_fact("rate.input"), "{}", model.key.model_id);
        assert!(
            !has_fact("rate.input.mode.standby"),
            "{}",
            model.key.model_id
        );
        assert!(model
            .refused
            .contains(&RefusedFactScope::ModeRates(Some("standby".to_owned()))));
    }
    assert_eq!(outcome.catalog.model_count(), 3);
    assert!(outcome.findings.iter().any(|finding| matches!(
        &finding.key, FindingKey::RefusedPrice { detail, .. } if detail.contains("standby")
    )));
}

fn projected(model: &NormalizedModel) -> (Facts, impl Iterator<Item = Conflict>) {
    let facts = facts_of(model);
    let values = facts
        .values
        .into_iter()
        .map(|(key, value)| (key.as_str().to_owned(), value))
        .collect();
    let conflicts = facts.conflicts.into_iter().map(|row| Conflict {
        provider: row.provider,
        model: row.model,
        fact: row.fact.as_str().to_owned(),
        values: row.values,
    });
    (values, conflicts)
}

fn assert_changes(actual: &Changes, expected: &Changes, scope: &str) {
    let discrepant = |key: &FactId, a: &Change, other: &Changes| other.get(key) != Some(a);
    let unexpected: Vec<_> = actual
        .iter()
        .filter(|(key, a)| discrepant(key, a, expected))
        .take(5)
        .collect();
    let missing: Vec<_> = expected
        .iter()
        .filter(|(key, a)| discrepant(key, a, actual))
        .take(5)
        .collect();
    assert!(
        unexpected.is_empty() && missing.is_empty(),
        "{scope}: actual {} expected {}; unexpected {unexpected:?}; missing {missing:?}",
        actual.len(),
        expected.len()
    );
}

fn fact_counts(changes: &Changes) -> BTreeMap<&str, usize> {
    let mut counts = BTreeMap::new();
    for (_, _, key) in changes.keys() {
        *counts.entry(key.as_str()).or_default() += 1;
    }
    counts
}

#[test]
fn models_dev_v2_shared_projection_matches_the_explained_set() {
    let old = fixture();
    let v2 = parse_v2(&saved_gzip("api-v2-20261125.json.gz"));
    let explained = explained();
    let mut changes = Changes::new();
    let mut held_rates = Changes::new();
    let mut conflicts = BTreeSet::new();
    let mut shared = BTreeSet::new();
    let mut defaults = BTreeSet::new();
    for p in &v2.catalog.providers {
        let Some(prior) = old.providers.get(&p.provider_id) else {
            continue;
        };
        // The published v2 provider object has no defaults. Keep this metadata
        // comparison separate from stored facts and check the exact v1 set.
        assert!(
            !p.api_base_quarantined && !p.npm_quarantined,
            "{}",
            p.provider_id
        );
        if prior.api_base_quarantined || prior.npm_quarantined {
            defaults.insert((
                p.provider_id.clone(),
                prior.api_base_quarantined,
                prior.npm_quarantined,
            ));
        }
        for m in &p.models {
            let Some(previous) = prior.models.get(&m.key.model_id) else {
                continue;
            };
            assert!(shared.insert((p.provider_id.clone(), m.key.model_id.clone())));
            assert_eq!(m.provider_name, prior.provider_name, "{:?}", m.key);
            let (next, model_conflicts) = projected(m);
            conflicts.extend(model_conflicts);
            for key in previous.keys().chain(next.keys()).collect::<BTreeSet<_>>() {
                let (old, new) = (previous.get(key), next.get(key));
                if old == new {
                    continue;
                }
                let label = (p.provider_id.clone(), m.key.model_id.clone(), key.clone());
                let entry = (old.cloned(), new.cloned());
                if new.is_none()
                    && m.refused
                        .iter()
                        .any(|scope| scope.covers(&FactKey::from_stored(key.clone())))
                {
                    assert!(held_rates.insert(label, entry).is_none());
                } else {
                    assert!(changes.insert(label, entry).is_none());
                }
            }
        }
    }
    assert_eq!(shared.len(), 10_052);
    let old_conflicts = old
        .conflicts
        .into_iter()
        .filter(|row| shared.contains(&(row.provider.clone(), row.model.clone())))
        .collect();
    assert_eq!(conflicts, old_conflicts);
    assert_eq!(
        defaults,
        explained.v1_provider_defaults.iter().cloned().collect()
    );
    let expected = explained.expect(&explained.normalized);
    eprintln!(
        "explained normalized changes by fact: {:?}",
        fact_counts(&expected)
    );
    assert_changes(&changes, &expected, "shared v2 storage projection");
    assert_changes(
        &held_rates,
        &explained.expected_holds(),
        "refused replacements",
    );
}

#[derive(Deserialize)]
struct Witness {
    normalized: Facts,
    served: BTreeMap<String, Value>,
}

fn witnesses() -> BTreeMap<String, Witness> {
    let rows: BTreeMap<String, Witness> =
        serde_json::from_str(include_str!("models_dev_v2_examples.json")).unwrap();
    assert_eq!(
        rows.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        NAMED_WITNESSES.iter().copied().collect()
    );
    rows
}

fn v2_fact_admitted(key: &str) -> bool {
    if matches!(
        key,
        "rate.input_audio.per_second" | "rate.input.per_character"
    ) {
        return true;
    }
    let base = if let Some((base, threshold)) = key.split_once(".above_context.") {
        if !base.starts_with("rate.")
            || !threshold
                .parse::<u64>()
                .is_ok_and(|n| n > 0 && n.to_string() == threshold)
        {
            return false;
        }
        base
    } else if let Some((base, name)) = key.split_once(".mode.") {
        if !base.starts_with("rate.") || !matches!(name, "fast" | "flex" | "priority" | "ultrafast")
        {
            return false;
        }
        base
    } else {
        key
    };
    base != "model.kind"
        && SERVED_FACTS
            .iter()
            .any(|fact| served_fact_key_matches(fact.key, base))
}

#[test]
fn models_dev_v2_only_witnesses_and_unit_vocabulary() {
    let old = fixture();
    let v2 = parse_v2(&saved_gzip("api-v2-20261125.json.gz"));
    let mut expected = witnesses();
    let mut new_models = 0;
    let mut unit_witnesses = BTreeSet::new();
    for model in v2.catalog.models() {
        let provider = &model.key.provider_id;
        let id = &model.key.model_id;
        let only_in_v2 = old
            .providers
            .get(provider)
            .is_none_or(|p| !p.models.contains_key(id));
        let (facts, conflicts) = projected(model);
        if only_in_v2 {
            new_models += 1;
            for key in facts.keys() {
                assert!(
                    v2_fact_admitted(key),
                    "{provider}/{id} projected unadmitted {key}"
                );
            }
        }
        if let Some(witness) = expected.remove(&format!("{provider}/{id}")) {
            assert!(only_in_v2, "{provider}/{id}");
            assert_eq!(conflicts.count(), 0, "{provider}/{id}");
            assert_eq!(facts, witness.normalized, "{provider}/{id}");
        }
        // The cost-unit table admits a token rate, a character rate and two
        // explicit audio-input denominators. The other units have no served key.
        if only_in_v2 {
            for key in [
                "rate.output",
                "rate.input.per_character",
                "rate.input_audio.per_second",
            ] {
                if let Some(bytes) = facts.get(key) {
                    let rate: Value = serde_json::from_str(bytes).unwrap();
                    if rate["state"] == "priced" {
                        unit_witnesses.insert((key, rate["per"].as_u64()));
                    }
                }
            }
        }
    }
    assert_eq!(new_models, 861);
    assert!(
        expected.is_empty(),
        "missing named v2 witnesses: {:?}",
        expected.keys()
    );
    assert_eq!(
        unit_witnesses,
        BTreeSet::from([
            ("rate.output", None),
            ("rate.input.per_character", Some(1_000_000)),
            ("rate.input_audio.per_second", Some(1)),
            ("rate.input_audio.per_second", Some(60)),
        ])
    );
}

#[derive(Deserialize)]
struct DocumentProvider {
    models: BTreeMap<String, IgnoredAny>,
}

fn published_identities(saved_v2: &[u8]) -> BTreeSet<Identity> {
    // Read the upstream object keys independently of the normalizer so a
    // dropped or replaced model cannot change the expected arrival set.
    let providers: BTreeMap<String, DocumentProvider> = serde_json::from_slice(saved_v2).unwrap();
    providers
        .into_iter()
        .flat_map(|(p, rows)| rows.models.into_keys().map(move |m| (p.clone(), m)))
        .collect()
}

fn recorded(store: &CatalogStore, query: &CatalogQuery) -> RecordedModels {
    store
        .read_catalog(query)
        .unwrap()
        .models
        .into_iter()
        .map(|model| {
            (
                (model.provider_id, model.model_id),
                model
                    .facts
                    .into_iter()
                    .map(|(key, value)| (key.as_str().to_owned(), value))
                    .collect(),
            )
        })
        .collect()
}

fn with_existence(mut facts: Facts, existence: Existence) -> Facts {
    assert!(facts
        .insert(
            FactKey::existence().as_str().to_owned(),
            existence.as_json().to_owned()
        )
        .is_none());
    facts
}

fn verify_wire(
    before: &ServedModels,
    after: &ServedModels,
    shared: &BTreeSet<Identity>,
    normalized_changes: &Changes,
    expected: &Changes,
) {
    let mut changes = Changes::new();
    let mut newly_explicit = BTreeMap::new();
    for (provider, model) in shared {
        let id = format!("{provider}/{model}");
        let before = &before[&id];
        let after = &after[&id];
        for key in ["wire.api", "wire.npm", "wire.shape"] {
            for facts in [before, after] {
                if let Some(claim) = facts.get(key) {
                    assert!(claim["from"].is_string(), "{id} {key} lost claim origin");
                }
            }
        }
        for key in ["wire.api", "wire.family"] {
            let value = |facts: &BTreeMap<String, Value>| {
                facts
                    .get(key)
                    .map(|v| if key == "wire.api" { &v["value"] } else { v })
                    .map(Value::to_string)
            };
            let (old, new) = (value(before), value(after));
            if old == new {
                continue;
            }
            if key == "wire.api" && old.is_none() {
                *newly_explicit.entry(provider).or_insert(0) += 1;
            }
            changes.insert(
                (provider.clone(), model.clone(), key.to_owned()),
                (old, new),
            );
        }
        if normalized_changes
            .get(&(provider.clone(), model.clone(), "wire.shape".to_owned()))
            .is_some_and(|(_, new)| new.is_none())
        {
            assert_eq!(
                before.get("wire.family"),
                after.get("wire.family"),
                "{id} lost its family with its shape"
            );
        }
    }
    eprintln!("new served endpoints by provider: {newly_explicit:?}");
    assert_changes(&changes, expected, "catalog.get wire values and families");
}

fn verify_curated_default_endpoints(v2: &NormalizedCatalog) {
    for provider in ["openai", "anthropic", "google", "xai"] {
        let correction =
            &crate::wire_corrections::corrections()[&(provider.to_owned(), "*".to_owned())];
        let default_endpoint = correction.value.as_ref().unwrap();
        let rows = v2
            .providers
            .iter()
            .find(|row| row.provider_id == provider)
            .unwrap();
        assert!(!rows.models.is_empty());
        for m in &rows.models {
            let (facts, _) = projected(m);
            let api: Value = serde_json::from_str(&facts["wire.api"]).unwrap();
            assert_eq!(
                api["value"],
                default_endpoint.as_str(),
                "{provider}/{} disagrees with its curated provider endpoint",
                m.key.model_id
            );
        }
    }
}

#[test]
fn models_dev_v2_guarded_switch_keeps_history_and_served_wire_continuity() {
    let old = fixture();
    let bytes = saved_gzip("api-v2-20261125.json.gz");
    let v2_ids = published_identities(&bytes);
    let v2 = parse_v2(&bytes);
    let explained = explained();
    let changes = explained.expect(&explained.normalized);
    let (store, _dir) = seed(&old);
    let mut old_models = RecordedModels::new();
    for (provider_id, provider) in old.providers {
        for (model, facts) in provider.models {
            old_models.insert((provider_id.clone(), model), facts);
        }
    }
    let v1_ids: BTreeSet<_> = old_models.keys().cloned().collect();
    let shared: BTreeSet<_> = v1_ids.intersection(&v2_ids).cloned().collect();
    let arrivals: BTreeSet<_> = v2_ids.difference(&v1_ids).cloned().collect();
    let retirements: BTreeSet<_> = v1_ids.difference(&v2_ids).cloned().collect();
    assert_eq!(
        (v1_ids.len(), v2_ids.len(), shared.len(), arrivals.len()),
        (10_161, 10_913, 10_052, 861)
    );
    let mut retired_by_provider = BTreeMap::new();
    for (provider, _) in &retirements {
        *retired_by_provider.entry(provider.as_str()).or_insert(0) += 1;
    }
    assert_eq!(
        retired_by_provider,
        BTreeMap::from([
            ("azure-cognitive-services", 91),
            ("google-vertex-anthropic", 18)
        ])
    );

    let prefixes = ["wire.", "existence"];
    let before = served(&store, &prefixes);
    let signals = Signals::default();
    let tick = apply_v2(&store, &signals, bytes.clone(), SWITCH_AT);
    assert!(
        matches!(tick.outcome, TickOutcome::Changed { .. }),
        "{tick:?}"
    );
    assert_eq!(
        tick.composition,
        EraComposition {
            models_arrived: arrivals.len(),
            models_withdrawn: retirements.len(),
            facts_changed: changes.len(),
        }
    );
    assert_eq!(tick.eras_written, 48_130);
    let after = served(&store, &prefixes);
    verify_wire(
        &before,
        &after,
        &shared,
        &changes,
        &explained.expect(&explained.served_wire),
    );
    verify_curated_default_endpoints(&v2.catalog);

    for (id, witness) in witnesses() {
        let (provider, model) = id.split_once('/').unwrap();
        assert_eq!(
            served_model(&store, provider, model),
            witness.served,
            "{id}"
        );
    }

    let current = recorded(&store, &CatalogQuery::current(SOURCE));
    let current_ids: BTreeSet<_> = current.keys().cloned().collect();
    assert_eq!(current_ids, v2_ids);
    assert_eq!(
        current_ids
            .difference(&v1_ids)
            .cloned()
            .collect::<BTreeSet<_>>(),
        arrivals
    );
    let archived = recorded(&store, &CatalogQuery::current(SOURCE).including_retired());
    let past = recorded(&store, &CatalogQuery::at(SOURCE, SEED_AT));
    for identity in retirements {
        let old = old_models.remove(&identity).unwrap();
        assert_eq!(
            past[&identity],
            with_existence(old.clone(), Existence::Present),
            "history of {identity:?}"
        );
        assert_eq!(
            archived[&identity],
            with_existence(old, Existence::Absent),
            "retired {identity:?}"
        );
        let history = store
            .fact_history(SOURCE, &identity.0, &identity.1, &FactKey::existence())
            .unwrap();
        assert_eq!(
            history
                .iter()
                .map(|era| (era.boundary_at, era.value_json.as_str()))
                .collect::<Vec<_>>(),
            [
                (SEED_AT, Existence::Present.as_json()),
                (SWITCH_AT, Existence::Absent.as_json())
            ],
            "existence history of {identity:?}"
        );
    }

    for (provider, model, fact) in explained.expected_holds().keys() {
        let key = FactKey::from_stored(fact.clone());
        let history = store.fact_history(SOURCE, provider, model, &key).unwrap();
        assert_eq!(history.len(), 1, "{provider}/{model} {fact} was not held");
        assert_eq!(history[0].boundary_at, SEED_AT, "{provider}/{model} {fact}");
    }
    // Ingest records withdrawals as values rather than deleting a fact era.
    // An unchanged or refused rate is left exactly as it was in the fixture.
    for ((p, m, fact), (old, new)) in changes {
        let facts = old_models.get_mut(&(p, m)).unwrap();
        let next = new.unwrap_or_else(|| {
            if fact.starts_with("rate.") {
                r#"{"state":"unpriced","reason":"missing_rate"}"#.to_owned()
            } else {
                "null".to_owned()
            }
        });
        assert_eq!(facts.insert(fact, next), old);
    }
    for (id, expected) in old_models {
        assert_eq!(
            current[&id],
            with_existence(expected, Existence::Present),
            "stored facts of {id:?}"
        );
    }

    let era_count = store.era_count(SOURCE).unwrap();
    let repeat = apply_v2(&store, &signals, bytes, Timestamp(3_000));
    assert_eq!(repeat.outcome, TickOutcome::Unchanged);
    assert_eq!(repeat.eras_written, 0);
    assert_eq!(store.era_count(SOURCE).unwrap(), era_count);
}
