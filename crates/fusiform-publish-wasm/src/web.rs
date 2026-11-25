use js_sys::{Array, Uint8Array};
use wasm_bindgen::prelude::*;

use super::{Failure, artifacts};

fn js_error(failure: Failure) -> JsError {
    JsError::new(&serde_json::to_string(&failure).expect("failure fields are strings"))
}

#[wasm_bindgen]
pub fn publish_step(
    models_dev: &[u8],
    index: &[u8],
    evidence: &[u8],
    upstream_observed_at_ms: i64,
) -> Result<Array, JsError> {
    let (summary, index) =
        super::publish_step(models_dev, index, evidence, upstream_observed_at_ms)
            .map_err(js_error)?;
    let result = Array::new_with_length(2);
    result.set(0, Uint8Array::from(summary.as_slice()).into());
    result.set(
        1,
        index.map_or(JsValue::NULL, |bytes| {
            Uint8Array::from(bytes.as_slice()).into()
        }),
    );
    Ok(result)
}

#[wasm_bindgen]
pub fn index_from_snapshot(snapshot: &[u8]) -> Result<Vec<u8>, JsError> {
    super::index_from_snapshot(snapshot).map_err(js_error)
}

#[wasm_bindgen]
pub fn index_apply_change(index: &[u8], change: &[u8]) -> Result<Vec<u8>, JsError> {
    super::index_apply_change(index, change).map_err(js_error)
}

#[wasm_bindgen]
pub fn upstream_hash(index: &[u8]) -> Result<String, JsError> {
    super::upstream_hash(index).map_err(js_error)
}

#[wasm_bindgen]
pub fn envelope_signing_bytes(envelope_json: &[u8]) -> Result<Vec<u8>, JsError> {
    super::envelope_signing_bytes(envelope_json).map_err(js_error)
}

#[wasm_bindgen]
pub fn head_signing_bytes(head_json: &[u8]) -> Result<Vec<u8>, JsError> {
    super::head_signing_bytes(head_json).map_err(js_error)
}

#[wasm_bindgen]
pub fn build_release(
    summary: &[u8],
    curation_set_raw: &[u8],
    parent_envelope_or_empty: &[u8],
    release_number: u64,
    published_at_ms: i64,
    upstream_observed_at_ms: i64,
) -> Vec<u8> {
    artifacts::in_band(fusiform_release::build_release(
        summary,
        curation_set_raw,
        parent_envelope_or_empty,
        release_number,
        published_at_ms,
        upstream_observed_at_ms,
    ))
}

#[wasm_bindgen]
#[allow(clippy::too_many_arguments)] // The host passes each stored document separately.
pub fn snapshot_bytes(
    models_dev: &[u8],
    evidence: &[u8],
    envelope: &[u8],
    curation_set_raw: &[u8],
    signatures: &[u8],
    history_files: &[u8],
    history_signatures: &[u8],
    release_public_key: &[u8],
) -> Vec<u8> {
    artifacts::in_band(
        super::decode_evidence(evidence)
            .map_err(Into::into)
            .and_then(|evidence| {
                fusiform_publish::snapshot_bytes(fusiform_publish::SnapshotInputs {
                    models_dev,
                    evidence: &evidence,
                    envelope,
                    curation_set_raw,
                    signatures,
                    history_files,
                    history_signatures,
                    release_public_key,
                })
            }),
    )
}

#[wasm_bindgen]
pub fn snapshot_bytes_from_state(
    state: &[u8],
    envelope: &[u8],
    signatures: &[u8],
    history_files: &[u8],
    history_signatures: &[u8],
    release_public_key: &[u8],
) -> Vec<u8> {
    artifacts::in_band(fusiform_publish::snapshot_bytes_from_state(
        state,
        envelope,
        signatures,
        history_files,
        history_signatures,
        release_public_key,
    ))
}

#[wasm_bindgen]
pub fn snapshot_bytes_from_previous(
    previous_snapshot: &[u8],
    releases_jsonl: &[u8],
    history_files: &[u8],
    history_signatures: &[u8],
    release_public_key: &[u8],
) -> Vec<u8> {
    artifacts::in_band(super::snapshot_replay::snapshot_bytes_from_previous(
        previous_snapshot,
        releases_jsonl,
        history_files,
        history_signatures,
        release_public_key,
    ))
}

#[wasm_bindgen]
pub fn history_day_bytes(day: &[u8], releases_jsonl: &[u8]) -> Vec<u8> {
    let day = str::from_utf8(day)
        .map_err(|err| fusiform_release::ArtifactRefusal::new("invalid_day", err));
    artifacts::in_band(day.and_then(|day| fusiform_release::history_day_bytes(day, releases_jsonl)))
}

#[wasm_bindgen]
pub fn canonical_json(bytes: &[u8]) -> Result<Vec<u8>, JsError> {
    artifacts::canonical_json(bytes).map_err(js_error)
}

#[wasm_bindgen]
pub fn history_attestation_signing_bytes(body_json: &[u8]) -> Result<Vec<u8>, JsError> {
    artifacts::history_attestation_signing_bytes(body_json).map_err(js_error)
}
