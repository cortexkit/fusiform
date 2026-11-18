use fusiform_publish::{FactIndex, SnapshotIndexError, SnapshotReadError};
use fusiform_release::Upstream;

#[test]
fn streaming_and_typed_recovery_agree_even_on_unsorted_snapshot_keys() {
    let wire = br#"{
        "z":{"models":{"z":{"z":1E7,"a":0.30000000000000004},"-":{"existence":"present"}},"provider":{"pp":"Z"}},
        "a\t":{"provider":{},"models":{"m\n":{"name":"\u96ea","existence":"present"}}}
    }"#;
    let typed: Upstream = serde_json::from_slice(wire).unwrap();
    let typed_index = FactIndex::from_snapshot_upstream(&typed).unwrap();
    let streamed_index = FactIndex::from_snapshot_upstream_reader(&wire[..]).unwrap();
    assert_eq!(streamed_index, typed_index);
    assert_eq!(streamed_index.model_count(), 3);
    assert_eq!(streamed_index.fact_count(), 6);
}

fn duplicate(wire: &[u8], key: &str) {
    let Err(SnapshotReadError::Json(err)) = FactIndex::from_snapshot_upstream_reader(wire) else {
        panic!("expected a duplicate-key refusal for {key:?}");
    };
    assert!(
        err.to_string().contains(&format!("duplicate key {key:?}")),
        "{err}"
    );
}

#[test]
fn the_reader_refuses_duplicate_providers() {
    duplicate(br#"{"p":{"provider":{},"models":{"m":{"f":0}}},"p":{"provider":{},"models":{"n":{"f":1}}}}"#, "p");
}

#[test]
fn the_reader_refuses_duplicate_models() {
    duplicate(
        br#"{"p":{"provider":{},"models":{"m":{"f":0},"m":{"f":1}}}}"#,
        "m",
    );
}

#[test]
fn the_reader_refuses_duplicate_facts_at_both_scopes() {
    duplicate(
        br#"{"p":{"provider":{},"models":{"m":{"f":0,"f":1}}}}"#,
        "f",
    );
    duplicate(
        br#"{"p":{"provider":{"f":0,"f":1},"models":{"m":{"existence":"present"}}}}"#,
        "f",
    );
}

#[test]
fn both_recovery_paths_refuse_a_provider_without_models() {
    let wire = br#"{"p":{"provider":{"provider.name":"P"},"models":{}}}"#;
    let typed = serde_json::from_slice(wire).unwrap();
    assert_eq!(
        FactIndex::from_snapshot_upstream(&typed),
        Err(SnapshotIndexError::EmptyProvider("p".into()))
    );
    assert!(
        matches!(FactIndex::from_snapshot_upstream_reader(&wire[..]),
        Err(SnapshotReadError::InvalidUpstream(SnapshotIndexError::EmptyProvider(provider))) if provider == "p")
    );
}
