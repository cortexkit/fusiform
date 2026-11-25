//! Capture the v1 storage projection for comparison after the parser is retired.
//!
//! To reproduce the fixture, check out its `generated_from_commit` and run from
//! the workspace root on Linux:
//! `UPDATE_V1_FIXTURE=1 cargo test --locked -p fusiform-module --lib normalization::v1_fixture::models_dev_v1_fixture -- --exact`

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use fusiform_core::normalize::normalize_legacy_models_dev_for_tests_with;
use fusiform_core::upstream_facts::facts_of;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SOURCE_SHA256: &str = "b666fc36523df75efce7d815b4b97efd18fff14d07a0c72eafaebf221bbd6382";

#[derive(Serialize)]
struct Fixture {
    conflicts: Vec<Conflict>,
    fixture_format: u32,
    generated_from_commit: String,
    providers: BTreeMap<String, Provider>,
    source_sha256: String,
}

#[derive(Serialize)]
struct Provider {
    api_base_quarantined: bool,
    // Membership records presence. Each fact value is the original JSON text,
    // not a nested JSON value whose encoding could change on reserialization.
    models: BTreeMap<String, BTreeMap<String, String>>,
    npm_quarantined: bool,
    provider_name: Option<String>,
}

#[derive(Serialize)]
struct Conflict {
    fact: String,
    model: String,
    provider: String,
    values: Vec<String>,
}

#[derive(Deserialize)]
struct Provenance {
    generated_from_commit: String,
}

fn fixture(source: &[u8], generated_from_commit: String) -> Fixture {
    let source_sha256 = format!("{:x}", Sha256::digest(source));
    assert_eq!(
        source_sha256, SOURCE_SHA256,
        "unexpected v1 source snapshot"
    );
    let catalog =
        normalize_legacy_models_dev_for_tests_with(source, &crate::input_audio::evidence())
            .expect("normalize the saved v1 document with production evidence")
            .catalog;

    let mut providers = BTreeMap::new();
    let mut conflicts = Vec::new();
    for provider in catalog.providers {
        // v1 repeats the provider's normalized name on every model. The saved
        // snapshot has no empty providers; don't silently lose a name if one
        // ever reaches this projection.
        let provider_name = provider
            .models
            .first()
            .expect("saved v1 provider has at least one model")
            .provider_name
            .clone();
        let mut models = BTreeMap::new();
        for model in provider.models {
            let projected = facts_of(&model);
            models.insert(
                model.key.model_id,
                projected
                    .values
                    .into_iter()
                    .map(|(fact, json)| (fact.as_str().to_owned(), json))
                    .collect(),
            );
            conflicts.extend(projected.conflicts.into_iter().map(|conflict| Conflict {
                fact: conflict.fact.as_str().to_owned(),
                model: conflict.model,
                provider: conflict.provider,
                values: conflict.values,
            }));
        }
        providers.insert(
            provider.provider_id,
            Provider {
                api_base_quarantined: provider.api_base_quarantined,
                models,
                npm_quarantined: provider.npm_quarantined,
                provider_name,
            },
        );
    }
    conflicts.sort_unstable_by(|a, b| {
        (&a.provider, &a.model, &a.fact).cmp(&(&b.provider, &b.model, &b.fact))
    });
    Fixture {
        conflicts,
        fixture_format: 1,
        generated_from_commit,
        providers,
        source_sha256,
    }
}

fn stdout(command: &mut Command) -> Vec<u8> {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("run {command:?}: {error}"));
    assert!(
        output.status.success(),
        "{command:?}: {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn models_dev_v1_fixture() {
    let module = Path::new(env!("CARGO_MANIFEST_DIR"));
    let data = module.join("../fusiform-core/tests/data/models-dev-v2");
    let path = data.join("v1-fixture.json.gz");
    let source = stdout(
        Command::new("gzip")
            .arg("-dc")
            .arg(data.join("api-v1-20261125.json.gz")),
    );
    let update = std::env::var("UPDATE_V1_FIXTURE").as_deref() == Ok("1");
    let committed = (!update).then(|| stdout(Command::new("gzip").arg("-dc").arg(&path)));
    let commit = if let Some(committed) = &committed {
        serde_json::from_slice::<Provenance>(committed)
            .expect("read the generating commit")
            .generated_from_commit
    } else {
        // The recorded revision must identify the v1 code that actually ran.
        let dirty = stdout(Command::new("git").current_dir(module).args([
            "status",
            "--porcelain",
            "--untracked-files=no",
        ]));
        assert!(
            dirty.is_empty(),
            "commit tracked changes before generating the fixture"
        );
        String::from_utf8(stdout(Command::new("git").current_dir(module).args([
            "rev-parse",
            "--verify",
            "HEAD",
        ])))
        .expect("Git commit ID is ASCII")
        .trim_end()
        .to_owned()
    };
    assert!(
        commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "expected a full generating commit ID"
    );
    let json = serde_json::to_vec(&fixture(&source, commit)).expect("serialize v1 fixture");
    if let Some(committed) = committed {
        assert!(
            json == committed,
            "the normalized v1 catalog differs from the fixture"
        );
    } else {
        let temp = tempfile::NamedTempFile::new().expect("create temporary v1 JSON");
        fs::write(temp.path(), json).expect("write v1 JSON");
        let compressed = stdout(Command::new("gzip").arg("-9nc").arg(temp.path()));
        fs::write(path, compressed).expect("write compressed v1 fixture");
    }
}
