use js_sys::{Array, Uint8Array};
use wasm_bindgen::prelude::*;

use super::Failure;

fn js_error(failure: Failure) -> JsError {
    JsError::new(&serde_json::to_string(&failure).expect("failure fields are strings"))
}

#[wasm_bindgen]
pub fn publish_step(models_dev: &[u8], index: &[u8], evidence: &[u8]) -> Result<Array, JsError> {
    let (summary, index) = super::publish_step(models_dev, index, evidence).map_err(js_error)?;
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
