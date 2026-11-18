//! Prepare seed inputs, then measure each poll in a separate process.
//!
//!     measure_seed prepare <work-dir>  # includes snapshot.json for the WASM measurement
//!     /usr/bin/time -v measure_seed rebuild <work-dir>
//!     /usr/bin/time -v measure_seed rebuild-stream <work-dir>
//!     /usr/bin/time -v measure_seed one <work-dir>
//!     /usr/bin/time -v measure_seed large <work-dir>

use std::{fs, io::BufWriter, path::Path, time::Instant};

use fusiform_core::{
    ChargeUnitEvidence, normalize_models_dev_with,
    upstream_facts::{Existence, FactKey, facts_of},
};
use fusiform_curate::input_audio::{
    add_output_evidence, evidence_from, load_for_schema, load_reporting_rejects,
};
use fusiform_publish::{FactIndex, publish_step};
use fusiform_release::{
    ProviderFacts, SigningKey, Snapshot, Upstream, sign_envelope, state_hash, upstream_state_hash,
};
use serde_json::{Value, json};

const SEED: &[u8] = include_bytes!("../../fusiform-module/data/models-dev-seed.json");
const INPUT_AUDIO: &str = include_str!("../../fusiform-module/data/input-audio-per-token.json");
const OUTPUT_AUDIO: &str = include_str!("../../fusiform-module/data/output-audio-per-token.json");

fn evidence() -> ChargeUnitEvidence {
    let (input, input_rejects) = load_reporting_rejects(INPUT_AUDIO);
    let (output, output_rejects) = load_for_schema(OUTPUT_AUDIO, "output-audio-per-token/v1");
    assert!(input_rejects.is_empty() && output_rejects.is_empty());
    add_output_evidence(evidence_from(&input), &output)
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(args.len(), 3, "expected a mode and work directory");
    let path = Path::new(&args[2]);
    match args[1].as_str() {
        "audit" => {
            if args[2] == "embedded" {
                audit_tiers(SEED);
            } else {
                audit_tiers(&fs::read(path).unwrap());
            }
        }
        "prepare" => prepare(path),
        mode @ ("rebuild" | "rebuild-stream") => {
            let start = Instant::now();
            let index = if mode == "rebuild-stream" {
                FactIndex::from_snapshot_upstream_reader(
                    fs::File::open(path.join("upstream.json")).unwrap(),
                )
                .unwrap()
            } else {
                rebuild(path)
            };
            println!(
                "rebuild_ms={:.1} models={} facts={} upstream_hash={}",
                start.elapsed().as_secs_f64() * 1000.0,
                index.model_count(),
                index.fact_count(),
                index.upstream_hash()
            );
        }
        mode @ ("one" | "large") => {
            let previous = FactIndex::from_bytes(&fs::read(path.join("index")).unwrap()).unwrap();
            let edited = fs::read(path.join(format!("{mode}.json"))).unwrap();
            measure(
                &edited,
                &previous,
                &evidence(),
                if mode == "one" { 1 } else { 11_403 },
            );
        }
        _ => panic!("expected prepare, rebuild, rebuild-stream, one, large, or audit"),
    }
}

fn prepare(path: &Path) {
    fs::create_dir_all(path).unwrap();
    let catalog = normalize_models_dev_with(SEED, &evidence())
        .unwrap()
        .catalog;
    let model_count = catalog.model_count();
    let upstream: Upstream = catalog
        .providers
        .into_iter()
        .filter(|p| !p.models.is_empty())
        .map(|provider| {
            let models = provider
                .models
                .into_iter()
                .map(|model| {
                    let mut facts: fusiform_release::Facts = facts_of(&model)
                        .values
                        .into_iter()
                        .map(|(key, raw)| {
                            (key.as_str().to_owned(), serde_json::from_str(&raw).unwrap())
                        })
                        .collect();
                    facts.insert(
                        FactKey::existence().as_str().to_owned(),
                        serde_json::from_str(Existence::Present.as_json()).unwrap(),
                    );
                    (model.key.model_id, facts)
                })
                .collect();
            (
                provider.provider_id,
                ProviderFacts {
                    provider: Default::default(),
                    models,
                },
            )
        })
        .collect();
    write_json(path, "upstream.json", &upstream);
    write_snapshot(path, upstream);
    println!(
        "snapshot_upstream_bytes={}",
        fs::metadata(path.join("upstream.json")).unwrap().len()
    );

    let bytes = rebuild(path).to_bytes();
    fs::write(path.join("index"), &bytes).unwrap();
    println!("serialized_index_bytes={}", bytes.len());

    let mut edited: Value = serde_json::from_slice(SEED).unwrap();
    let label = &mut edited["openai"]["models"]["gpt-realtime-2.1"]["name"];
    assert!(label.is_string());
    *label = json!("Changed display label");
    write_json(path, "one.json", &edited);

    let mut context_changes = 11_403 - model_count;
    for provider in edited.as_object_mut().unwrap().values_mut() {
        for model in provider["models"].as_object_mut().unwrap().values_mut() {
            model["name"] = json!("Changed display label for large diff");
            if context_changes > 0 && model["limit"]["context"].as_u64().is_some() {
                model["limit"]["context"] = json!(1_234_567);
                context_changes -= 1;
            }
        }
    }
    assert_eq!(context_changes, 0);
    write_json(path, "large.json", &edited);
}

