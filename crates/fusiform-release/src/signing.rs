use std::{collections::BTreeMap, fmt};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Signer as _};
use serde::{Deserialize, Serialize};

pub use ed25519_dalek::{SigningKey, VerifyingKey};

use crate::{HeadBody, InvalidParent, ReleaseEnvelope, canonical_json};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signatures {
    pub kid: String,
    pub sig: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureError {
    InvalidParent(InvalidParent),
    UnknownKey,
    InvalidEncoding,
    BadSignature,
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidParent(err) => err.fmt(f),
            Self::UnknownKey => f.write_str("signature key id is not trusted"),
            Self::InvalidEncoding => {
                f.write_str("expected an unpadded base64url Ed25519 signature")
            }
            Self::BadSignature => f.write_str("Ed25519 signature did not verify"),
        }
    }
}

impl std::error::Error for SignatureError {}

/// Sign the canonical envelope, not the rest of the release or snapshot.
pub fn sign_envelope(
    envelope: &ReleaseEnvelope,
    kid: &str,
    key: &SigningKey,
) -> Result<Signatures, SignatureError> {
    Ok(sign(&envelope_signing_bytes(envelope)?, kid, key))
}

pub fn verify_envelope(
    envelope: &ReleaseEnvelope,
    signatures: &Signatures,
    pinned_keys: &BTreeMap<String, VerifyingKey>,
) -> Result<(), SignatureError> {
    verify(&envelope_signing_bytes(envelope)?, signatures, pinned_keys)
}

/// Sign the canonical `head` body, without its enclosing signature fields.
pub fn sign_head(head: &HeadBody, kid: &str, key: &SigningKey) -> Signatures {
    sign(&head_signing_bytes(head), kid, key)
}

pub fn verify_head(
    head: &HeadBody,
    signatures: &Signatures,
    pinned_keys: &BTreeMap<String, VerifyingKey>,
) -> Result<(), SignatureError> {
    verify(&head_signing_bytes(head), signatures, pinned_keys)
}

/// Canonical envelope bytes for external Ed25519 signers.
pub fn envelope_signing_bytes(envelope: &ReleaseEnvelope) -> Result<Vec<u8>, SignatureError> {
    envelope
        .validate_parent()
        .map_err(SignatureError::InvalidParent)?;
    Ok(signed_bytes(envelope))
}

/// Canonical head body bytes for external Ed25519 signers.
pub fn head_signing_bytes(head: &HeadBody) -> Vec<u8> {
    signed_bytes(head)
}

fn signed_bytes(body: &impl Serialize) -> Vec<u8> {
    canonical_json(&serde_json::to_value(body).expect("a valid signed body is JSON"))
}

fn sign(bytes: &[u8], kid: &str, key: &SigningKey) -> Signatures {
    Signatures {
        kid: kid.to_owned(),
        sig: URL_SAFE_NO_PAD.encode(key.sign(bytes).to_bytes()),
    }
}

fn verify(
    message: &[u8],
    signatures: &Signatures,
    pinned_keys: &BTreeMap<String, VerifyingKey>,
) -> Result<(), SignatureError> {
    let key = pinned_keys
        .get(&signatures.kid)
        .ok_or(SignatureError::UnknownKey)?;
    if signatures.sig.len() != 86 {
        return Err(SignatureError::InvalidEncoding);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(&signatures.sig)
        .map_err(|_| SignatureError::InvalidEncoding)?;
    let signature = Signature::from_slice(&bytes).map_err(|_| SignatureError::InvalidEncoding)?;
    key.verify_strict(message, &signature)
        .map_err(|_| SignatureError::BadSignature)
}
