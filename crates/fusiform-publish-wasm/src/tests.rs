use super::*;
use fusiform_release::{Snapshot, upstream_state_hash};
use serde_json::Value;

const SNAPSHOT: &[u8] =
    include_bytes!("../../fusiform-release/fixtures/release-format-v1/snapshot.json");
const EVIDENCE: &[u8] = include_bytes!("../fixtures/evidence.json");
const MODELS: &[u8] = include_bytes!("../fixtures/models-dev.json");

#[test]
fn evidence_fixture_is_canonical_and_comes_from_both_embedded_documents() {
    let doc = |bytes| serde_json::from_slice::<Value>(bytes).unwrap();
    let expected = json!({
        "input-audio-per-token": doc(include_bytes!("../../fusiform-module/data/input-audio-per-token.json")),
        "output-audio-per-token": doc(include_bytes!("../../fusiform-module/data/output-audio-per-token.json")),
    });
    assert_eq!(EVIDENCE, canonical_json(&expected));

    let index = index_from_snapshot(SNAPSHOT).unwrap();
    let audio = br#"{"openai":{"models":{"gpt-realtime-2.1":{"cost":{"input_audio":32,"output_audio":64}}}}}"#;
    let (summary, _) = publish_step(audio, &index, EVIDENCE).unwrap();
    let ops = &doc(&summary)["ops"];
    for fact in ["rate.input_audio", "rate.output_audio"] {
        assert!(
            ops.as_array()
                .unwrap()
                .iter()
                .any(|op| op["op"] == "set" && op["fact"] == fact)
        );
    }
}

#[test]
fn publish_index_and_replayed_index_agree_by_hash() {
    let snapshot: Snapshot = serde_json::from_slice(SNAPSHOT).unwrap();
    let prior = index_from_snapshot(SNAPSHOT).unwrap();
    assert_eq!(
        upstream_hash(&prior).unwrap(),
        upstream_state_hash(&snapshot.state.upstream).to_string()
    );
    let (summary, next) = publish_step(MODELS, &prior, EVIDENCE).unwrap();
    let summary: Value = serde_json::from_slice(&summary).unwrap();
    assert_eq!(summary["model_count"], 2);
    assert!(summary.get("refusal").is_none());
    let next = next.unwrap();
    let change = canonical_json(&json!({"upstream": summary["ops"], "curation_set": null}));
    let replay = index_apply_change(&prior, &change).unwrap();
    assert_eq!(
        upstream_hash(&replay).unwrap(),
        upstream_hash(&next).unwrap()
    );
    assert_eq!(summary["upstream_hash"], upstream_hash(&next).unwrap());
}

#[test]
fn zero_length_index_is_not_bootstrap() {
    let (summary, index) = publish_step(MODELS, &[], EVIDENCE).unwrap();
    assert!(index.is_none());
    let summary: Value = serde_json::from_slice(&summary).unwrap();
    assert_eq!(summary["refusal"]["class"], "empty_index");
    assert_eq!(summary["ops"], json!([]));
    assert_eq!(summary["conflicts"], json!([]));
    assert!(summary.get("upstream_hash").is_none());
    assert!(summary.get("model_count").is_none());
}

#[test]
fn signing_bytes_are_canonical_bodies_not_transport_wrappers() {
    let snapshot: Value = serde_json::from_slice(SNAPSHOT).unwrap();
    let head: Value = serde_json::from_slice(include_bytes!(
        "../../fusiform-release/fixtures/release-format-v1/head.json"
    ))
    .unwrap();
    let envelope = &snapshot["envelope"];
    let head = &head["head"];
    assert_eq!(
        envelope_signing_bytes(&serde_json::to_vec_pretty(envelope).unwrap()).unwrap(),
        canonical_json(envelope)
    );
    assert_eq!(
        head_signing_bytes(&serde_json::to_vec_pretty(head).unwrap()).unwrap(),
        canonical_json(head)
    );
    assert_eq!(
        envelope_signing_bytes(SNAPSHOT).unwrap_err().class,
        "invalid_envelope"
    );
}