fn write_snapshot(path: &Path, upstream: Upstream) {
    let mut snapshot: Snapshot = serde_json::from_slice(include_bytes!(
        "../../fusiform-release/fixtures/release-format-v1/snapshot.json"
    ))
    .unwrap();
    snapshot.state.upstream = upstream;
    let curation = &mut snapshot.state.curation_set;
    for (name, doc) in [
        ("input-audio-per-token", INPUT_AUDIO),
        ("output-audio-per-token", OUTPUT_AUDIO),
    ] {
        curation
            .vendor_facts
            .insert(name.to_owned(), serde_json::from_str(doc).unwrap());
    }
    let envelope = &mut snapshot.envelope;
    envelope.upstream_hash = upstream_state_hash(&snapshot.state.upstream);
    envelope.curation_hash = curation.curation_hash();
    envelope.state_hash = state_hash(envelope.curation_hash, envelope.upstream_hash);
    // The release-format test key signs the generated benchmark snapshot only.
    snapshot.signatures = sign_envelope(
        envelope,
        "release-format-v1-test-key",
        &SigningKey::from_bytes(&[42; 32]),
    )
    .unwrap();
    write_json(path, "snapshot-envelope.json", &snapshot.envelope);
    write_json(path, "snapshot-curation.json", &snapshot.state.curation_set);
    write_json(path, "snapshot-signatures.json", &snapshot.signatures);
    write_json(path, "snapshot.json", &snapshot);
}

fn audit_tiers(bytes: &[u8]) {
    use fusiform_core::{ChargeBasis, RateCondition};
    let catalog = normalize_models_dev_with(bytes, &evidence())
        .unwrap()
        .catalog;
    let mut repeated_keys = 0;
    let mut conflicts = 0;
    for model in catalog.models() {
        let mut seen = std::collections::BTreeSet::new();
        let mut repeats = std::collections::BTreeSet::new();
        for rate in &model.rates {
            if let (
                ChargeBasis::PerMillionTokens { class },
                RateCondition::MinContextTokens { tokens },
            ) = (&rate.basis, &rate.condition)
            {
                let fact = FactKey::rate_above_context(*class, *tokens);
                if !seen.insert(fact.clone()) {
                    repeats.insert(fact);
                }
            }
        }
        repeated_keys += repeats.len();
        conflicts += facts_of(model).conflicts.len();
    }
    println!(
        "models={} repeated_tier_rate_keys={repeated_keys} conflicting_fact_keys={conflicts}",
        catalog.model_count()
    );
}

fn write_json(path: &Path, name: &str, data: &impl serde::Serialize) {
    use std::io::Write as _;
    let mut out = BufWriter::new(fs::File::create(path.join(name)).unwrap());
    serde_json::to_writer(&mut out, data).unwrap();
    out.flush().unwrap();
}

fn rebuild(path: &Path) -> FactIndex {
    let upstream: Upstream =
        serde_json::from_slice(&fs::read(path.join("upstream.json")).unwrap()).unwrap();
    FactIndex::from_snapshot_upstream(&upstream).unwrap()
}

fn measure(bytes: &[u8], previous: &FactIndex, evidence: &ChargeUnitEvidence, expected_ops: usize) {
    let start = Instant::now();
    let result = publish_step(bytes, previous, evidence).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(result.ops.len(), expected_ops);
    println!(
        "publish_step_ms={:.1} models={} facts={} ops={} upstream_hash={}",
        elapsed.as_secs_f64() * 1000.0,
        result.model_count,
        result.index.fact_count(),
        result.ops.len(),
        result.upstream_hash
    );
}
