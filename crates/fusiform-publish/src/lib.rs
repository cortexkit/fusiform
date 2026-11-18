#![forbid(unsafe_code)]

//! Compute a models.dev release diff without retaining any previous fact values.

mod index;
mod recovery;
mod snapshot_reader;

use std::fmt;

use fusiform_core::shrink_guard::implausible_shrink;
use fusiform_core::upstream_facts::{Existence, FactConflict, FactKey, facts_of};
use fusiform_core::{ChargeUnitEvidence, normalize_models_dev_with};
use fusiform_release::{ChangeOp, FactKeyRef, Hash, fact_hash};

use index::IndexBuilder;
pub use index::{FactIndex, IndexDecodeError};
pub use recovery::{ReplayError, ReplayRefusal, SnapshotIndexError};
pub use snapshot_reader::SnapshotReadError;

#[derive(Debug)]
pub struct PublishStep {
    pub ops: Vec<ChangeOp>,
    pub index: FactIndex,
    pub upstream_hash: Hash,
    pub model_count: usize,
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
) -> Result<PublishStep, PublishRefusal> {
    let catalog = normalize_models_dev_with(models_dev, evidence)
        .map_err(|err| PublishRefusal::Parse(err.to_string()))?
        .catalog;
    let model_count = catalog.model_count();
    if let Some(reason) = implausible_shrink(previous.model_count(), model_count) {
        return Err(PublishRefusal::Implausible(reason));
    }

    let mut previous_facts = previous.facts().peekable();
    let mut ops = Vec::new();
    let mut index = IndexBuilder::default();
    let mut conflicts = Vec::new();

    // The normalizer emits providers and models in key order. Visit one model's
    // facts at a time and merge against the old hashes to find ordered removals.
    for provider in catalog.providers {
        let provider_id = provider.provider_id;
        for model in provider.models {
            let projected = facts_of(&model);
            let mut facts = projected.values;
            conflicts.extend(projected.conflicts);
            let model_id = model.key.model_id;
            let existence = FactKey::existence();
            let at = facts.partition_point(|(key, _)| key < &existence);
            facts.insert(at, (existence, Existence::Present.as_json().to_owned()));
            for (fact, json) in facts {
                let key = (provider_id.as_str(), Some(model_id.as_str()), fact.as_str());
                let value = serde_json::from_str(&json).expect("upstream storage facts are JSON");
                let hash = fact_hash(&value);
                index.add_sorted(key, hash);

                while let Some((old, _)) = previous_facts.next_if(|(old, _)| *old < key) {
                    ops.push(remove(old));
                }
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
        conflicts,
    })
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
