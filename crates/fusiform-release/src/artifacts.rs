use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    Change, ChangeOp, CurationSet, Hash, ReleaseEnvelope, ReleaseFormat, canonical_json, fact_hash,
    state_hash,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactRefusal {
    pub class: &'static str,
    pub detail: String,
}

impl ArtifactRefusal {
    pub fn new(class: &'static str, detail: impl fmt::Display) -> Self {
        Self {
            class,
            detail: detail.to_string(),
        }
    }
}

impl fmt::Display for ArtifactRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for ArtifactRefusal {}

/// Parse and hash the curation as stored, without normalizing numbers through
/// a floating-point representation or silently dropping unrecognized fields.
pub fn decode_curation(raw: &[u8]) -> Result<(CurationSet, Hash), ArtifactRefusal> {
    let curation: CurationSet =
        serde_json::from_slice(raw).map_err(|err| ArtifactRefusal::new("invalid_curation", err))?;
    let value: Value =
        serde_json::from_slice(raw).map_err(|err| ArtifactRefusal::new("invalid_curation", err))?;
    let hash = fact_hash(&value);
    if curation.curation_hash() != hash {
        return Err(ArtifactRefusal::new(
            "invalid_curation",
            "stored curation does not round-trip in the release schema",
        ));
    }
    Ok((curation, hash))
}

/// Build the canonical unsigned release from a successful `publish_step`
/// summary and the raw curation document. The caller signs the envelope.
pub fn build_release(
    summary: &[u8],
    curation_set_raw: &[u8],
    parent_envelope_or_empty: &[u8],
    release_number: u64,
    published_at_ms: i64,
    upstream_observed_at_ms: i64,
) -> Result<Vec<u8>, ArtifactRefusal> {
    #[derive(Deserialize)]
    struct Summary {
        ops: Vec<ChangeOp>,
        upstream_hash: Hash,
    }

    let summary: Summary = serde_json::from_slice(summary)
        .map_err(|err| ArtifactRefusal::new("invalid_summary", err))?;
    if !summary
        .ops
        .windows(2)
        .all(|pair| pair[0].key() < pair[1].key())
    {
        return Err(ArtifactRefusal::new(
            "invalid_summary",
            "upstream operations are not strictly sorted",
        ));
    }
    let parent: Option<ReleaseEnvelope> = if parent_envelope_or_empty.is_empty() {
        None
    } else {
        Some(
            serde_json::from_slice(parent_envelope_or_empty)
                .map_err(|err| ArtifactRefusal::new("invalid_parent", err))?,
        )
    };
    let (curation, curation_hash) = decode_curation(curation_set_raw)?;
    let version = curation.version;
    let upstream_hash = summary.upstream_hash;
    let mut envelope = ReleaseEnvelope {
        format: ReleaseFormat::V1,
        release: release_number,
        parent: parent.as_ref().map(|prior| prior.release),
        published_at_ms,
        upstream_observed_at_ms,
        upstream_hash,
        curation_version: version,
        curation_hash,
        state_hash: state_hash(curation_hash, upstream_hash),
        change_hash: Hash::from_bytes([0; 32]),
    };
    envelope
        .validate_parent()
        .map_err(|err| ArtifactRefusal::new("invalid_parent", err))?;

    let unchanged = parent
        .as_ref()
        .filter(|prior| prior.curation_version == version);
    if let Some(prior) = unchanged {
        if prior.curation_hash != curation_hash {
            return Err(ArtifactRefusal::new(
                "curation_version_reused",
                "curation changed without a version change",
            ));
        }
        if summary.ops.is_empty() {
            return Err(ArtifactRefusal::new(
                "nothing_to_publish",
                "upstream and curation are unchanged",
            ));
        }
    }
    let change = Change {
        upstream: summary.ops,
        curation_set: unchanged.is_none().then_some(curation),
    };
    envelope.change_hash = change.change_hash();
    Ok(canonical_json(
        &json!({"envelope": envelope, "change": change}),
    ))
}
