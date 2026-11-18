use std::{collections::BTreeMap, fmt, num::NonZeroU64};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Number, Value};

use crate::{FactKeyRef, Hash, Signatures, fact_hash};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReleaseFormat {
    #[serde(rename = "ck-models-release/v1")]
    V1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct ReleaseEnvelope {
    pub format: ReleaseFormat,
    pub release: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub parent: Option<u64>,
    pub published_at_ms: i64,
    pub upstream_observed_at_ms: i64,
    pub upstream_hash: Hash,
    pub curation_version: u64,
    pub curation_hash: Hash,
    pub state_hash: Hash,
    pub change_hash: Hash,
}

impl ReleaseEnvelope {
    pub fn validate_parent(&self) -> Result<(), InvalidParent> {
        match (self.release, self.parent) {
            (1, None) => Ok(()),
            (n, Some(parent)) if n > 1 && parent == n - 1 => Ok(()),
            (release, parent) => Err(InvalidParent { release, parent }),
        }
    }
}

impl Serialize for ReleaseEnvelope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate_parent().map_err(serde::ser::Error::custom)?;
        Self::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for ReleaseEnvelope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let envelope = Self::deserialize(deserializer)?;
        envelope
            .validate_parent()
            .map_err(serde::de::Error::custom)?;
        Ok(envelope)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidParent {
    pub release: u64,
    pub parent: Option<u64>,
}

impl fmt::Display for InvalidParent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid parent {:?} for release {}",
            self.parent, self.release
        )
    }
}

impl std::error::Error for InvalidParent {}

// A nullable field is still required: omission is not a wire spelling of null.
fn required_nullable<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    Option::deserialize(deserializer)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub envelope: ReleaseEnvelope,
    pub change: Change,
    pub signatures: Signatures,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub upstream: Vec<ChangeOp>,
    #[serde(deserialize_with = "required_nullable")]
    pub curation_set: Option<CurationSet>,
}

impl Change {
    pub fn change_hash(&self) -> Hash {
        fact_hash(&serde_json::to_value(self).expect("a change is JSON"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ChangeOp {
    Set {
        provider: String,
        model: String,
        fact: String,
        value: Value,
    },
    Remove {
        provider: String,
        model: String,
        fact: String,
    },
    SetProvider {
        provider: String,
        fact: String,
        value: Value,
    },
    RemoveProvider {
        provider: String,
        fact: String,
    },
}

impl ChangeOp {
    pub fn key(&self) -> FactKeyRef<'_> {
        match self {
            Self::Set {
                provider,
                model,
                fact,
                ..
            }
            | Self::Remove {
                provider,
                model,
                fact,
            } => (provider, Some(model), fact),
            Self::SetProvider { provider, fact, .. } | Self::RemoveProvider { provider, fact } => {
                (provider, None, fact)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub head: HeadBody,
    /// The release key signs the canonical `head` body.
    pub signatures: Signatures,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadBody {
    pub release: NonZeroU64,
    pub state_hash: Hash,
    pub published_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub envelope: ReleaseEnvelope,
    pub state: State,
    /// Carried over from the release at this envelope.
    pub signatures: Signatures,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub upstream: Upstream,
    pub curation_set: CurationSet,
}

pub type Upstream = BTreeMap<String, ProviderFacts>;
pub type Facts = BTreeMap<String, Value>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderFacts {
    pub provider: Facts,
    pub models: BTreeMap<String, Facts>,
}

/// Iterate a snapshot's facts in upstream hash order, borrowing the values.
pub fn upstream_facts(upstream: &Upstream) -> impl Iterator<Item = (FactKeyRef<'_>, &Value)> {
    upstream.iter().flat_map(|(id, provider)| {
        let id = id.as_str();
        let provider_facts = provider
            .provider
            .iter()
            .map(move |(key, value)| ((id, None, key.as_str()), value));
        let model_facts = provider.models.iter().flat_map(move |(model, facts)| {
            facts
                .iter()
                .map(move |(key, value)| ((id, Some(model.as_str()), key.as_str()), value))
        });
        provider_facts.chain(model_facts)
    })
}

/// Compute the upstream hash of a full snapshot without flattening its values.
pub fn upstream_state_hash(upstream: &Upstream) -> Hash {
    crate::upstream_hash_sorted(
        upstream_facts(upstream).map(|(key, value)| (key, fact_hash(value))),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurationSet {
    pub version: u64,
    #[serde(deserialize_with = "required_nullable")]
    pub approved_at_ms: Option<i64>,
    pub approved_drafts: Vec<String>,
    pub grades: BTreeMap<String, Grade>,
    pub effort_grades: BTreeMap<String, BTreeMap<String, EffortGrade>>,
    pub kind_grades: BTreeMap<String, BTreeMap<String, Number>>,
    pub workload_efforts: BTreeMap<String, BTreeMap<String, WorkloadEffort>>,
    pub decommissioned: BTreeMap<String, Decommissioned>,
    pub vendor_facts: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grade {
    pub elo: Number,
    pub eq: Number,
    pub graded_effort: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortGrade {
    pub elo: Number,
    pub eq: Number,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadEffort {
    pub default: String,
    pub minimum: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decommissioned {
    pub reason: String,
}

impl CurationSet {
    pub fn curation_hash(&self) -> Hash {
        fact_hash(&serde_json::to_value(self).expect("a curation set is JSON"))
    }
}

pub fn state_hash(curation_hash: Hash, upstream_hash: Hash) -> Hash {
    fact_hash(&serde_json::json!({"curation": curation_hash, "upstream": upstream_hash}))
}
