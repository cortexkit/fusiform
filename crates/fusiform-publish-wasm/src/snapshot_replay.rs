use std::{borrow::Cow, collections::BTreeMap, fmt, num::NonZeroU64};

use fusiform_release::{
    ArtifactRefusal, Change, Hash, HistoryAttestation, HistoryAttestationBody, ReleaseEnvelope,
    Signatures, UpstreamHasher, decode_curation, decode_release_public_key, fact_hash, state_hash,
    validate_history_files, validate_upstream_key, verify_envelope, verify_history_attestation,
    write_canonical_json,
};
use serde::{Deserialize, Serialize, de};
use serde_json::{Value, value::RawValue};

#[derive(Deserialize)]
struct Previous<'a> {
    envelope: ReleaseEnvelope,
    #[serde(borrow)]
    signatures: &'a RawValue,
    #[serde(borrow)]
    state: SnapshotState<'a>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotState<'a> {
    #[serde(borrow)]
    curation_set: &'a RawValue,
    #[serde(borrow)]
    upstream: Upstream<'a>,
}

#[derive(Deserialize)]
struct ReleaseLine<'a> {
    envelope: ReleaseEnvelope,
    #[serde(borrow)]
    signatures: &'a RawValue,
    #[serde(borrow)]
    change: &'a RawValue,
}

#[derive(Deserialize)]
struct RawChange<'a> {
    #[serde(borrow)]
    upstream: Vec<&'a RawValue>,
    #[serde(borrow)]
    curation_set: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Operation<'a> {
    op: Op,
    #[serde(borrow)]
    provider: Key<'a>,
    #[serde(default, borrow)]
    model: Key<'a>,
    #[serde(borrow)]
    fact: Key<'a>,
    #[serde(default, borrow, deserialize_with = "present_raw_value")]
    value: Option<&'a RawValue>,
}

