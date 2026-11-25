#[path = "consumer.rs"]
mod consumer;

use std::collections::BTreeMap;

use fusiform_core::ChargeUnitEvidence;
use fusiform_publish::{FactIndex, SnapshotInputs, publish_step, snapshot_bytes};
use fusiform_release::{
    HistoryAttestationBody, HistoryFile, Release, SignatureError, SigningKey, Snapshot,
    build_release, canonical_json, history_day_bytes, sha256, sign_envelope,
    sign_history_attestation, verify_envelope, verify_history_attestation,
};
use serde_json::{Value, json};

const DAY: &str = "2026-11-19";
const START_MS: i64 = 1795046400000;
const MODELS: &[u8] = include_bytes!("../../../fusiform-publish-wasm/fixtures/models-dev.json");
const CURATION: &str = include_str!("../../../fusiform-publish-wasm/fixtures/curation.json");
const KID: &str = "release-format-v1-test-key";
const PUBLIC_KEY: &[u8] = br#"{"kid":"release-format-v1-test-key","public_key":"GX9rI-FshTLGq8g4-s1ep4m-DHaykgM0A5v6iz02jWE"}"#;

fn parse(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

fn wire(value: impl serde::Serialize) -> Vec<u8> {
    canonical_json(&serde_json::to_value(value).unwrap())
}

fn key() -> SigningKey {
    SigningKey::from_bytes(&[42; 32])
}

fn signed(unsigned: &[u8], key: &SigningKey) -> Release {
    let mut object = parse(unsigned);
    let envelope = serde_json::from_value(object["envelope"].clone()).unwrap();
    object["signatures"] = json!(sign_envelope(&envelope, KID, key).unwrap());
    serde_json::from_value(object).unwrap()
}

fn models_with_rename() -> Vec<u8> {
    let mut models = parse(MODELS);
    models["zeta"]["models"]["mini"]["name"] = json!("Mini 2");
    wire(models)
}

fn summary(models: &[u8], previous: &FactIndex) -> (Vec<u8>, FactIndex) {
    let step = publish_step(models, previous, &ChargeUnitEvidence::default()).unwrap();
    (
        wire(json!({
            "base_upstream_hash": previous.upstream_hash(),
            "ops": step.ops,
            "upstream_hash": step.upstream_hash,
        })),
        step.index,
    )
}

fn two_releases() -> (Release, Release, FactIndex) {
    let index = FactIndex::from_snapshot_upstream(&BTreeMap::new()).unwrap();
    let (genesis, index) = summary(MODELS, &index);
    let r1 = build_release(
        &genesis,
        CURATION.as_bytes(),
        b"",
        1,
        START_MS - 1,
        START_MS - 2,
    )
    .unwrap();
    let r1 = signed(&r1, &key());
    let (changed, index) = summary(&models_with_rename(), &index);
    let r2 = build_release(
        &changed,
        CURATION.as_bytes(),
        &wire(&r1.envelope),
        2,
        START_MS,
        START_MS - 1,
    )
    .unwrap();
    (r1, signed(&r2, &key()), index)
}

fn third_release(r2: &Release, index: &FactIndex) -> Release {
    let (unchanged, _) = summary(&models_with_rename(), index);
    let curation = CURATION.replace("\"version\":1", "\"version\":2");
    let r3 = build_release(
        &unchanged,
        curation.as_bytes(),
        &wire(&r2.envelope),
        3,
        START_MS + 86_399_999,
        START_MS + 86_399_998,
    )
    .unwrap();
    signed(&r3, &key())
}

fn jsonl(releases: &[&Release]) -> Vec<u8> {
    let mut lines = Vec::new();
    for release in releases {
        lines.extend(wire(release));
        lines.push(b'\n');
    }
    lines
}

struct SnapshotDocs {
    envelope: Vec<u8>,
    signatures: Vec<u8>,
    history: Vec<u8>,
    history_signatures: Vec<u8>,
}

impl SnapshotDocs {
    fn new(release: &Release, history: Vec<HistoryFile>) -> Self {
        let body = HistoryAttestationBody {
            release: release.envelope.release.try_into().unwrap(),
            state_hash: release.envelope.state_hash,
            history,
        };
        Self {
            envelope: wire(&release.envelope),
            signatures: wire(&release.signatures),
            history: wire(&body.history),
            history_signatures: wire(sign_history_attestation(&body, KID, &key())),
        }
    }

    fn inputs<'a>(
        &'a self,
        models_dev: &'a [u8],
        evidence: &'a ChargeUnitEvidence,
    ) -> SnapshotInputs<'a> {
        SnapshotInputs {
            models_dev,
            evidence,
            envelope: &self.envelope,
            curation_set_raw: CURATION.as_bytes(),
            signatures: &self.signatures,
            history_files: &self.history,
            history_signatures: &self.history_signatures,
            release_public_key: PUBLIC_KEY,
        }
    }
}

