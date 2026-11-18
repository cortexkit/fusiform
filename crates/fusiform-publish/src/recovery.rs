use std::fmt;

use fusiform_release::{Change, ChangeOp, Upstream, fact_hash, upstream_facts};

use crate::FactIndex;
use crate::index::IndexBuilder;

impl FactIndex {
    /// Rebuild after losing an index without producing operations. Begin with
    /// a verified published snapshot, replay each subsequent verified release
    /// through [`Self::apply_change`], then compare [`Self::upstream_hash`]
    /// against the head release's envelope `upstream_hash` before polling.
    /// The host owns fetching the releases and checking their signatures.
    pub fn from_snapshot_upstream(upstream: &Upstream) -> Result<Self, SnapshotIndexError> {
        for (provider, facts) in upstream {
            check_models(
                provider,
                facts
                    .models
                    .iter()
                    .map(|(model, facts)| (model.as_str(), facts.is_empty())),
            )?;
        }
        let mut builder = IndexBuilder::default();
        for (key, value) in upstream_facts(upstream) {
            builder.add_sorted(key, fact_hash(value));
        }
        Ok(builder.finish())
    }

    /// Replay one verified release's upstream changes without retaining values.
    /// A set adds or replaces a key; a remove must name an existing key.
    /// Operations must be strictly ordered and unique. A provider-level set
    /// needs an existing provider or a model set for that provider in the same
    /// change. Removing a provider's last model must also remove all of its
    /// provider facts. The index remains untouched on any refusal.
    pub fn apply_change(&mut self, change: &Change) -> Result<(), ReplayError> {
        if change.upstream.is_empty() {
            return Ok(());
        }

        let mut held = self.facts().peekable();
        let mut builder = IndexBuilder::default();
        let mut previous_key = None;
        let mut base = 0;
        for group in change
            .upstream
            .chunk_by(|left, right| left.key().0 == right.key().0)
        {
            let provider = group[0].key().0;
            while let Some((key, hash)) = held.next_if(|(key, _)| key.0 < provider) {
                builder.add_sorted(key, hash);
            }
            let provider_exists = held.peek().is_some_and(|(key, _)| key.0 == provider);
            let adds_model_fact = group.iter().any(|op| matches!(op, ChangeOp::Set { .. }));

            for (offset, op) in group.iter().enumerate() {
                let key = op.key();
                let err = |reason| ReplayError {
                    operation: base + offset,
                    reason,
                };
                if previous_key.is_some_and(|prior| prior >= key) {
                    return Err(err(ReplayRefusal::UnsortedOrDuplicate));
                }
                previous_key = Some(key);
                while let Some((key, hash)) = held.next_if(|(old, _)| *old < key) {
                    builder.add_sorted(key, hash);
                }
                let exists = held.next_if(|(old, _)| *old == key).is_some();

                match op {
                    ChangeOp::Set { value, .. } => builder.add_sorted(key, fact_hash(value)),
                    ChangeOp::SetProvider { value, .. } => {
                        if !provider_exists && !adds_model_fact {
                            return Err(err(ReplayRefusal::UnknownProvider));
                        }
                        builder.add_sorted(key, fact_hash(value));
                    }
                    ChangeOp::Remove { .. } | ChangeOp::RemoveProvider { .. } => {
                        if !exists {
                            return Err(err(ReplayRefusal::AbsentFact));
                        }
                    }
                }
            }

            while let Some((key, hash)) = held.next_if(|(key, _)| key.0 == provider) {
                builder.add_sorted(key, hash);
            }
            if builder.has_orphan_provider_facts(provider) {
                // The final model removal leaves the provider facts orphaned.
                let offset = group
                    .iter()
                    .rposition(|op| matches!(op, ChangeOp::Remove { .. }))
                    .expect("a provider that had a model now has none");
                return Err(ReplayError {
                    operation: base + offset,
                    reason: ReplayRefusal::OrphanProviderFacts,
                });
            }
            base += group.len();
        }

        for (key, hash) in held {
            builder.add_sorted(key, hash);
        }
        let replacement = builder.finish();
        *self = replacement;
        Ok(())
    }
}

pub(crate) fn check_models<'a>(
    provider: &str,
    mut models: impl ExactSizeIterator<Item = (&'a str, bool)>,
) -> Result<(), SnapshotIndexError> {
    if models.len() == 0 {
        return Err(SnapshotIndexError::EmptyProvider(provider.to_owned()));
    }
    if let Some((model, _)) = models.find(|(_, is_empty)| *is_empty) {
        return Err(SnapshotIndexError::EmptyModel {
            provider: provider.to_owned(),
            model: model.to_owned(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotIndexError {
    EmptyProvider(String),
    EmptyModel { provider: String, model: String },
}

impl fmt::Display for SnapshotIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyProvider(provider) => {
                write!(f, "snapshot provider {provider:?} has no models")
            }
            Self::EmptyModel { provider, model } => {
                write!(f, "snapshot model {provider:?}/{model:?} has no facts")
            }
        }
    }
}

impl std::error::Error for SnapshotIndexError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayError {
    /// The zero-based position in `change.upstream`.
    pub operation: usize,
    pub reason: ReplayRefusal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayRefusal {
    UnsortedOrDuplicate,
    AbsentFact,
    UnknownProvider,
    OrphanProviderFacts,
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self.reason {
            ReplayRefusal::UnsortedOrDuplicate => "operations must be strictly ordered by fact key",
            ReplayRefusal::AbsentFact => "remove names a fact that does not exist",
            ReplayRefusal::UnknownProvider => {
                "set_provider names a provider with no existing or incoming models"
            }
            ReplayRefusal::OrphanProviderFacts => {
                "removing the last model must also remove the provider facts"
            }
        };
        write!(f, "upstream operation {}: {reason}", self.operation)
    }
}

impl std::error::Error for ReplayError {}