fn present_raw_value<'de, D: de::Deserializer<'de>>(
    decoder: D,
) -> Result<Option<&'de RawValue>, D::Error> {
    Deserialize::deserialize(decoder).map(Some)
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Op {
    Set,
    Remove,
    SetProvider,
    RemoveProvider,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseKey {
    kid: String,
    public_key: String,
}

pub(super) fn snapshot_bytes_from_previous(
    previous_snapshot: &[u8],
    releases_jsonl: &[u8],
    history_files: &[u8],
    history_signatures: &[u8],
    release_public_key: &[u8],
) -> Result<Vec<u8>, ArtifactRefusal> {
    let key: ReleaseKey = decode(release_public_key, "invalid_release_public_key")?;
    let verifying_key = decode_release_public_key(&key.public_key)
        .map_err(|err| ArtifactRefusal::new("invalid_release_public_key", err))?;
    let pinned = BTreeMap::from([(key.kid, verifying_key)]);
    let verify = |envelope: &ReleaseEnvelope, signatures: &RawValue| {
        let signature: Signatures =
            decode(signatures.get().as_bytes(), "invalid_release_signature")?;
        verify_envelope(envelope, &signature, &pinned)
            .map_err(|err| ArtifactRefusal::new("invalid_release_signature", err))
    };

    let Previous {
        envelope: mut head,
        signatures,
        state,
    } = decode(previous_snapshot, "invalid_snapshot")?;
    verify(&head, signatures)?;
    let mut curation = state.curation_set;
    check_curation(curation, &head)?;
    check_state_hash(&head)?;
    let mut upstream = state.upstream;
    let mut digest = UpstreamHasher::default();
    visit_facts(&upstream, &mut digest, None)?;
    check_upstream_hash(digest.finish(), &head)?;

    let mut signatures = None;
    let lines = std::str::from_utf8(releases_jsonl)
        .map_err(|err| ArtifactRefusal::new("invalid_release", err))?;
    for line in lines.lines() {
        let next: ReleaseLine<'_> = decode(line.as_bytes(), "invalid_release")?;
        verify(&next.envelope, next.signatures)?;
        if next.envelope.parent != Some(head.release) {
            return Err(ArtifactRefusal::new(
                "invalid_parent",
                format_args!(
                    "release {} does not follow {}",
                    next.envelope.release, head.release
                ),
            ));
        }
        let raw = next.change.get().as_bytes();
        // Authenticate the canonical typed change before relying on its bytes.
        // Drop the decoded values before applying the operations as borrowed spans.
        let change: Change = decode(raw, "invalid_release")?;
        if change.change_hash() != next.envelope.change_hash {
            return Err(ArtifactRefusal::new(
                "change_hash_mismatch",
                "release change did not match its signed envelope",
            ));
        }
        if !next
            .envelope
            .chronology_follows(&head, !change.upstream.is_empty())
        {
            return Err(ArtifactRefusal::new(
                "invalid_release_times",
                "release times do not advance the verified chain",
            ));
        }
        for operation in &change.upstream {
            validate_upstream_key(operation.key())
                .map_err(|err| ArtifactRefusal::new("invalid_change", err))?;
        }
        if !change
            .upstream
            .windows(2)
            .all(|pair| pair[0].key() < pair[1].key())
        {
            return Err(ArtifactRefusal::new(
                "invalid_change",
                "upstream operations must be strictly ordered by fact key",
            ));
        }
        drop(change);
        let change: RawChange<'_> = decode(raw, "invalid_release")?;
        if let Some(replacement) = change.curation_set {
            if next.envelope.curation_version <= head.curation_version {
                return Err(ArtifactRefusal::new(
                    "snapshot_curation_mismatch",
                    "replacement curation must advance the version",
                ));
            }
            check_curation(replacement, &next.envelope)?;
            curation = replacement;
        } else if next.envelope.curation_hash != head.curation_hash
            || next.envelope.curation_version != head.curation_version
        {
            return Err(ArtifactRefusal::new(
                "snapshot_curation_mismatch",
                "curation changed without a replacement",
            ));
        }
        check_state_hash(&next.envelope)?;
        apply_ops(&mut upstream, change.upstream)?;
        head = next.envelope;
        signatures = Some(next.signatures);
    }
    let signatures = signatures.ok_or_else(|| {
        ArtifactRefusal::new(
            "empty_release_range",
            "expected a signed release after the previous snapshot",
        )
    })?;
    let attestation = HistoryAttestation {
        body: HistoryAttestationBody {
            release: NonZeroU64::new(head.release).expect("verified release number is nonzero"),
            state_hash: head.state_hash,
            history: decode(history_files, "invalid_history_files")?,
        },
        signatures: decode(history_signatures, "invalid_history_signature")?,
    };
    verify_history_attestation(&attestation, &head, &pinned)
        .map_err(|err| ArtifactRefusal::new("invalid_history_signature", err))?;
    validate_history_files(&attestation.body.history)
        .map_err(|err| ArtifactRefusal::new("invalid_history_files", err))?;

    let mut out = Vec::with_capacity(previous_snapshot.len());
    out.extend_from_slice(b"{\"envelope\":");
    write_value(&mut out, &head);
    out.extend_from_slice(b",\"history_attestation\":");
    write_value(&mut out, attestation);
    out.extend_from_slice(b",\"signatures\":");
    write_value(
        &mut out,
        decode::<Value>(signatures.get().as_bytes(), "invalid_release_signature")?,
    );
    out.extend_from_slice(b",\"state\":{\"curation_set\":");
    write_value(
        &mut out,
        decode::<Value>(curation.get().as_bytes(), "invalid_curation")?,
    );
    out.extend_from_slice(b",\"upstream\":");
    let mut digest = UpstreamHasher::default();
    visit_facts(&upstream, &mut digest, Some(&mut out))?;
    check_upstream_hash(digest.finish(), &head)?;
    out.extend_from_slice(b"}}");
    Ok(out)
}

fn decode<'a, T: Deserialize<'a>>(
    bytes: &'a [u8],
    class: &'static str,
) -> Result<T, ArtifactRefusal> {
    serde_json::from_slice(bytes).map_err(|err| ArtifactRefusal::new(class, err))
}

fn check_curation(raw: &RawValue, envelope: &ReleaseEnvelope) -> Result<(), ArtifactRefusal> {
    let (curation, hash) = decode_curation(raw.get().as_bytes())?;
    if curation.version != envelope.curation_version || hash != envelope.curation_hash {
        return Err(ArtifactRefusal::new(
            "snapshot_curation_mismatch",
            "curation version or hash does not match the envelope",
        ));
    }
    Ok(())
}

fn check_state_hash(envelope: &ReleaseEnvelope) -> Result<(), ArtifactRefusal> {
    if state_hash(envelope.curation_hash, envelope.upstream_hash) != envelope.state_hash {
        return Err(ArtifactRefusal::new(
            "snapshot_state_mismatch",
            "state hash does not match the envelope",
        ));
    }
    Ok(())
}

fn check_upstream_hash(hash: Hash, envelope: &ReleaseEnvelope) -> Result<(), ArtifactRefusal> {
    if hash != envelope.upstream_hash {
        return Err(ArtifactRefusal::new(
            "snapshot_hash_mismatch",
            format_args!(
                "snapshot upstream hashes to {hash}, expected {}",
                envelope.upstream_hash
            ),
        ));
    }
    Ok(())
}

// Keys and values normally borrow the input buffers. A key with JSON escapes
// owns only its decoded string; a fact value is parsed only when hashed or written.
#[derive(Default, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
struct Key<'a>(#[serde(borrow)] Cow<'a, str>);