pub fn fixtures() -> BTreeMap<&'static str, Vec<u8>> {
    let (r1, r2, _) = two_releases();
    let base = history_base(&r1);
    let day = history_day_bytes(DAY, &jsonl(&[&r2])).unwrap();
    let docs = SnapshotDocs::new(
        &r2,
        vec![
            HistoryFile {
                file: "base.json".into(),
                sha256: sha256(&base),
            },
            HistoryFile {
                file: format!("days/{DAY}.json"),
                sha256: sha256(&day),
            },
        ],
    );
    let snapshot =
        snapshot_bytes(docs.inputs(&models_with_rename(), &ChargeUnitEvidence::default())).unwrap();
    BTreeMap::from([
        ("chain/base.json", base),
        ("chain/release-1.json", wire(&r1)),
        ("chain/release-2.json", wire(&r2)),
        ("chain/snapshot.json", snapshot),
        ("chain/days/2026-11-19.json", day),
    ])
}

fn history_base(import: &Release) -> Vec<u8> {
    use fusiform_release::ChangeOp;

    let at = import.envelope.upstream_observed_at_ms;
    let era: Vec<_> = import
        .change
        .upstream
        .iter()
        .enumerate()
        .map(|(i, op)| {
            let ChangeOp::Set {
                provider,
                model,
                fact,
                value,
            } = op
            else {
                panic!("the fixture starts from an empty upstream with only model-scoped facts");
            };
            json!({"id": i + 1, "source": "models.dev", "provider_id": provider, "model_id": model,
            "fact_key": fact, "value_json": str::from_utf8(&wire(value)).unwrap(),
            "boundary_at_ms": at, "boundary_kind": "seed", "prior_observation_at_ms": null,
            "observation_id": null, "corrected_fields_json": null, "affected_from_ms": null,
            "affected_until_ms": null, "correction_reason": null})
        })
        .collect();
    let grade = &import.change.curation_set.as_ref().unwrap().grades["mini"];
    wire(
        json!({"format": "ck-models-history-base/v1", "release": 1, "tables": {
            "cache_lifetime_era": [], "curated_model_era": [], "era": era, "fact_override_era": [],
            "model_mark_era": [], "observation": [{"id": 1, "source": "models.dev", "observed_at_ms": at,
                "outcome": "seeded", "failure_class": null, "detail": null, "normalized_hash": null,
                "raw_hash": null, "etag": null, "duration_ms": null}],
            "observation_artifact_event": [], "plan_price_era": [],
            "quality_grade_era": [{"id": 1, "model": "mini", "elo": grade.elo.to_string(),
                "eq": grade.eq.to_string(), "withdrawn": 0, "graded_by": "operator",
                "reason": "Fixture grade", "recorded_at_ms": at, "grade": null, "kind": null}]
        }}),
    )
}

