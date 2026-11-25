use super::*;
use fusiform_release::{Snapshot, upstream_state_hash};
use serde_json::Value;

#[path = "../../fusiform-publish/tests/support/consumer.rs"]
mod consumer;

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
    let audio = br#"{"google":{"models":{"gemini-3.1-flash-live-preview":{"cost":{"input_audio":3,"output_audio":12}}}}}"#;
    let (summary, _) = publish_step(audio, &index, EVIDENCE, 0).unwrap();
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
    let (summary, next) = publish_step(MODELS, &prior, EVIDENCE, 0).unwrap();
    let summary: Value = serde_json::from_slice(&summary).unwrap();
    assert_eq!(summary["model_count"], 2);
    assert_eq!(
        summary["base_upstream_hash"],
        upstream_hash(&prior).unwrap()
    );
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
    let (genesis, after1) = publish_step(MODELS, &empty, EVIDENCE, 0).unwrap();
    let after1 = after1.unwrap();
    let release = |summary: &[u8], parent: &[u8], number, time| {
        build_release(summary, curation, parent, number, time, time - 1)
    };
    let envelope = |wire: &[u8]| {
        let unsigned: Value = serde_json::from_slice(wire).unwrap();
        let envelope: ReleaseEnvelope =
            serde_json::from_value(unsigned["envelope"].clone()).unwrap();
        serde_json::to_vec(&envelope).unwrap()
    };
    let r1 = envelope(&release(&genesis, &[], 1, 1).unwrap());

    let mut catalog: Value = serde_json::from_slice(MODELS).unwrap();
    catalog["alpha"]["models"]["-"]["cost"] = json!({});
    let (with_removal, after2) =
        publish_step(&canonical_json(&catalog), &after1, EVIDENCE, 0).unwrap();
    let r2 = envelope(&release(&with_removal, &r1, 2, 2).unwrap());
    let mut stale = catalog;
    stale["zeta"]["models"]["mini"]["name"] = json!("Mini 2");
    let catalog = canonical_json(&stale);
    let (stale, _) = publish_step(&catalog, &after1, EVIDENCE, 0).unwrap();
    let summary: Value = serde_json::from_slice(&stale).unwrap();
    assert!(summary["ops"].as_array().unwrap().contains(&json!({
        "op": "remove", "provider": "alpha", "model": "-", "fact": "rate.input"
    })));
    assert_eq!(
        release(&stale, &r2, 3, 3).err().map(|err| err.class),
        Some("invalid_summary"),
        "builder accepted a repeated removal diffed from release 1 against parent release 2"
    );
    let (fresh, _) = publish_step(&catalog, &after2.unwrap(), EVIDENCE, 0).unwrap();
    assert!(release(&fresh, &r2, 3, 3).is_ok());
    assert_eq!(
        release(&stale, &[], 1, 3).unwrap_err().class,
        "invalid_summary"
    );
}

#[test]
fn release_with_decision_grade_replays_but_unknown_kind_is_refused() {
    use fusiform_release::{Release, SigningKey, build_release, sign_envelope, state_hash};

    let first: Release = decode_json(
        include_bytes!("../../fusiform-release/fixtures/release-format-v1/chain/release-1.json"),
        "fixture",
    )
    .unwrap();
    let key = SigningKey::from_bytes(&[42; 32]);
    let kid = "release-format-v1-test-key";
    let keys = std::collections::BTreeMap::from([(kid.to_string(), key.verifying_key())]);
    let mut index = FactIndex::from_snapshot_upstream(&Default::default()).unwrap();
    index.apply_change(&first.change).unwrap();
    let mut catalog: Value = serde_json::from_slice(MODELS).unwrap();
    catalog["zeta"]["models"]["mini"]["name"] = json!("Mini 2");
    let (summary, next) =
        publish_step(&canonical_json(&catalog), &index.to_bytes(), EVIDENCE, 0).unwrap();
    let mut curation = first.change.curation_set.clone().unwrap();
    curation.version += 1;
    curation
        .kind_grades
        .insert("zeta/mini".into(), [("decision".into(), 83.into())].into());
    let time = first.envelope.published_at_ms + 1;
    let unsigned = build_release(
        &summary,
        &serde_json::to_vec(&curation).unwrap(),
        &serde_json::to_vec(&first.envelope).unwrap(),
        2,
        time,
        time,
    )
    .unwrap();
    let mut unsigned: Value = serde_json::from_slice(&unsigned).unwrap();
    let envelope = serde_json::from_value(unsigned["envelope"].clone()).unwrap();
    unsigned["signatures"] = json!(sign_envelope(&envelope, kid, &key).unwrap());
    let second: Release = serde_json::from_value(unsigned).unwrap();
    assert!(!second.change.upstream.is_empty());
    assert_eq!(
        consumer::check_chain(&[first.clone(), second.clone()], &keys)
            .unwrap()
            .to_bytes(),
        next.unwrap()
    );

    // Re-sign the bad grade so replay has to inspect the kind itself, not just
    // reject a mismatched signature or hash.
    let mut invalid = second;
    curation
        .kind_grades
        .insert("zeta/mini".into(), [("bogus".into(), 83.into())].into());
    let envelope = &mut invalid.envelope;
    envelope.curation_hash = curation.curation_hash();
    envelope.state_hash = state_hash(envelope.curation_hash, envelope.upstream_hash);
    invalid.change.curation_set = Some(curation);
    envelope.change_hash = invalid.change.change_hash();
    invalid.signatures = sign_envelope(envelope, kid, &key).unwrap();
    let failure = consumer::check_chain(&[first, invalid], &keys)
        .expect_err("publisher chain checker replayed a signed bogus task grade");
    assert!(
        failure.contains("invalid task grade for zeta/mini"),
        "{failure}"
    );
}

#[test]
fn zero_length_index_is_not_bootstrap() {
    let (summary, index) = publish_step(MODELS, &[], EVIDENCE, 0).unwrap();
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
