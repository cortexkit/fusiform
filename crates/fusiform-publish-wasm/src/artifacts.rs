use fusiform_release::{
    ArtifactRefusal, HistoryAttestationBody, canonical_json as canonical_value,
};
use serde_json::json;

use crate::{Failure, decode_json};

impl From<Failure> for ArtifactRefusal {
    fn from(failure: Failure) -> Self {
        Self {
            class: failure.class,
            detail: failure.detail,
        }
    }
}

pub(crate) fn in_band(result: Result<Vec<u8>, ArtifactRefusal>) -> Vec<u8> {
    result.unwrap_or_else(|refusal| canonical_value(&json!({"refusal": refusal})))
}

pub(crate) fn canonical_json(bytes: &[u8]) -> Result<Vec<u8>, Failure> {
    Ok(canonical_value(&decode_json(bytes, "invalid_json")?))
}

pub(crate) fn history_attestation_signing_bytes(body_json: &[u8]) -> Result<Vec<u8>, Failure> {
    let body: HistoryAttestationBody = decode_json(body_json, "invalid_history_attestation")?;
    Ok(fusiform_release::history_attestation_signing_bytes(&body))
}
