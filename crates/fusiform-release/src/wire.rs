use std::{collections::BTreeMap, fmt, io::Write as _, num::NonZeroU64};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Number, Value};

use crate::{FactKeyRef, Hash, HistoryAttestation, Signatures, fact_hash};

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

    /// Publication time cannot go backwards. A release with new upstream
    /// facts also needs a strictly newer upstream observation.
    pub fn chronology_follows(&self, parent: &Self, has_upstream_changes: bool) -> bool {
        self.published_at_ms >= parent.published_at_ms
            && (self.upstream_observed_at_ms > parent.upstream_observed_at_ms
                || (!has_upstream_changes
                    && self.upstream_observed_at_ms == parent.upstream_observed_at_ms))
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
        use crate::{hash::DigestWriter, write_canonical_json};

        // The two v1 fields have a fixed canonical order. Encode one operation
        // at a time so hashing does not make a second tree of a whole release.
        let mut out = DigestWriter::default();
        let encode = |out: &mut DigestWriter, value| {
            write_canonical_json(out, &value).expect("digest writes cannot fail");
        };
        out.write_all(br#"{"curation_set":"#).unwrap();
        encode(
            &mut out,
            serde_json::to_value(&self.curation_set).expect("curation is JSON"),
        );
        out.write_all(br#","upstream":["#).unwrap();
        for (index, operation) in self.upstream.iter().enumerate() {
            if index != 0 {
                out.write_all(b",").unwrap();
            }
            encode(
                &mut out,
                serde_json::to_value(operation).expect("an operation is JSON"),
            );
        }
        out.write_all(b"]}").unwrap();
        out.finish()
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

/// Check that an operation names every component of its upstream key.
pub fn validate_upstream_key((provider, model, fact): FactKeyRef<'_>) -> Result<(), &'static str> {
    if provider.is_empty() || model == Some("") || fact.is_empty() {
        return Err(
            "upstream operations must name a provider and a fact; model ids cannot be empty",
        );
    }
    Ok(())
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
#[serde(remote = "Self")]
pub struct HeadBody {
    pub release: NonZeroU64,
    pub latest_snapshot: u64,
    pub state_hash: Hash,
    pub published_at_ms: i64,
}

impl HeadBody {
    pub fn validate_latest_snapshot(&self) -> Result<(), InvalidLatestSnapshot> {
        if (1..=self.release.get()).contains(&self.latest_snapshot) {
            Ok(())
        } else {
            Err(InvalidLatestSnapshot {
                release: self.release,
                latest_snapshot: self.latest_snapshot,
            })
        }
    }
}

impl Serialize for HeadBody {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate_latest_snapshot()
            .map_err(serde::ser::Error::custom)?;
        Self::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for HeadBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let head = Self::deserialize(deserializer)?;
        head.validate_latest_snapshot()
            .map_err(serde::de::Error::custom)?;
        Ok(head)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidLatestSnapshot {
    pub release: NonZeroU64,
    pub latest_snapshot: u64,
}

impl fmt::Display for InvalidLatestSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "latest_snapshot {} is outside 1..={}",
            self.latest_snapshot, self.release
        )
    }
}

impl std::error::Error for InvalidLatestSnapshot {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub envelope: ReleaseEnvelope,
    pub state: State,
    /// Carried over from the release at this envelope.
    pub signatures: Signatures,
    /// Optional only so the first format samples, written before history was
    /// attested, still decode for index recovery. A consumer that loads
    /// history from a snapshot must refuse one without an attestation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_attestation: Option<HistoryAttestation>,
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "nonempty_effort"
    )]
    pub graded_effort: Option<String>,
}

fn nonempty_effort<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    let effort = String::deserialize(deserializer)?;
    if effort.is_empty() {
        return Err(serde::de::Error::custom("an effort must not be empty"));
    }
    Ok(Some(effort))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortGrade {
    pub elo: Number,
    pub eq: Number,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkloadEffort {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub excluded: bool,
}

impl<'de> Deserialize<'de> for WorkloadEffort {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Fields {
            #[serde(default, deserialize_with = "nonempty_effort")]
            default: Option<String>,
            #[serde(default, deserialize_with = "nonempty_effort")]
            minimum: Option<String>,
            #[serde(default)]
            excluded: bool,
        }

        let Fields {
            default,
            minimum,
            excluded,
        } = Fields::deserialize(deserializer)?;
        if excluded && (default.is_some() || minimum.is_some()) || !excluded && default.is_none() {
            return Err(serde::de::Error::custom(
                "a workload must either specify a default effort or be excluded without any effort",
            ));
        }
        Ok(Self {
            default,
            minimum,
            excluded,
        })
    }
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
