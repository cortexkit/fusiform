use fusiform_release::{
    ArtifactRefusal, CurationSet, Release, ReleaseEnvelope, Upstream, build_release,
    canonical_json, decode_curation, validate_published,
};
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

fn legacy_carbon_aliases() -> Value {
    json!({"schema": "model-aliases/v1", "aliases": [{
        "provider_id": "google",
        "model_id": "antigravity-gemini-4-carbon",
        "model_name": "Gemini 4 Carbon (Antigravity)",
        "wire_family": "google.code_assist",
        "target": {"provider_id": "google", "model_id": "gemini-4-carbon"},
        "basis": "The resolver recognises the id as Carbon (isGeminiCarbonModel) and resolves every tier through getGeminiCarbonAntigravityModel: low, medium and high go to gemini-4-carbon-low, -medium and -high, and a missing or unknown tier defaults to gemini-4-carbon-medium. RESOLVER_ALIASES maps each of those back to gemini-4-carbon, so the suffix spells a reasoning level and every request reaches Google's gemini-4-carbon.",
        "source_ref": "https://github.com/cortexkit/antigravity-auth/blob/cb40633d96e87ad2ad958e4940c71fa2a685df94/packages/core/src/model-registry.ts (ALL_MODEL_DEFINITIONS antigravity-gemini-4-carbon, RESOLVER_ALIASES, isGeminiCarbonModel, getGeminiCarbonAntigravityModel), pinned by packages/core/src/carbon-model.test.ts at the same commit; protocol: https://github.com/cortexkit/antigravity-auth/blob/44eb8fa0ddd93e236942b9b36f9bb10fa2abde6d/packages/opencode/src/plugin/request.ts#L1387-L1390 constructs /v1internal:${rawAction}?alt=sse; #L2373-L2393 serializes the Antigravity project/request/model envelope",
        "established_at_ms": 1794355200000_i64,
        "review_by_ms": 1799539200000_i64,
        "limit_output": {
            "value": 262144,
            "source_ref": "https://github.com/cortexkit/antigravity-auth/blob/cb40633d96e87ad2ad958e4940c71fa2a685df94/test-fixtures/agy-cli-1.5.5/native-carbon-low.json (requests[].native_generation_config); https://github.com/cortexkit/antigravity-auth/blob/cb40633d96e87ad2ad958e4940c71fa2a685df94/packages/core/src/model-registry.ts (ALL_MODEL_DEFINITIONS antigravity-gemini-4-carbon)",
            "quote": "The captured agy 1.5.5 request for \"model\": \"gemini-4-carbon-low\" sends \"maxOutputTokens\": 262144; the registry declares limit: { context: 1048576, output: 262144 }."
        },
        "reasoning_wire_ids": {
            "low": "gemini-4-carbon-low",
            "medium": "gemini-4-carbon-medium",
            "high": "gemini-4-carbon-high"
        },
        "reasoning_default": "medium"
    }]})
}

fn legacy_carbon_curation() -> (ReleaseEnvelope, CurationSet) {
    let first: Release = serde_json::from_slice(include_bytes!(
        "../fixtures/release-format-v1/chain/release-1.json"
    ))
    .unwrap();
    let mut curation = first.change.curation_set.unwrap();
    curation
        .vendor_facts
        .insert("model-aliases".into(), legacy_carbon_aliases());
    let mut parent = first.envelope;
    parent.curation_hash = curation.curation_hash();
    (parent, curation)
}

#[test]
fn decodes_stored_alias_with_retired_output_override() {
    let (_, curation) = legacy_carbon_curation();
    let raw = serde_json::to_vec(&curation).unwrap();

    let (decoded, hash) = decode_curation(&raw).unwrap();
    assert_eq!(decoded, curation);
    assert_eq!(hash, curation.curation_hash());
}

#[test]
fn draft_validation_refuses_alias_with_retired_output_override() {
    let (_, curation) = legacy_carbon_curation();
    let error = validate_published(&curation.vendor_facts, &Upstream::new()).unwrap_err();
    assert!(error.starts_with("model-aliases:"), "{error}");
    assert!(error.contains("unknown field `limit_output`"), "{error}");
}

fn build_next_with_curation(
    parent: &ReleaseEnvelope,
    curation: &CurationSet,
) -> Result<Vec<u8>, ArtifactRefusal> {
    let next: Release = serde_json::from_slice(include_bytes!(
        "../fixtures/release-format-v1/chain/release-2.json"
    ))
    .unwrap();
    build_release(
        &canonical_json(&json!({
            "base_upstream_hash": parent.upstream_hash,
            "upstream_hash": next.envelope.upstream_hash,
            "ops": next.change.upstream,
        })),
        &serde_json::to_vec(curation).unwrap(),
        &serde_json::to_vec(parent).unwrap(),
        next.envelope.release,
        next.envelope.published_at_ms,
        next.envelope.upstream_observed_at_ms,
    )
}

#[test]
fn builder_reuses_parent_curation_with_retired_alias_output_override() {
    let (parent, curation) = legacy_carbon_curation();
    let release = build_next_with_curation(&parent, &curation).unwrap();
    let release: Value = serde_json::from_slice(&release).unwrap();
    assert_eq!(release["change"].get("curation_set"), Some(&Value::Null));
    assert_eq!(
        release["envelope"]["curation_hash"],
        json!(parent.curation_hash)
    );
}

#[test]
fn builder_refuses_changed_curation_with_retired_alias_output_override() {
    let (parent, mut curation) = legacy_carbon_curation();
    curation.version += 1;
    let error = build_next_with_curation(&parent, &curation).unwrap_err();
    assert_eq!(error.class, "invalid_curation");
    assert!(error.detail.starts_with("model-aliases:"), "{error}");
    assert!(
        error.detail.contains("unknown field `limit_output`"),
        "{error}"
    );
}

fn legacy_wire_curation() -> (ReleaseEnvelope, CurationSet) {
    let first: Release = serde_json::from_slice(include_bytes!(
        "../fixtures/release-format-v1/chain/release-1.json"
    ))
    .unwrap();
    let mut curation = first.change.curation_set.unwrap();
    curation.vendor_facts.insert(
        "wire-corrections".into(),
        serde_json::from_str(include_str!("../fixtures/legacy-wire-corrections.json")).unwrap(),
    );
    let mut parent = first.envelope;
    parent.curation_hash = curation.curation_hash();
    (parent, curation)
}

#[test]
fn stored_wire_curation_preserves_retired_completeness_in_the_hash() {
    let (parent, curation) = legacy_wire_curation();
    let raw = serde_json::to_vec(&curation).unwrap();
    let (decoded, hash) = decode_curation(&raw).unwrap();
    assert_eq!(decoded, curation);
    assert_eq!(hash, parent.curation_hash);
    let release: Value =
        serde_json::from_slice(&build_next_with_curation(&parent, &curation).unwrap()).unwrap();
    assert_eq!(release["change"].get("curation_set"), Some(&Value::Null));
    assert_eq!(release["envelope"]["curation_hash"], json!(hash));
}

#[test]
fn authored_wire_curation_refuses_retired_completeness() {
    let (parent, mut curation) = legacy_wire_curation();
    const DETAIL: &str = "wire-corrections: unknown field `complete_providers`";
    let error = validate_published(&curation.vendor_facts, &Upstream::new()).unwrap_err();
    assert_eq!(error, DETAIL);
    curation.version += 1;
    let error = build_next_with_curation(&parent, &curation).unwrap_err();
    assert_eq!(error.class, "invalid_curation");
    assert_eq!(error.detail, DETAIL);
}
