use std::{collections::BTreeMap, fs, path::PathBuf};

use fusiform_release::{
    Change, ChangeOp, CurationSet, Head, HeadBody, Release, ReleaseEnvelope, ReleaseFormat,
    SigningKey, Snapshot, State, Upstream, canonical_json, sha256, sign_envelope, sign_head,
    state_hash, upstream_state_hash,
};
use serde::Serialize;
use serde_json::json;

// Public, deterministic test key. Never trusted by a release reader.
const TEST_SIGNING_KEY_BYTES: [u8; 32] = [42; 32];
const TEST_KID: &str = "release-format-v1-test-key";

fn canonical_file(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = canonical_json(&serde_json::to_value(value).unwrap());
    bytes.push(b'\n');
    bytes
}

fn generate() -> BTreeMap<&'static str, Vec<u8>> {
    let canonical_input = r#" { "é": "\u96ea\/", "z": {"number": 1E7, "control": "\u0000\t\n"}, "a": [0.30000000000000004, 1e-7, -0] } "#;
    let canonical = canonical_json(&serde_json::from_str(canonical_input).unwrap());
    let vector = json!({
        "input": canonical_input,
        "canonical": str::from_utf8(&canonical).unwrap(),
        "sha256": sha256(&canonical),
    });

    let upstream: Upstream = serde_json::from_value(json!({
        "alpha": {
            "provider": {"provider.name": "Fixture Labs"},
            "models": {"-": {"existence": "present", "rate.input": {
                "state": "priced", "currency": "USD", "units": 300000000, "exponent": -9
            }}}
        },
        "zeta": {"provider": {}, "models": {"mini": {"existence": "present", "limit.context": 8192}}}
    })).unwrap();
    let upstream_hash = upstream_state_hash(&upstream);
    let curation: CurationSet = serde_json::from_value(json!({
        "version": 1, "approved_at_ms": null, "approved_drafts": [],
        "grades": {}, "effort_grades": {}, "kind_grades": {}, "workload_efforts": {},
        "decommissioned": {}, "vendor_facts": {}
    }))
    .unwrap();
    let curation_hash = curation.curation_hash();
    let state_hash = state_hash(curation_hash, upstream_hash);

    let change = Change {
        upstream: vec![
            ChangeOp::SetProvider {
                provider: "alpha".into(),
                fact: "provider.name".into(),
                value: json!("Fixture Labs"),
            },
            ChangeOp::RemoveProvider {
                provider: "alpha".into(),
                fact: "provider.old_label".into(),
            },
            ChangeOp::Set {
                provider: "alpha".into(),
                model: "-".into(),
                fact: "rate.input".into(),
                value: upstream["alpha"].models["-"]["rate.input"].clone(),
            },
            ChangeOp::Remove {
                provider: "zeta".into(),
                model: "mini".into(),
                fact: "rate.output".into(),
            },
        ],
        curation_set: None,
    };
    assert!(
        change
            .upstream
            .windows(2)
            .all(|pair| pair[0].key() < pair[1].key())
    );
    let envelope = ReleaseEnvelope {
        format: ReleaseFormat::V1,
        release: 2,
        parent: Some(1),
        published_at_ms: 1795000000000,
        upstream_observed_at_ms: 1794999990000,
        upstream_hash,
        curation_version: curation.version,
        curation_hash,
        state_hash,
        change_hash: change.change_hash(),
    };
    let test_key = SigningKey::from_bytes(&TEST_SIGNING_KEY_BYTES);
    let signatures = sign_envelope(&envelope, TEST_KID, &test_key).unwrap();
    let snapshot = Snapshot {
        envelope: envelope.clone(),
        state: State {
            upstream,
            curation_set: curation,
        },
        signatures: signatures.clone(),
    };
    let release = Release {
        envelope,
        change,
        signatures,
    };
    let head = HeadBody {
        release: release.envelope.release.try_into().unwrap(),
        published_at_ms: release.envelope.published_at_ms,
        state_hash,
    };
    let head = Head {
        signatures: sign_head(&head, TEST_KID, &test_key),
        head,
    };
    BTreeMap::from([
        ("canonical.json", canonical_file(&vector)),
        ("change.json", canonical_file(&release)),
        ("head.json", canonical_file(&head)),
        ("snapshot.json", canonical_file(&snapshot)),
    ])
}

#[test]
fn release_format_vectors_and_checksums_do_not_drift() {
    let mut files = generate();
    let sums = files
        .iter()
        .map(|(name, bytes)| format!("{}  {name}\n", sha256(bytes)))
        .collect::<String>();
    files.insert("SHA256SUMS", sums.into_bytes());

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/release-format-v1");
    if std::env::var("UPDATE_RELEASE_FORMAT_FIXTURES").as_deref() == Ok("1") {
        fs::create_dir_all(&root).unwrap();
        for (name, bytes) in files {
            fs::write(root.join(name), bytes).unwrap();
        }
        eprintln!("regenerated canonical JSON, signed release, head and snapshot, and SHA256SUMS");
        return;
    }

    for (name, expected) in files {
        let path = root.join(name);
        let actual = fs::read(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        assert_eq!(actual, expected, "release-format v1 fixture drift: {name}");
    }
}
