use fusiform_release::{Release, build_release, canonical_json};
use serde_json::{Value, json};

fn check_refusal(edit: impl FnOnce(&mut Value), detail: &str) {
    let first: Release = serde_json::from_slice(include_bytes!(
        "../fixtures/release-format-v1/chain/release-1.json"
    ))
    .unwrap();
    let parent = &first.envelope;
    let mut curation = serde_json::to_value(first.change.curation_set.unwrap()).unwrap();
    curation["version"] = json!(parent.curation_version + 1);
    edit(&mut curation);
    let result = build_release(
        &canonical_json(&json!({
            "base_upstream_hash": parent.upstream_hash,
            "upstream_hash": parent.upstream_hash,
            "ops": [],
        })),
        &canonical_json(&curation),
        &serde_json::to_vec(parent).unwrap(),
        2,
        parent.published_at_ms + 1,
        parent.upstream_observed_at_ms,
    );
    let failure = result
        .err()
        .unwrap_or_else(|| panic!("builder accepted curation: {detail}"));
    assert_eq!(failure.class, "invalid_curation");
    assert!(failure.detail.contains(detail), "{}", failure.detail);
}

#[test]
fn refuses_unknown_task_grade_kind() {
    check_refusal(
        |set| set["kind_grades"]["p/m"] = json!({"Decision": 83}),
        "invalid task grade for p/m",
    );
}

#[test]
fn refuses_zero_chat_grade() {
    check_refusal(
        |set| set["grades"]["mini"]["elo"] = json!(0),
        "invalid chat grade for mini",
    );
}

#[test]
fn refuses_blank_graded_effort() {
    check_refusal(
        |set| set["grades"]["mini"]["graded_effort"] = json!(" "),
        "invalid chat grade for mini",
    );
}

#[test]
fn refuses_unknown_workload() {
    check_refusal(
        |set| set["workload_efforts"]["p/m"] = json!({"coding": {"default": "high"}}),
        "invalid workload effort for p/m",
    );
}

#[test]
fn refuses_local_key_in_chat_grades() {
    check_refusal(
        |set| set["grades"]["local:p/m"] = json!({"elo": 1, "eq": 1}),
        "invalid published model key \"local:p/m\"",
    );
}

#[test]
fn refuses_unknown_vendor_document() {
    check_refusal(
        |set| set["vendor_facts"]["unknown-document"] = json!({}),
        "unrecognized published vendor document \"unknown-document\"",
    );
}

#[test]
fn refuses_malformed_curated_providers() {
    check_refusal(
        |set| {
            set["vendor_facts"]["curated-providers"] =
                json!({"schema": "curated-providers/v2", "providers": []});
        },
        "curated-providers: expected curated-providers/v1 and a providers array",
    );
}

#[test]
fn refuses_rejected_plan_price_row() {
    check_refusal(
        |set| set["vendor_facts"]["plan-prices"] = json!({"cells": [{}]}),
        "plan-prices: cell 0: missing provider_id or tier",
    );
}