impl std::borrow::Borrow<str> for Key<'_> {
    fn borrow(&self) -> &str {
        &self.0
    }
}

struct Unique<'a, V>(BTreeMap<Key<'a>, V>);

type Facts<'a> = Unique<'a, &'a RawValue>;
type Upstream<'a> = Unique<'a, Provider<'a>>;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Provider<'a> {
    #[serde(borrow)]
    provider: Facts<'a>,
    #[serde(borrow)]
    models: Unique<'a, Facts<'a>>,
}

impl<V> Default for Unique<'_, V> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<'de: 'a, 'a, V: Deserialize<'de>> Deserialize<'de> for Unique<'a, V> {
    fn deserialize<D: de::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        struct MapVisitor<T>(std::marker::PhantomData<T>);
        impl<'de, K: Deserialize<'de> + Ord, V: Deserialize<'de>> de::Visitor<'de> for MapVisitor<(K, V)> {
            type Value = BTreeMap<K, V>;

            fn expecting(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
                out.write_str("an object with unique keys")
            }

            fn visit_map<A: de::MapAccess<'de>>(
                self,
                mut entries: A,
            ) -> Result<Self::Value, A::Error> {
                let mut map = BTreeMap::new();
                while let Some((key, value)) = entries.next_entry()? {
                    if map.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate state key"));
                    }
                }
                Ok(map)
            }
        }
        decoder
            .deserialize_map(MapVisitor(std::marker::PhantomData))
            .map(Self)
    }
}

fn apply_ops<'a>(
    upstream: &mut Upstream<'a>,
    ops: Vec<&'a RawValue>,
) -> Result<(), ArtifactRefusal> {
    for raw in ops {
        let Operation {
            op,
            provider,
            model,
            fact,
            value,
        } = decode(raw.get().as_bytes(), "invalid_change")?;
        let absent =
            || ArtifactRefusal::new("invalid_change", "remove names a fact that does not exist");
        match op {
            Op::Set | Op::SetProvider => {
                let value = value.expect("verified set carries a value");
                let provider = upstream.0.entry(provider).or_default();
                let facts = match op {
                    Op::Set => provider.models.0.entry(model).or_default(),
                    _ => &mut provider.provider,
                };
                facts.0.insert(fact, value);
            }
            Op::Remove | Op::RemoveProvider => {
                let provider = upstream.0.get_mut(provider.0.as_ref()).ok_or_else(absent)?;
                let facts = match op {
                    Op::Remove => provider
                        .models
                        .0
                        .get_mut(model.0.as_ref())
                        .ok_or_else(absent)?,
                    _ => &mut provider.provider,
                };
                facts.0.remove(fact.0.as_ref()).ok_or_else(absent)?;
                if matches!(op, Op::Remove) && facts.0.is_empty() {
                    provider.models.0.remove(model.0.as_ref());
                }
            }
        }
    }
    // A release may set provider facts before it creates that provider's first
    // model. Check the shape after the whole release has been applied.
    if upstream
        .0
        .values()
        .any(|row| row.models.0.is_empty() && !row.provider.0.is_empty())
    {
        return Err(ArtifactRefusal::new(
            "invalid_change",
            "provider facts have no models",
        ));
    }
    upstream.0.retain(|_, row| !row.models.0.is_empty());
    Ok(())
}