#[test]
fn signed_chain_replays_and_snapshot_reproduces_state() {
    let (r1, r2, index) = two_releases();
    let r3 = third_release(&r2, &index);
    assert!(r1.change.curation_set.is_some());
    assert!(r2.change.curation_set.is_none());
    assert!(!r2.change.upstream.is_empty());
    assert!(r3.change.curation_set.is_some());
    assert!(r3.change.upstream.is_empty());
    let keys = BTreeMap::from([(KID.to_string(), key().verifying_key())]);
    let chain = vec![r1, r2, r3];
    assert_eq!(consumer::check_chain(&chain, &keys).unwrap(), index);
    let mut tampered = chain.clone();
    tampered[2]
        .change
        .curation_set
        .as_mut()
        .unwrap()
        .grades
        .get_mut("mini")
        .unwrap()
        .elo = "1.0".parse().unwrap();
    assert_eq!(
        consumer::check_chain(&tampered, &keys).unwrap_err(),
        "change hash mismatch"
    );
    tampered[2].envelope.change_hash = tampered[2].change.change_hash();
    tampered[2].signatures = sign_envelope(&tampered[2].envelope, KID, &key()).unwrap();
    assert_eq!(
        consumer::check_chain(&tampered, &keys).unwrap_err(),
        "replayed state does not match the envelope"
    );

    let fixtures = fixtures();
    let bytes = &fixtures["chain/snapshot.json"];
    let snapshot: Snapshot = serde_json::from_slice(bytes).unwrap();
    assert_eq!(*bytes, wire(&snapshot));
    assert_eq!(snapshot.envelope, chain[1].envelope);
    assert_eq!(snapshot.signatures, chain[1].signatures);
    assert_eq!(
        FactIndex::from_snapshot_upstream(&snapshot.state.upstream).unwrap(),
        index
    );
    let attestation = snapshot.history_attestation.unwrap();
    verify_envelope(&snapshot.envelope, &snapshot.signatures, &keys).unwrap();
    verify_history_attestation(&attestation, &snapshot.envelope, &keys).unwrap();
    for file in &attestation.body.history {
        file.verify_bytes(&fixtures[format!("chain/{}", file.file).as_str()])
            .unwrap();
    }
    let day = history_day_bytes(DAY, &jsonl(&[&chain[1], &chain[2]])).unwrap();
    assert_eq!(day, wire(json!({"day": DAY, "releases": &chain[1..]})));
}

#[test]
fn release_builder_refuses_invalid_parent_and_noop() {
    let (r1, r2, index) = two_releases();
    let (summary, _) = summary(&models_with_rename(), &index);
    let build = |summary: &[u8], curation: &[u8], parent: &[u8], number| {
        build_release(summary, curation, parent, number, START_MS, START_MS)
            .unwrap_err()
            .class
    };
    assert_eq!(
        build(&summary, CURATION.as_bytes(), &wire(&r2.envelope), 3),
        "nothing_to_publish"
    );
    assert_eq!(
        build(&summary, CURATION.as_bytes(), &wire(&r1.envelope), 3),
        "invalid_parent"
    );
    assert_eq!(
        build(&summary, CURATION.as_bytes(), b"", 3),
        "invalid_parent"
    );
    assert_eq!(
        build(&summary, CURATION.as_bytes(), &wire(&r1.envelope), 1),
        "invalid_parent"
    );
    assert_eq!(
        build(&summary, CURATION.as_bytes(), b"null", 3),
        "invalid_parent"
    );
    assert_eq!(
        build(b"{\"refusal\":{}}", CURATION.as_bytes(), b"", 1),
        "invalid_summary"
    );
    assert_eq!(build(&summary, b"{}", b"", 1), "invalid_curation");
}

#[test]
fn curation_number_spelling_is_hashed_and_version_must_change() {
    let (r1, _, _) = two_releases();
    let prior = FactIndex::from_snapshot_upstream(&BTreeMap::new()).unwrap();
    let (genesis, index) = summary(MODELS, &prior);
    let (changed, _) = summary(&models_with_rename(), &index);
    let tampered = CURATION.replace("1.00", "1.0");
    let build = |raw: &str| {
        parse(&build_release(&genesis, raw.as_bytes(), b"", 1, START_MS, START_MS).unwrap())
    };
    let original = build(CURATION);
    let changed_spelling = build(&tampered);
    assert_ne!(
        original["envelope"]["curation_hash"],
        changed_spelling["envelope"]["curation_hash"]
    );
    assert_eq!(
        original["envelope"]["curation_hash"],
        json!(sha256(&wire(parse(CURATION.as_bytes()))))
    );
    let err = build_release(
        &changed,
        tampered.as_bytes(),
        &wire(&r1.envelope),
        2,
        START_MS,
        START_MS,
    )
    .unwrap_err();
    assert_eq!(err.class, "curation_version_reused");
    let mut unsorted = parse(&genesis);
    unsorted["ops"].as_array_mut().unwrap().reverse();
    let err = build_release(
        &wire(unsorted),
        CURATION.as_bytes(),
        b"",
        1,
        START_MS,
        START_MS,
    )
    .unwrap_err();
    assert_eq!(err.class, "invalid_summary");
}

