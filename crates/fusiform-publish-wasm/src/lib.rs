#![forbid(unsafe_code)]
#![cfg(any(target_arch = "wasm32", test))]

use std::fmt::Display;

use fusiform_core::ChargeUnitEvidence;
use fusiform_curate::input_audio::{
    add_output_evidence, evidence_from, load_for_schema, load_reporting_rejects,
};
use fusiform_publish::{FactIndex, ReplayRefusal};
use fusiform_release::{Change, ReleaseEnvelope, Signatures, canonical_json, write_canonical_json};
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue};

#[cfg(target_arch = "wasm32")]
mod artifacts;
#[cfg(target_arch = "wasm32")]
mod web;

#[derive(Debug, Serialize)]
struct Failure {
    class: &'static str,
    detail: String,
}

impl Failure {
    fn new(class: &'static str, detail: impl Display) -> Self {
        Self {
            class,
            detail: detail.to_string(),
        }
    }
}

type PublishResult = (Vec<u8>, Option<Vec<u8>>);

fn refuse(failure: Failure) -> PublishResult {
    (
        canonical_json(&json!({"conflicts": [], "ops": [], "refusal": failure})),
        None,
    )
}

fn publish_step(
    models_dev: &[u8],
    index: &[u8],
    evidence: &[u8],
) -> Result<PublishResult, Failure> {
    if index.is_empty() {
        return Ok(refuse(Failure::new(
            "empty_index",
            "recover the index from a published snapshot before polling",
        )));
    }
    let previous = decode_index(index)?;
    let base_upstream_hash = previous.upstream_hash();
    let evidence = decode_evidence(evidence)?;
    let step = match fusiform_publish::publish_step(models_dev, &previous, &evidence) {
        Ok(step) => step,
        Err(err) => return Ok(refuse(Failure::new(err.class(), err))),
    };
    drop(previous);

    let conflicts: Vec<_> = step
        .conflicts
        .into_iter()
        .map(|conflict| {
            json!({
                "provider": conflict.provider,
                "model": conflict.model,
                "fact": conflict.fact.as_str(),
                "values": conflict.values,
            })
        })
        .collect();
    let mut summary = br#"{"base_upstream_hash":"#.to_vec();
    write_json(&mut summary, base_upstream_hash);
    summary.extend_from_slice(br#","conflicts":"#);
    write_json(&mut summary, conflicts);
    summary.extend_from_slice(b",\"model_count\":");
    write_json(&mut summary, step.model_count);
    // Ops are already sorted. Consume them one at a time so the encoder
    // never holds a second tree for the whole diff.
    summary.extend_from_slice(b",\"ops\":[");
    for (i, op) in step.ops.into_iter().enumerate() {
        if i != 0 {
            summary.push(b',');
        }
        write_json(&mut summary, op);
    }
    summary.extend_from_slice(b"],\"upstream_hash\":");
    write_json(&mut summary, step.upstream_hash);
    summary.push(b'}');
    Ok((summary, Some(step.index.to_bytes())))
}

fn write_json(out: &mut Vec<u8>, value: impl Serialize) {
    let value = serde_json::to_value(value).expect("publish summary fields are JSON");
    write_canonical_json(out, &value).expect("writing to a Vec cannot fail");
}

fn decode_index(bytes: &[u8]) -> Result<FactIndex, Failure> {
    FactIndex::from_bytes(bytes).map_err(|err| Failure::new("invalid_index", err))
}

fn decode_evidence(bytes: &[u8]) -> Result<ChargeUnitEvidence, Failure> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Evidence<'a> {
        #[serde(rename = "input-audio-per-token", borrow)]
        input: &'a RawValue,
        #[serde(rename = "output-audio-per-token", borrow)]
        output: &'a RawValue,
    }

    let evidence: Evidence<'_> = decode_json(bytes, "invalid_evidence")?;
    let (input, input_rejects) = load_reporting_rejects(evidence.input.get());
    let (output, output_rejects) =
        load_for_schema(evidence.output.get(), "output-audio-per-token/v1");
    if !input_rejects.is_empty() || !output_rejects.is_empty() {
        return Err(Failure::new(
            "invalid_evidence",
            [input_rejects, output_rejects].concat().join("; "),
        ));
    }
    Ok(add_output_evidence(evidence_from(&input), &output))
}

fn index_from_snapshot(bytes: &[u8]) -> Result<Vec<u8>, Failure> {
    #[derive(Deserialize)]
    struct Snapshot<'a> {
        envelope: ReleaseEnvelope,
        #[serde(rename = "signatures")]
        _signatures: Signatures,
        #[serde(borrow)]
        state: SnapshotState<'a>,
    }

    // Borrow the upstream JSON so the streaming reader can hash one fact at a
    // time. The host verifies signatures and curation before calling recovery.
    #[derive(Deserialize)]
    struct SnapshotState<'a> {
        #[serde(borrow)]
        upstream: &'a RawValue,
        #[serde(rename = "curation_set", borrow)]
        _curation_set: &'a RawValue,
    }

    let snapshot: Snapshot<'_> = decode_json(bytes, "invalid_snapshot")?;
    let index = FactIndex::from_snapshot_upstream_reader(snapshot.state.upstream.get().as_bytes())
        .map_err(|err| Failure::new("invalid_snapshot", err))?;
    if index.upstream_hash() != snapshot.envelope.upstream_hash {
        return Err(Failure::new(
            "snapshot_hash_mismatch",
            format_args!(
                "snapshot upstream hashes to {}, expected {}",
                index.upstream_hash(),
                snapshot.envelope.upstream_hash,
            ),
        ));
    }
    Ok(index.to_bytes())
}

fn index_apply_change(bytes: &[u8], change: &[u8]) -> Result<Vec<u8>, Failure> {
    let mut index = decode_index(bytes)?;
    let change: Change = decode_json(change, "invalid_change")?;
    index.apply_change(&change).map_err(|err| {
        let class = match err.reason {
            ReplayRefusal::UnsortedOrDuplicate => "unsorted_or_duplicate",
            ReplayRefusal::AbsentFact => "absent_fact",
            ReplayRefusal::UnknownProvider => "unknown_provider",
            ReplayRefusal::OrphanProviderFacts => "orphan_provider_facts",
        };
        Failure::new(class, err)
    })?;
    Ok(index.to_bytes())
}

fn upstream_hash(index: &[u8]) -> Result<String, Failure> {
    Ok(decode_index(index)?.upstream_hash().to_string())
}

fn envelope_signing_bytes(bytes: &[u8]) -> Result<Vec<u8>, Failure> {
    fusiform_release::envelope_signing_bytes(&decode_json(bytes, "invalid_envelope")?)
        .map_err(|err| Failure::new("invalid_envelope", err))
}

fn head_signing_bytes(bytes: &[u8]) -> Result<Vec<u8>, Failure> {
    fusiform_release::head_signing_bytes(&decode_json(bytes, "invalid_head")?)
        .map_err(|err| Failure::new("invalid_head", err))
}

fn decode_json<'a, T: Deserialize<'a>>(bytes: &'a [u8], class: &'static str) -> Result<T, Failure> {
    serde_json::from_slice(bytes).map_err(|err| Failure::new(class, err))
}

#[cfg(test)]
mod tests;
