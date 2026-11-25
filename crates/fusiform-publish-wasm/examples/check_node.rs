//! Native oracle for the Node integration test. Reads its observations on stdin.

use std::{collections::BTreeMap, io};

use fusiform_core::ChargeUnitEvidence;
use fusiform_publish::{FactIndex, publish_step};
use fusiform_release::{
    Head, Release, ReleaseEnvelope, SignatureError, Signatures, SigningKey, Snapshot,
    upstream_state_hash, verify_envelope, verify_head, verify_history_attestation,
};
use serde::Deserialize;

#[path = "../../fusiform-publish/tests/support/consumer.rs"]
mod consumer;

#[derive(Deserialize)]
struct Observations {
    recovered_hash: String,
    published_hash: String,
    envelope_signature: Signatures,
    changed_envelope: ReleaseEnvelope,
    head_signature: Signatures,
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("chain") {
        check_signed_chain();
        return;
    }
    let result: Observations = serde_json::from_reader(io::stdin().lock()).unwrap();
    let snapshot: Snapshot = serde_json::from_slice(include_bytes!(
        "../../fusiform-release/fixtures/release-format-v1/snapshot.json"
    ))
    .unwrap();
    let head: Head = serde_json::from_slice(include_bytes!(
        "../../fusiform-release/fixtures/release-format-v1/head.json"
    ))
    .unwrap();
    let upstream = snapshot.state.upstream;
    assert_eq!(
        result.recovered_hash,
        upstream_state_hash(&upstream).to_string()
    );
    let index = FactIndex::from_snapshot_upstream(&upstream).unwrap();
    let models_dev = include_bytes!("../fixtures/models-dev.json");
    // The sample has no audio rates, so native and JS can use different evidence.
    let native_step = publish_step(
        models_dev,
        &index,
        &ChargeUnitEvidence::default(),
        snapshot.envelope.upstream_observed_at_ms,
    )
    .unwrap();
    assert_eq!(result.published_hash, native_step.upstream_hash.to_string());

    let key = SigningKey::from_bytes(&[42; 32]);
    let keys = BTreeMap::from([("release-format-v1-test-key".into(), key.verifying_key())]);
    verify_envelope(&snapshot.envelope, &result.envelope_signature, &keys).unwrap();
    assert_eq!(
        verify_envelope(&result.changed_envelope, &result.envelope_signature, &keys),
        Err(SignatureError::BadSignature),
    );
    verify_head(&head.head, &result.head_signature, &keys).unwrap();
}

fn check_signed_chain() {
    #[derive(Deserialize)]
    struct Observations {
        releases: [String; 3],
        snapshot: String,
        base: String,
        day: String,
    }

    let result: Observations = serde_json::from_reader(io::stdin().lock()).unwrap();
    let releases: [Release; 3] = result
        .releases
        .each_ref()
        .map(|wire| serde_json::from_str(wire).unwrap());
    let key = SigningKey::from_bytes(&[42; 32]);
    let keys = BTreeMap::from([("release-format-v1-test-key".into(), key.verifying_key())]);
    let index = consumer::check_chain(&releases, &keys).unwrap();
    let snapshot: Snapshot = serde_json::from_str(&result.snapshot).unwrap();
    assert_eq!(snapshot.envelope, releases[1].envelope);
    assert_eq!(
        snapshot.envelope.curation_hash,
        snapshot.state.curation_set.curation_hash()
    );
    assert_eq!(
        index,
        FactIndex::from_snapshot_upstream(&snapshot.state.upstream).unwrap()
    );
    verify_envelope(&snapshot.envelope, &snapshot.signatures, &keys).unwrap();
    let attestation = snapshot.history_attestation.unwrap();
    verify_history_attestation(&attestation, &snapshot.envelope, &keys).unwrap();
    assert_eq!(attestation.body.history.len(), 2);
    attestation.body.history[0]
        .verify_bytes(result.base.as_bytes())
        .unwrap();
    attestation.body.history[1]
        .verify_bytes(result.day.as_bytes())
        .unwrap();
}
