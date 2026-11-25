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
    assert_eq!(summary["base_upstream_hash"], upstream_hash(&prior).unwrap());
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
fn release_builder_refuses_a_diff_from_a_stale_index() {
    use fusiform_release::{ReleaseEnvelope, build_release};

    let empty = FactIndex::from_snapshot_upstream(&Default::default())
        .unwrap()
        .to_bytes();
    let curation = include_bytes!("../fixtures/curation.json");
    let (genesis, after1) = publish_step(MODELS, &empty, EVIDENCE).unwrap();
    let after1 = after1.unwrap();
    let release = |summary: &[u8], parent: &[u8], number, time| {
        build_release(summary, curation, parent, number, time, time - 1)
    };
    let envelope = |wire: &[u8]| {
        let unsigned: Value = serde_json::from_slice(wire).unwrap();
        let envelope: ReleaseEnvelope = serde_json::from_value(unsigned["envelope"].clone()).unwrap();
        serde_json::to_vec(&envelope).unwrap()
    };
    let r1 = envelope(&release(&genesis, &[], 1, 1).unwrap());

    let mut catalog: Value = serde_json::from_slice(MODELS).unwrap();
    catalog["alpha"]["models"]["-"]["cost"] = json!({});
    let (with_removal, after2) =
        publish_step(&canonical_json(&catalog), &after1, EVIDENCE).unwrap();
    let r2 = envelope(&release(&with_removal, &r1, 2, 2).unwrap());
    let mut stale = catalog;
    stale["zeta"]["models"]["mini"]["name"] = json!("Mini 2");
    let catalog = canonical_json(&stale);
    let (stale, _) = publish_step(&catalog, &after1, EVIDENCE).unwrap();
    let summary: Value = serde_json::from_slice(&stale).unwrap();
    assert!(summary["ops"].as_array().unwrap().contains(&json!({
        "op": "remove", "provider": "alpha", "model": "-", "fact": "rate.input"
    })));
    assert_eq!(
        release(&stale, &r2, 3, 3).err().map(|err| err.class),
        Some("invalid_summary"),
        "builder accepted a repeated removal diffed from release 1 against parent release 2"
    );
    let (fresh, _) = publish_step(&catalog, &after2.unwrap(), EVIDENCE).unwrap();
    assert!(release(&fresh, &r2, 3, 3).is_ok());
    assert_eq!(release(&stale, &[], 1, 3).unwrap_err().class, "invalid_summary");
}

#[test]
fn replay_accepts_a_change_with_a_decision_grade() {
    let mut snapshot: Snapshot = serde_json::from_slice(SNAPSHOT).unwrap();
    let index = index_from_snapshot(SNAPSHOT).unwrap();
    snapshot
        .state
        .curation_set
        .kind_grades
        .entry("fixture/decision".into())
        .or_default()
        .insert("decision".into(), 83.into());
    let change =
        canonical_json(&json!({"upstream": [], "curation_set": snapshot.state.curation_set}));
    assert_eq!(index_apply_change(&index, &change).unwrap(), index);
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
