use fusiform_publish::{FactIndex, ReplayError, ReplayRefusal};
use fusiform_release::{Change, ChangeOp, Upstream, upstream_state_hash};
use serde_json::{Value, json};

fn snapshot(value: Value) -> Upstream {
    serde_json::from_value(value).unwrap()
}

fn first_snapshot() -> Upstream {
    snapshot(
        json!({"p": {"provider": {"provider.name": "P"}, "models": {"-": {"existence": "present"}}}}),
    )
}

fn change(upstream: Vec<ChangeOp>) -> Change {
    Change {
        upstream,
        curation_set: None,
    }
}

fn set(provider: &str, model: &str, fact: &str, value: Value) -> ChangeOp {
    ChangeOp::Set {
        provider: provider.into(),
        model: model.into(),
        fact: fact.into(),
        value,
    }
}

fn remove(provider: &str, model: &str, fact: &str) -> ChangeOp {
    ChangeOp::Remove {
        provider: provider.into(),
        model: model.into(),
        fact: fact.into(),
    }
}

fn set_provider(provider: &str, value: Value) -> ChangeOp {
    ChangeOp::SetProvider {
        provider: provider.into(),
        fact: "provider.name".into(),
        value,
    }
}

fn remove_provider(provider: &str) -> ChangeOp {
    ChangeOp::RemoveProvider {
        provider: provider.into(),
        fact: "provider.name".into(),
    }
}

#[test]
fn rebuild_and_replay_produces_the_same_hash_as_a_snapshot_at_head() {
    let first = first_snapshot();
    let mut index = FactIndex::from_snapshot_upstream(&first).unwrap();
    assert_eq!(index.upstream_hash(), upstream_state_hash(&first));
    assert_eq!(index.model_count(), 1);
    assert_eq!(index.fact_count(), 2);

    index
        .apply_change(&change(vec![
            set_provider("p", json!("P renamed")),
            set("p", "-", "model.name", json!("First model")),
            set_provider("q", json!("Q")),
            set("q", "m", "existence", json!("present")),
        ]))
        .unwrap();
    assert_eq!(index.model_count(), 2);
    assert_eq!(index.fact_count(), 5);
    index
        .apply_change(&change(vec![
            remove_provider("p"),
            remove("p", "-", "existence"),
            remove("p", "-", "model.name"),
            set("q", "m", "model.name", json!("New model")),
        ]))
        .unwrap();

    let head = snapshot(json!({"q": {
        "provider": {"provider.name": "Q"},
        "models": {"m": {"existence": "present", "model.name": "New model"}}
    }}));
    assert_eq!(index.upstream_hash(), upstream_state_hash(&head));
    assert_eq!(index, FactIndex::from_snapshot_upstream(&head).unwrap());
    assert_eq!(index, FactIndex::from_bytes(&index.to_bytes()).unwrap());
}

fn assert_refused(ops: Vec<ChangeOp>, operation: usize, reason: ReplayRefusal) {
    let mut index = FactIndex::from_snapshot_upstream(&first_snapshot()).unwrap();
    let original = index.to_bytes();
    assert_eq!(
        index.apply_change(&change(ops)),
        Err(ReplayError { operation, reason })
    );
    assert_eq!(
        index.to_bytes(),
        original,
        "a refusal must not update the index"
    );
}

#[test]
fn replay_refuses_an_absent_model_fact_after_a_valid_set() {
    assert_refused(
        vec![
            set("p", "-", "model.name", json!("Name")),
            remove("p", "-", "rate.input"),
        ],
        1,
        ReplayRefusal::AbsentFact,
    );
}

#[test]
fn replay_refuses_an_absent_provider_fact() {
    assert_refused(
        vec![set_provider("p", json!("Name")), remove_provider("q")],
        1,
        ReplayRefusal::AbsentFact,
    );
}

#[test]
fn replay_refuses_unsorted_and_duplicate_operations() {
    assert_refused(
        vec![
            set_provider("p", json!("Name")),
            set_provider("p", json!("Same key")),
        ],
        1,
        ReplayRefusal::UnsortedOrDuplicate,
    );
    assert_refused(
        vec![
            set("q", "m", "existence", json!("present")),
            set_provider("p", json!("Wrong order")),
        ],
        1,
        ReplayRefusal::UnsortedOrDuplicate,
    );
}

#[test]
fn replay_refuses_a_provider_only_arrival_and_leftover_provider_facts() {
    assert_refused(
        vec![
            set_provider("p", json!("Name")),
            set_provider("q", json!("No model")),
        ],
        1,
        ReplayRefusal::UnknownProvider,
    );
    assert_refused(
        vec![
            set_provider("p", json!("Name")),
            remove("p", "-", "existence"),
        ],
        1,
        ReplayRefusal::OrphanProviderFacts,
    );
}
