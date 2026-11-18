use std::collections::BTreeMap;

use fusiform_core::{ChargeUnitEvidence, NormalizedCatalog};
use fusiform_release::{
    ArtifactRefusal, Hash, HistoryAttestation, HistoryAttestationBody, HistoryFile,
    ReleaseEnvelope, Signatures, UpstreamHasher, VerifyingKey, decode_curation,
    decode_release_public_key, fact_hash, state_hash, verify_envelope, verify_history_attestation,
    write_canonical_json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{parse_catalog, project_model};

pub struct SnapshotInputs<'a> {
    pub models_dev: &'a [u8],
    pub evidence: &'a ChargeUnitEvidence,
    pub envelope: &'a [u8],
    pub curation_set_raw: &'a [u8],
    pub signatures: &'a [u8],
    pub history_files: &'a [u8],
    pub history_signatures: &'a [u8],
    pub release_public_key: &'a [u8],
}

/// Emit canonical snapshot bytes for a signed release and signed history list.
/// Only one model's projected values are held at a time, not a full upstream
/// value tree in addition to the normalized catalog and output bytes.
pub fn snapshot_bytes(input: SnapshotInputs<'_>) -> Result<Vec<u8>, ArtifactRefusal> {
    let envelope: ReleaseEnvelope = decode(input.envelope, "invalid_envelope")?;
    let signatures: Signatures = decode(input.signatures, "invalid_release_signature")?;
    let history_files: Vec<HistoryFile> = decode(input.history_files, "invalid_history_files")?;
    let pinned_keys = pinned_keys(input.release_public_key)?;
    verify_envelope(&envelope, &signatures, &pinned_keys)
        .map_err(|err| ArtifactRefusal::new("invalid_release_signature", err))?;
    let attestation = HistoryAttestation {
        body: HistoryAttestationBody {
            release: envelope
                .release
                .try_into()
                .expect("validated release number is nonzero"),
            state_hash: envelope.state_hash,
            history: history_files,
        },
        signatures: decode(input.history_signatures, "invalid_history_signature")?,
    };
    verify_history_attestation(&attestation, &envelope, &pinned_keys)
        .map_err(|err| ArtifactRefusal::new("invalid_history_signature", err))?;

    let (curation, curation_hash) = decode_curation(input.curation_set_raw)?;
    if curation.version != envelope.curation_version || curation_hash != envelope.curation_hash {
        return Err(ArtifactRefusal::new(
            "snapshot_curation_mismatch",
            "curation version or hash does not match the envelope",
        ));
    }
    if state_hash(curation_hash, envelope.upstream_hash) != envelope.state_hash {
        return Err(ArtifactRefusal::new(
            "snapshot_state_mismatch",
            "state hash does not match the envelope",
        ));
    }
    let catalog = parse_catalog(input.models_dev, input.evidence)
        .map_err(|err| ArtifactRefusal::new(err.class(), err))?;
    let mut out = b"{\"envelope\":".to_vec();
    write_value(&mut out, &envelope);
    out.extend_from_slice(b",\"history_attestation\":");
    write_value(&mut out, attestation);
    out.extend_from_slice(b",\"signatures\":");
    // Keep the host's signature object, not a reconstruction from the two
    // fields required for verification.
    write_value(
        &mut out,
        decode::<Value>(input.signatures, "invalid_release_signature")?,
    );
    out.extend_from_slice(b",\"state\":{\"curation_set\":");
    write_value(&mut out, curation);
    out.extend_from_slice(b",\"upstream\":");
    let upstream_hash = write_upstream(&mut out, catalog);
    if upstream_hash != envelope.upstream_hash {
        return Err(ArtifactRefusal::new(
            "snapshot_hash_mismatch",
            format_args!(
                "snapshot upstream hashes to {upstream_hash}, expected {}",
                envelope.upstream_hash
            ),
        ));
    }
    out.extend_from_slice(b"}}");
    Ok(out)
}

fn pinned_keys(bytes: &[u8]) -> Result<BTreeMap<String, VerifyingKey>, ArtifactRefusal> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Key {
        kid: String,
        public_key: String,
    }
    let key: Key = decode(bytes, "invalid_release_public_key")?;
    let public_key = decode_release_public_key(&key.public_key)
        .map_err(|err| ArtifactRefusal::new("invalid_release_public_key", err))?;
    Ok(BTreeMap::from([(key.kid, public_key)]))
}

fn decode<'a, T: Deserialize<'a>>(
    bytes: &'a [u8],
    class: &'static str,
) -> Result<T, ArtifactRefusal> {
    serde_json::from_slice(bytes).map_err(|err| ArtifactRefusal::new(class, err))
}

fn write_value(out: &mut Vec<u8>, value: impl Serialize) {
    let value = serde_json::to_value(value).expect("snapshot fields are JSON");
    write_canonical_json(out, &value).expect("writing to a Vec cannot fail");
}

fn write_key(out: &mut Vec<u8>, key: &str, comma: bool) {
    if comma {
        out.push(b',');
    }
    serde_json::to_writer(&mut *out, key).expect("writing a key to a Vec cannot fail");
    out.push(b':');
}

fn write_upstream(out: &mut Vec<u8>, catalog: NormalizedCatalog) -> Hash {
    let mut digest = UpstreamHasher::default();
    out.push(b'{');
    for (p, provider) in catalog
        .providers
        .into_iter()
        .filter(|p| !p.models.is_empty())
        .enumerate()
    {
        write_key(out, &provider.provider_id, p != 0);
        out.extend_from_slice(b"{\"models\":{");
        for (m, model) in provider.models.into_iter().enumerate() {
            write_key(out, &model.key.model_id, m != 0);
            out.push(b'{');
            for (f, (key, raw)) in project_model(&model).values.into_iter().enumerate() {
                let fact = key.as_str();
                let value = serde_json::from_str(&raw).expect("upstream storage facts are JSON");
                digest.add_sorted(
                    (&provider.provider_id, Some(&model.key.model_id), fact),
                    fact_hash(&value),
                );
                write_key(out, fact, f != 0);
                write_canonical_json(&mut *out, &value).expect("writing to a Vec cannot fail");
            }
            out.push(b'}');
        }
        out.extend_from_slice(b"},\"provider\":{}}");
    }
    out.push(b'}');
    digest.finish()
}