#[test]
fn snapshot_builder_refuses_signed_invalid_history_lists() {
    let (_, r2, _) = two_releases();
    let models = models_with_rename();
    let evidence = ChargeUnitEvidence::default();
    for names in [
        &[][..],
        &["days/2026-11-19.json"][..],
        &["base.json", "days/2026-11-19.txt"][..],
        &["base.json", "days/2026-02-30.json"][..],
        &["base.json", "days/2026-11-20.json", "days/2026-11-19.json"][..],
        &["base.json", "days/2026-11-19.json", "days/2026-11-19.json"][..],
    ] {
        let history = names
            .iter()
            .map(|&file| HistoryFile {
                file: file.into(),
                sha256: sha256(file.as_bytes()),
            })
            .collect();
        let docs = SnapshotDocs::new(&r2, history);
        let result = snapshot_bytes(docs.inputs(&models, &evidence));
        assert_eq!(
            result.err().map(|err| err.class),
            Some("invalid_history_files"),
            "accepted signed history list {names:?}"
        );
    }
}

#[test]
fn history_day_builder_keeps_unknown_fields_in_the_canonical_bytes() {
    let (_, release, _) = two_releases();
    let mut stored = serde_json::to_value(release).unwrap();
    let extension: Value = serde_json::from_str(r#"{"z":[9007199254740993,1.00],"a":true}"#).unwrap();
    stored["extra"] = extension.clone();
    stored["envelope"]["extra"] = extension.clone();
    stored["change"]["extra"] = extension.clone();
    stored["change"]["upstream"][0]["extra"] = extension;
    let actual = history_day_bytes(DAY, &serde_json::to_vec(&stored).unwrap()).unwrap();
    assert_eq!(actual, wire(json!({"day": DAY, "releases": [stored]})));
}

#[test]
fn history_day_builder_refuses_a_change_that_does_not_match_its_envelope() {
    let (_, mut release, _) = two_releases();
    let fusiform_release::ChangeOp::Set { value, .. } = &mut release.change.upstream[0] else {
        panic!("the release must set the new model name");
    };
    *value = json!("Different name");
    let result = history_day_bytes(DAY, &jsonl(&[&release]));
    assert_eq!(
        result.err().map(|err| err.class),
        Some("history_change_mismatch"),
        "accepted a release whose change did not match its envelope"
    );
}

#[test]
fn snapshot_builder_refuses_mismatching_inputs() {
    let (_, r2, _) = two_releases();
    let models = models_with_rename();
    let evidence = ChargeUnitEvidence::default();
    let history_base = vec![HistoryFile {
        file: "base.json".into(),
        sha256: sha256(b"base"),
    }];
    let docs = SnapshotDocs::new(&r2, history_base.clone());
    let inputs = || docs.inputs(&models, &evidence);
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            models_dev: MODELS,
            ..inputs()
        })
        .unwrap_err()
        .class,
        "snapshot_hash_mismatch"
    );
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            models_dev: b"{",
            ..inputs()
        })
        .unwrap_err()
        .class,
        "parse"
    );
    let modified = CURATION.replace("1.00", "1.0");
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            curation_set_raw: modified.as_bytes(),
            ..inputs()
        })
        .unwrap_err()
        .class,
        "snapshot_curation_mismatch"
    );
    let modified = CURATION.replace("\"version\":1", "\"version\":2");
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            curation_set_raw: modified.as_bytes(),
            ..inputs()
        })
        .unwrap_err()
        .class,
        "snapshot_curation_mismatch"
    );
    let history = wire(vec![HistoryFile {
        file: "base.json".into(),
        sha256: sha256(b"different base"),
    }]);
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            history_files: &history,
            ..inputs()
        })
        .unwrap_err()
        .class,
        "invalid_history_signature"
    );
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            history_files: b"{}",
            ..inputs()
        })
        .unwrap_err()
        .class,
        "invalid_history_files"
    );
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            signatures: &docs.history_signatures,
            ..inputs()
        })
        .unwrap_err()
        .class,
        "invalid_release_signature"
    );
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            release_public_key: br#"{"kid":"a","public_key":"short"}"#,
            ..inputs()
        })
        .unwrap_err()
        .class,
        "invalid_release_public_key"
    );
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            envelope: b"{}",
            ..inputs()
        })
        .unwrap_err()
        .class,
        "invalid_envelope"
    );
    assert_eq!(
        snapshot_bytes(SnapshotInputs {
            curation_set_raw: b"{}",
            ..inputs()
        })
        .unwrap_err()
        .class,
        "invalid_curation"
    );
    let mut inconsistent = r2.clone();
    inconsistent.envelope.state_hash = sha256(b"not this state");
    inconsistent.signatures = sign_envelope(&inconsistent.envelope, KID, &key()).unwrap();
    let inconsistent = SnapshotDocs::new(&inconsistent, history_base);
    assert_eq!(
        snapshot_bytes(inconsistent.inputs(&models, &evidence))
            .unwrap_err()
            .class,
        "snapshot_state_mismatch"
    );
}

