use std::collections::BTreeMap;

use fusiform_core::{ChargeUnitEvidence, NormalizedCatalog};
use fusiform_release::{
    ArtifactRefusal, Hash, HistoryAttestation, HistoryAttestationBody, HistoryFile,
    ReleaseEnvelope, Signatures, UpstreamHasher, VerifyingKey, decode_curation,
    decode_release_public_key, fact_hash, state_hash, verify_envelope, verify_history_attestation,
    write_canonical_json,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

use crate::{parse_catalog, project_model, recovery::check_models, snapshot_reader::UniqueMap};

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
    build_snapshot(
        input.envelope,
        input.curation_set_raw,
        input.signatures,
        input.history_files,
        input.history_signatures,
        input.release_public_key,
        |out| {
            let catalog = parse_catalog(input.models_dev, input.evidence)
                .map_err(|err| ArtifactRefusal::new(err.class(), err))?;
            Ok(write_upstream(out, catalog))
        },
    )
}

/// Emit an attested snapshot from an existing state, such as an imported store.
/// Upstream values are decoded and written one fact at a time.
pub fn snapshot_bytes_from_state(
    state: &[u8],
    envelope: &[u8],
    signatures: &[u8],
    history_files: &[u8],
    history_signatures: &[u8],
    release_public_key: &[u8],
) -> Result<Vec<u8>, ArtifactRefusal> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StateParts<'a> {
        #[serde(borrow)]
        curation_set: &'a RawValue,
        #[serde(borrow)]
        upstream: &'a RawValue,
    }
    let state: StateParts<'_> = decode(state, "invalid_state")?;
    build_snapshot(
        envelope,
        state.curation_set.get().as_bytes(),
        signatures,
        history_files,
        history_signatures,
        release_public_key,
        |out| write_state_upstream(out, state.upstream),
    )
}

fn build_snapshot(
    envelope: &[u8],
    curation_set_raw: &[u8],
    release_signatures: &[u8],
    history_files: &[u8],
    history_signatures: &[u8],
    release_public_key: &[u8],
    write_upstream: impl FnOnce(&mut Vec<u8>) -> Result<Hash, ArtifactRefusal>,
) -> Result<Vec<u8>, ArtifactRefusal> {
    let envelope: ReleaseEnvelope = decode(envelope, "invalid_envelope")?;
    let signatures: Signatures = decode(release_signatures, "invalid_release_signature")?;
    let history_files: Vec<HistoryFile> = decode(history_files, "invalid_history_files")?;
    let pinned_keys = pinned_keys(release_public_key)?;
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
        signatures: decode(history_signatures, "invalid_history_signature")?,
    };
    verify_history_attestation(&attestation, &envelope, &pinned_keys)
        .map_err(|err| ArtifactRefusal::new("invalid_history_signature", err))?;

    let (curation, curation_hash) = decode_curation(curation_set_raw)?;
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
    let mut out = b"{\"envelope\":".to_vec();
    write_value(&mut out, &envelope);
    out.extend_from_slice(b",\"history_attestation\":");
    write_value(&mut out, attestation);
    out.extend_from_slice(b",\"signatures\":");
    // Keep the host's signature object, not a reconstruction from the two
    // fields required for verification.
    write_value(
        &mut out,
        decode::<Value>(release_signatures, "invalid_release_signature")?,
    );
    out.extend_from_slice(b",\"state\":{\"curation_set\":");
    write_value(&mut out, curation);
    out.extend_from_slice(b",\"upstream\":");
    let upstream_hash = write_upstream(&mut out)?;
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

fn write_state_upstream(out: &mut Vec<u8>, upstream: &RawValue) -> Result<Hash, ArtifactRefusal> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Provider<'a> {
        #[serde(borrow)]
        provider: &'a RawValue,
        #[serde(borrow)]
        models: &'a RawValue,
    }

    let mut digest = UpstreamHasher::default();
    let UniqueMap(providers) = decode(upstream.get().as_bytes(), "invalid_state")?;
    out.push(b'{');
    for (p, (id, raw)) in providers.into_iter().enumerate() {
        let Provider { provider, models } = decode_state_part(raw)?;
        let UniqueMap(models) = decode_state_part::<UniqueMap<&RawValue>>(models)?;

        // Hash order starts with provider-level facts; JSON key order puts
        // them after the models. Buffer only that provider's top-level facts.
        let mut provider_bytes = Vec::new();
        write_state_facts(&mut provider_bytes, &mut digest, &id, None, provider)?;
        write_key(out, &id, p != 0);
        out.extend_from_slice(b"{\"models\":{");
        let mut model_shapes = Vec::with_capacity(models.len());
        for (m, (model, raw)) in models.iter().enumerate() {
            write_key(out, model, m != 0);
            let empty = write_state_facts(out, &mut digest, &id, Some(model), raw)?;
            model_shapes.push((model.as_str(), empty));
        }
        check_models(&id, model_shapes.into_iter())
            .map_err(|err| ArtifactRefusal::new("invalid_state", err))?;
        out.extend_from_slice(b"},\"provider\":");
        out.extend_from_slice(&provider_bytes);
        out.push(b'}');
    }
    out.push(b'}');
    Ok(digest.finish())
}

fn write_state_facts(
    out: &mut Vec<u8>,
    digest: &mut UpstreamHasher,
    provider: &str,
    model: Option<&str>,
    raw: &RawValue,
) -> Result<bool, ArtifactRefusal> {
    let UniqueMap(facts) = decode_state_part::<UniqueMap<&RawValue>>(raw)?;
    let empty = facts.is_empty();
    out.push(b'{');
    for (f, (key, raw)) in facts.into_iter().enumerate() {
        let value: Value = decode_state_part(raw)?;
        digest.add_sorted((provider, model, &key), fact_hash(&value));
        write_key(out, &key, f != 0);
        write_canonical_json(&mut *out, &value).expect("writing to a Vec cannot fail");
    }
    out.push(b'}');
    Ok(empty)
}

fn decode_state_part<'a, T: Deserialize<'a>>(raw: &'a RawValue) -> Result<T, ArtifactRefusal> {
    decode(raw.get().as_bytes(), "invalid_state")
}
