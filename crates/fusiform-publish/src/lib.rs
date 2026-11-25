#![forbid(unsafe_code)]

//! Compute a models.dev release diff without retaining any previous fact values.

mod index;
mod recovery;
mod snapshot;
mod snapshot_reader;

use std::fmt;

use fusiform_core::normalize::{NormalizedModel, v2::Finding};
use fusiform_core::shrink_guard::implausible_shrink;
use fusiform_core::upstream_facts::{Existence, FactConflict, FactKey, ModelFacts, facts_of};
use fusiform_core::{ChargeUnitEvidence, NormalizeV2Outcome, Timestamp, normalize_models_dev_with};
use fusiform_release::{ChangeOp, FactKeyRef, Hash, fact_hash};

use index::IndexBuilder;
pub use index::{FactIndex, IndexDecodeError};
pub use recovery::{ReplayError, ReplayRefusal, SnapshotIndexError};
pub use snapshot::{SnapshotInputs, snapshot_bytes, snapshot_bytes_from_state};
pub use snapshot_reader::SnapshotReadError;

#[derive(Debug)]
pub struct PublishStep {
    pub ops: Vec<ChangeOp>,
    pub index: FactIndex,
    pub upstream_hash: Hash,
    pub model_count: usize,
    pub next_date_boundary_ms: Option<i64>,
    pub findings: Vec<Finding>,
    /// Existing fact hashes retained because the current claim was refused.
    pub held_fact_count: usize,
    /// Conflicting upstream prices that were omitted rather than guessed.
    pub conflicts: Vec<FactConflict>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishRefusal {
    Parse(String),
    Implausible(String),
}

impl PublishRefusal {
    pub const fn class(&self) -> &'static str {
        match self {
            Self::Parse(_) => "parse",
            Self::Implausible(_) => "implausible",
        }
    }
}

impl fmt::Display for PublishRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(detail) | Self::Implausible(detail) => f.write_str(detail),
        }
    }
}

impl std::error::Error for PublishRefusal {}

/// Ingest a complete models.dev response, using the selected curation version's
/// charge-unit evidence. Empty ops mean the upstream state did not change.
/// `previous` must describe the published head; rebuild a lost index with
/// [`FactIndex::from_snapshot_upstream`] and [`FactIndex::apply_change`].
pub fn publish_step(
    models_dev: &[u8],
    previous: &FactIndex,
    evidence: &ChargeUnitEvidence,
    upstream_observed_at_ms: i64,
) -> Result<PublishStep, PublishRefusal> {
    let NormalizeV2Outcome {
        catalog,
        next_date_boundary_ms,
        findings,
    } = parse_catalog(models_dev, evidence, upstream_observed_at_ms)?;
    let model_count = catalog.model_count();
    if let Some(reason) = implausible_shrink(previous.model_count(), model_count) {
        return Err(PublishRefusal::Implausible(reason));
    }

    let mut previous_facts = previous.facts().peekable();
    let mut ops = Vec::new();
    let mut index = IndexBuilder::default();
    let mut conflicts = Vec::new();
    let mut held_fact_count = 0;

    // The normalizer emits providers and models in key order. Visit one model's
    // facts at a time and merge against the old hashes to find ordered removals.
    for provider in catalog.providers {
        let provider_id = provider.provider_id;
        for model in provider.models {
            let projected = project_model(&model);
            conflicts.extend(projected.conflicts);
            let model_id = &model.key.model_id;
            for (fact, json) in projected.values {
                let key = (provider_id.as_str(), Some(model_id.as_str()), fact.as_str());
                let value = serde_json::from_str(&json).expect("upstream storage facts are JSON");
                let hash = fact_hash(&value);
                while let Some((old, held)) = previous_facts.next_if(|(old, _)| *old < key) {
                    held_fact_count +=
                        usize::from(hold_or_remove(&mut index, &mut ops, old, held, &model));
                }
                index.add_sorted(key, hash);
                if !previous_facts
                    .next_if(|(old, _)| *old == key)
                    .is_some_and(|(_, held)| held == hash)
                {
                    ops.push(ChangeOp::Set {
                        provider: provider_id.clone(),
                        model: model_id.clone(),
                        fact: fact.as_str().to_owned(),
                        value,
                    });
                }
            }
            // A refused field can sort after the last fact this model projects.
            while let Some((old, held)) =
                previous_facts.next_if(|(old, _)| old.0 == provider_id && old.1 == Some(model_id))
            {
                held_fact_count +=
                    usize::from(hold_or_remove(&mut index, &mut ops, old, held, &model));
            }
        }
    }
    ops.extend(previous_facts.map(|(key, _)| remove(key)));

    let index = index.finish();
    let upstream_hash = index.upstream_hash();

    Ok(PublishStep {
        ops,
        index,
        upstream_hash,
        model_count,
        next_date_boundary_ms,
        findings,
        held_fact_count,
        conflicts,
    })
}

fn hold_or_remove(
    index: &mut IndexBuilder,
    ops: &mut Vec<ChangeOp>,
    old: FactKeyRef<'_>,
    hash: Hash,
    model: &NormalizedModel,
) -> bool {
    if !model.refused.is_empty()
        && old.0 == model.key.provider_id
        && old.1 == Some(&model.key.model_id)
    {
        let key = FactKey::from_stored(old.2.to_owned());
        if model.refused.iter().any(|scope| scope.covers(&key)) {
            index.add_sorted(old, hash);
            return true;
        }
    }
    ops.push(remove(old));
    false
}

fn parse_catalog(
    models_dev: &[u8],
    evidence: &ChargeUnitEvidence,
    upstream_observed_at_ms: i64,
) -> Result<NormalizeV2Outcome, PublishRefusal> {
    let outcome =
        normalize_models_dev_with(models_dev, evidence, Timestamp(upstream_observed_at_ms))
            .map_err(|err| PublishRefusal::Parse(err.to_string()))?;
    for provider in &outcome.catalog.providers {
        if provider.provider_id.is_empty() {
            return Err(PublishRefusal::Parse("empty provider id".into()));
        }
        if provider
            .models
            .iter()
            .any(|model| model.key.model_id.is_empty())
        {
            return Err(PublishRefusal::Parse(format!(
                "empty model id for provider {:?}",
                provider.provider_id
            )));
        }
    }
    Ok(outcome)
}

fn project_model(model: &NormalizedModel) -> ModelFacts {
    let mut projected = facts_of(model);
    let existence = FactKey::existence();
    let at = projected
        .values
        .partition_point(|(key, _)| key < &existence);
    projected
        .values
        .insert(at, (existence, Existence::Present.as_json().to_owned()));
    projected
}

fn remove((provider, model, fact): FactKeyRef<'_>) -> ChangeOp {
    match model {
        Some(model) => ChangeOp::Remove {
            provider: provider.to_owned(),
            model: model.to_owned(),
            fact: fact.to_owned(),
        },
        None => ChangeOp::RemoveProvider {
            provider: provider.to_owned(),
            fact: fact.to_owned(),
        },
    }
}
