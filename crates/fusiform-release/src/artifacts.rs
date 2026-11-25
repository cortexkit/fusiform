use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    Change, ChangeOp, CurationSet, Hash, ReleaseEnvelope, ReleaseFormat, fact_hash, state_hash,
    upstream_hash_sorted, validate_upstream_key, write_canonical_json,
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
        base_upstream_hash: Hash,
        ops: Vec<ChangeOp>,
        upstream_hash: Hash,
    }

    let summary: Summary = serde_json::from_slice(summary)
        .map_err(|err| ArtifactRefusal::new("invalid_summary", err))?;
    for op in &summary.ops {
        validate_upstream_key(op.key())
            .map_err(|err| ArtifactRefusal::new("invalid_summary", err))?;
    }
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
    let expected_base = parent.as_ref().map_or_else(
        || upstream_hash_sorted(std::iter::empty()),
        |prior| prior.upstream_hash,
    );
    if summary.base_upstream_hash != expected_base {
        return Err(ArtifactRefusal::new(
            "invalid_summary",
            "summary base_upstream_hash does not match the parent upstream hash",
        ));
    }
    if parent
        .as_ref()
        .is_some_and(|prior| !envelope.chronology_follows(prior, !summary.ops.is_empty()))
    {
        return Err(ArtifactRefusal::new(
            "invalid_release_times",
            "release times do not advance the verified chain",
        ));
    }

    if parent
        .as_ref()
        .is_some_and(|prior| version < prior.curation_version)
    {
        return Err(ArtifactRefusal::new(
            "curation_version_regressed",
            "curation version does not advance",
        ));
    }
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
    let mut out = br#"{"change":"#.to_vec();
    change
        .write_canonical(&mut out)
        .expect("writing to a Vec cannot fail");
    out.extend_from_slice(br#","envelope":"#);
    write_canonical_json(
        &mut out,
        &serde_json::to_value(envelope).expect("an envelope is JSON"),
    )
    .expect("writing to a Vec cannot fail");
    out.push(b'}');
    Ok(out)
}
