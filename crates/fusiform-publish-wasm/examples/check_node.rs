//! Native oracle for the Node integration test. Reads its observations on stdin.

use std::{collections::BTreeMap, io};

use fusiform_core::ChargeUnitEvidence;
use fusiform_publish::{FactIndex, publish_step};
use fusiform_release::{
    Head, ReleaseEnvelope, SignatureError, Signatures, SigningKey, Snapshot, upstream_state_hash,
    verify_envelope, verify_head,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct Observations {
    recovered_hash: String,
    published_hash: String,
    envelope_signature: Signatures,
    changed_envelope: ReleaseEnvelope,
    head_signature: Signatures,
}

fn main() {
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
    let native_step = publish_step(models_dev, &index, &ChargeUnitEvidence::default()).unwrap();
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
