use super::*;

#[test]
fn v1_catalog_is_refused_without_publishing() {
    let index = FactIndex::from_snapshot_upstream(&Default::default())
        .unwrap()
        .to_bytes();
    let evidence = include_bytes!("../fixtures/evidence.json");
    let v1 = br#"{"p":{"id":"p","models":{"m":{"id":"m","name":"Old","cost":{"input":1},"modalities":{"input":["text"],"output":["text"]}}}}}"#;
    let (summary, replacement) = publish_step(v1, &index, evidence, 0).unwrap();
    let summary: serde_json::Value = serde_json::from_slice(&summary).unwrap();
    assert_eq!(summary["refusal"]["class"], "parse");
    assert_eq!(summary["ops"], json!([]));
    assert!(replacement.is_none());
}

#[test]
fn snapshot_replay_rejects_an_invalid_pinned_key() {
    let err = snapshot_replay::snapshot_bytes_from_previous(b"", b"", b"", b"", b"{}").unwrap_err();
    assert_eq!(err.class, "invalid_release_public_key");
}

#[test]
fn incremental_publish_holds_a_refused_fact_and_reports_why() {
    let evidence = br#"{"input-audio-per-token":{"schema":"input-audio-per-token/v1","providers":[]},"output-audio-per-token":{"schema":"output-audio-per-token/v1","providers":[]}}"#;
    let body = |limit: &str| {
        format!(
            r#"{{"p":{{"models":{{"m":{{"type":"chat","api":{{}},"capabilities":{{}}{limit}}}}}}}}}"#
        )
    };
    let empty = FactIndex::from_snapshot_upstream(&Default::default())
        .unwrap()
        .to_bytes();
    let (_, index) = publish_step(
        body(r#", "limit":{"context":4096}"#).as_bytes(),
        &empty,
        evidence,
        1,
    )
    .unwrap();
    let index = index.unwrap();
    let (summary, next) = publish_step(
        body(r#", "limit":{"context":"unknown"}"#).as_bytes(),
        &index,
        evidence,
        2,
    )
    .unwrap();
    let summary: serde_json::Value = serde_json::from_slice(&summary).unwrap();
    assert_eq!(summary["ops"], json!([]), "a refusal is not a withdrawal");
    assert_eq!(next.unwrap(), index);
    assert_eq!(summary["held_fact_count"], 1);
    assert_eq!(
        summary["findings"][0]["key"],
        json!({"kind":"refused_model_member", "path":"limit.context"})
    );

    let (summary, next) = publish_step(
        body(r#", "limit":{"output":128}"#).as_bytes(),
        &index,
        evidence,
        3,
    )
    .unwrap();
    let summary: serde_json::Value = serde_json::from_slice(&summary).unwrap();
    assert_eq!(summary["held_fact_count"], 0);
    assert_ne!(next.unwrap(), index, "true absence changes the index");
    assert!(
        summary["ops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["fact"] == "limit.context")
    );
}
