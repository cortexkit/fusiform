use fusiform_core::ChargeUnitEvidence;
use fusiform_publish::{FactIndex, IndexDecodeError, PublishRefusal, publish_step};
use fusiform_release::ChangeOp;
use serde_json::json;

fn empty_snapshot_index() -> FactIndex {
    FactIndex::from_snapshot_upstream(&Default::default()).unwrap()
}

fn document(model_count: usize) -> Vec<u8> {
    let models: serde_json::Map<_, _> = (0..model_count)
        .map(|n| {
            (
                format!("model-{n:03}"),
                json!({"name": format!("Model {n:03}")}),
            )
        })
        .collect();
    serde_json::to_vec(&json!({"p": {"name": "P", "models": models}})).unwrap()
}

#[test]
fn an_identical_observation_has_no_ops_even_after_the_index_round_trips() {
    let evidence = ChargeUnitEvidence::default();
    let json = document(2);
    let first = publish_step(&json, &empty_snapshot_index(), &evidence).unwrap();
    let stored = FactIndex::from_bytes(&first.index.to_bytes()).unwrap();
    assert_eq!(stored, first.index);
    assert_eq!(stored.upstream_hash(), first.upstream_hash);
    let same = publish_step(&json, &stored, &evidence).unwrap();
    assert!(same.ops.is_empty());
    assert_eq!(same.upstream_hash, first.upstream_hash);
}

#[test]
fn a_changed_fact_withdrawal_and_an_arrival_remain_sorted() {
    let initial = br#"{"p":{"models":{"a":{"name":"Remove this"},"c":{}}}}"#;
    let changed = br#"{"p":{"models":{"a":{"limit":{"context":100}},"b":{}}}}"#;
    let evidence = ChargeUnitEvidence::default();
    let previous = publish_step(initial, &empty_snapshot_index(), &evidence).unwrap();
    let next = publish_step(changed, &previous.index, &evidence).unwrap();
    assert!(
        next.ops
            .windows(2)
            .all(|pair| pair[0].key() < pair[1].key())
    );
    assert_eq!(
        next.ops[0],
        ChangeOp::Set {
            provider: "p".into(),
            model: "a".into(),
            fact: "limit.context".into(),
            value: json!(100),
        }
    );
    assert_eq!(
        next.ops[1],
        ChangeOp::Remove {
            provider: "p".into(),
            model: "a".into(),
            fact: "model.name".into(),
        }
    );
    assert!(next.ops.iter().any(
        |op| matches!(op, ChangeOp::Set { model, fact, .. } if model == "b" && fact == "existence")
    ));
    assert!(next.ops.iter().any(|op| matches!(op, ChangeOp::Remove { model, fact, .. } if model == "c" && fact == "existence")));
}

#[test]
fn refusals_separate_parse_errors_from_a_drop_beyond_the_guard() {
    let evidence = ChargeUnitEvidence::default();
    let first = publish_step(&document(100), &empty_snapshot_index(), &evidence).unwrap();
    let parse = publish_step(b"{", &first.index, &evidence).unwrap_err();
    assert!(matches!(parse, PublishRefusal::Parse(_)));
    assert_eq!(parse.class(), "parse");

    assert!(publish_step(&document(75), &first.index, &evidence).is_ok());
    let shrink = publish_step(&document(74), &first.index, &evidence).unwrap_err();
    assert!(matches!(shrink, PublishRefusal::Implausible(_)));
    assert_eq!(shrink.class(), "implausible");

    let small = publish_step(&document(99), &empty_snapshot_index(), &evidence).unwrap();
    assert!(publish_step(&document(1), &small.index, &evidence).is_ok());
    assert!(publish_step(&document(101), &first.index, &evidence).is_ok());
}

#[test]
fn refuses_empty_upstream_ids() {
    let previous = empty_snapshot_index();
    let evidence = ChargeUnitEvidence::default();
    for (input, detail) in [
        (
            br#"{"":{"models":{"m":{}}}}"#.as_slice(),
            "empty provider id",
        ),
        (br#"{"p":{"models":{"":{}}}}"#.as_slice(), "empty model id"),
    ] {
        assert!(
            matches!(
                publish_step(input, &previous, &evidence),
                Err(PublishRefusal::Parse(reason)) if reason.contains(detail)
            ),
            "expected a refusal for {detail}"
        );
    }
}

#[test]
fn the_index_refuses_corruption_truncation_and_unsupported_versions() {
    let step = publish_step(
        &document(2),
        &empty_snapshot_index(),
        &ChargeUnitEvidence::default(),
    )
    .unwrap();
    let mut bytes = step.index.to_bytes();
    for cut in [0, 3, bytes.len() / 2, bytes.len() - 1] {
        assert!(
            FactIndex::from_bytes(&bytes[..cut]).is_err(),
            "cut at {cut}"
        );
    }
    *bytes.last_mut().unwrap() ^= 1;
    assert_eq!(
        FactIndex::from_bytes(&bytes),
        Err(IndexDecodeError::HashMismatch)
    );
    bytes[4] = 99;
    assert_eq!(
        FactIndex::from_bytes(&bytes),
        Err(IndexDecodeError::UnsupportedHeader)
    );
}