#[test]
fn history_attestation_refuses_tampering_and_cross_release_reuse() {
    let (r1, r2, _) = two_releases();
    let fixtures = fixtures();
    let snapshot: Snapshot = serde_json::from_slice(&fixtures["chain/snapshot.json"]).unwrap();
    let attestation = snapshot.history_attestation.unwrap();
    let keys = BTreeMap::from([(KID.to_string(), key().verifying_key())]);
    let mut tampered = attestation.clone();
    tampered.body.history.reverse();
    assert_eq!(
        verify_history_attestation(&tampered, &r2.envelope, &keys),
        Err(SignatureError::BadSignature)
    );
    assert_eq!(
        attestation.body.history[0]
            .verify_bytes(b"tampered base")
            .unwrap_err()
            .class,
        "history_file_hash_mismatch"
    );
    assert_eq!(
        verify_history_attestation(&attestation, &r1.envelope, &keys),
        Err(SignatureError::WrongHistoryAttestation)
    );
    let mut wrong_state = r2.envelope.clone();
    wrong_state.state_hash = sha256(b"a different state");
    assert_eq!(
        verify_history_attestation(&attestation, &wrong_state, &keys),
        Err(SignatureError::WrongHistoryAttestation)
    );
}

#[test]
fn day_builder_refuses_bad_batches_and_checks_utc_boundaries() {
    let (r1, r2, index) = two_releases();
    let r3 = third_release(&r2, &index);
    let mut gap = r3.clone();
    gap.envelope.release = 4;
    gap.envelope.parent = Some(3);
    let mut tomorrow = r3.clone();
    tomorrow.envelope.published_at_ms += 1;
    let mut yesterday = r2.clone();
    yesterday.envelope.published_at_ms -= 1;
    let class = |day, lines: &[u8]| history_day_bytes(day, lines).unwrap_err().class;
    assert_eq!(class(DAY, &jsonl(&[&r2, &gap])), "nonconsecutive_history");
    assert_eq!(class(DAY, &jsonl(&[&r3, &r2])), "nonconsecutive_history");
    assert_eq!(class(DAY, &jsonl(&[&r2, &r2])), "nonconsecutive_history");
    assert_eq!(
        class(DAY, &jsonl(&[&r2, &tomorrow])),
        "history_day_mismatch"
    );
    assert_eq!(class(DAY, &jsonl(&[&yesterday])), "history_day_mismatch");
    assert_eq!(
        class("2026-11-18", &jsonl(&[&r1])),
        "history_import_release"
    );
    assert_eq!(class("2026-02-29", &jsonl(&[&r2])), "invalid_day");
    assert_eq!(class("2026-1-19", &jsonl(&[&r2])), "invalid_day");
    assert_eq!(class(DAY, b""), "empty_history");
    assert_eq!(class(DAY, b"{}"), "invalid_history_release");
}