fn visit_facts(
    upstream: &Upstream<'_>,
    digest: &mut UpstreamHasher,
    mut out: Option<&mut Vec<u8>>,
) -> Result<(), ArtifactRefusal> {
    if let Some(out) = &mut out {
        out.push(b'{');
    }
    for (p, (provider, row)) in upstream.0.iter().enumerate() {
        if row.models.0.is_empty() {
            return Err(ArtifactRefusal::new(
                "invalid_state",
                "snapshot provider has no models",
            ));
        }
        let id = provider.0.as_ref();
        // Provider facts hash before models but are written afterward in JSON.
        let mut provider_bytes = Vec::new();
        visit_fact_map(
            &row.provider,
            id,
            None,
            digest,
            out.as_ref().map(|_| &mut provider_bytes),
        )?;
        if let Some(out) = &mut out {
            write_key(out, id, p != 0);
            out.extend_from_slice(b"{\"models\":{");
        }
        for (m, (model, facts)) in row.models.0.iter().enumerate() {
            if facts.0.is_empty() {
                return Err(ArtifactRefusal::new(
                    "invalid_state",
                    "snapshot model has no facts",
                ));
            }
            let model = model.0.as_ref();
            if let Some(out) = &mut out {
                write_key(out, model, m != 0);
            }
            visit_fact_map(facts, id, Some(model), digest, out.as_deref_mut())?;
        }
        if let Some(out) = &mut out {
            out.extend_from_slice(b"},\"provider\":");
            out.extend_from_slice(&provider_bytes);
            out.push(b'}');
        }
    }
    if let Some(out) = out {
        out.push(b'}');
    }
    Ok(())
}

fn visit_fact_map(
    facts: &Facts<'_>,
    provider: &str,
    model: Option<&str>,
    digest: &mut UpstreamHasher,
    mut out: Option<&mut Vec<u8>>,
) -> Result<(), ArtifactRefusal> {
    if let Some(out) = &mut out {
        out.push(b'{');
    }
    for (i, (fact, raw)) in facts.0.iter().enumerate() {
        let fact = fact.0.as_ref();
        let key = (provider, model, fact);
        validate_upstream_key(key).map_err(|err| ArtifactRefusal::new("invalid_state", err))?;
        let value: Value = decode(raw.get().as_bytes(), "invalid_state")?;
        digest.add_sorted(key, fact_hash(&value));
        if let Some(out) = &mut out {
            write_key(out, fact, i != 0);
            write_canonical_json(out, &value).expect("writing to a Vec cannot fail");
        }
    }
    if let Some(out) = out {
        out.push(b'}');
    }
    Ok(())
}

fn write_key(out: &mut Vec<u8>, key: &str, comma: bool) {
    if comma {
        out.push(b',');
    }
    serde_json::to_writer(&mut *out, key).expect("writing to a Vec cannot fail");
    out.push(b':');
}

fn write_value(out: &mut Vec<u8>, value: impl Serialize) {
    let value = serde_json::to_value(value).expect("snapshot fields are JSON");
    write_canonical_json(out, &value).expect("writing to a Vec cannot fail");
}
