//! Reading models.dev, and refusing to guess.
//!
//! This module turns one upstream document into fusiform's domain. Everything
//! it does follows from a single stance: **the upstream is data, never
//! authority**. A field whose meaning cannot be established does not get a
//! plausible default — it gets an explicit unknown, or it stops the parse.
//!
//! The rules a consumer relies on are stated in `docs/wire-contract.md`; the
//! measurements that decide code here are noted at the rule.
//!
//! # What this module refuses to do
//!
//! - **Choose a transport.** Published client packages, endpoints and request
//!   shapes are served verbatim as upstream claims, not interpreted as renderer
//!   families. Consumers own that mapping and all transport quirks. Other
//!   request overrides, including a mode's `provider` block, remain unread;
//!   mode prices are served separately from the bytes that select the mode.
//! - **Infer a semantic from a field's name.** `tier.size` is read as a
//!   threshold only where `tier.type` says `context`; anywhere else it is a
//!   refused schedule rather than a guess.
//! - **Turn a missing value into a real one.** A zero limit is absence, a
//!   missing rate is `Unpriced`, and an unattributable charge unit is
//!   `UnknownChargeBasis` — never a number a consumer would spend against.

#[cfg(any(test, feature = "legacy-test-parser"))]
mod raw;
pub mod v2;

#[cfg(any(test, feature = "legacy-test-parser"))]
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::money::{
    decimal_str_to_minor_units, Amount, CurrencyCode, PolicyId, UnitProvenance, NANO_EXPONENT,
};
use crate::upstream_facts::{prefix, rate_key, FactKey};
use crate::{ChargeBasis, Modality, ModelKey, RateCondition, RateValue, SourceId};
#[cfg(any(test, feature = "legacy-test-parser"))]
use crate::{TokenClass, UnpricedReason};

#[cfg(any(test, feature = "legacy-test-parser"))]
pub use raw::{RawModel, RawProvider};
pub use v2::{normalize_models_dev_v2_with, NormalizeV2Outcome};

/// The currency models.dev rates are assumed to be in.
///
/// Asserted by fusiform under a named policy, not read from the document:
/// measured 2026-08-11, the payload carries no currency field anywhere. Every
/// amount produced here carries [`PolicyId::models_dev_usd_v1`] as its unit
/// provenance, so "why does the catalog say USD" resolves to an auditable rule
/// and a policy change can find every row that depended on it.
pub const MODELS_DEV_ASSUMED_CURRENCY: &str = "USD";

/// A rate as fusiform records it: what it is charged for, under what condition,
/// and what its value is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedRate {
    pub basis: ChargeBasis,
    pub condition: RateCondition,
    pub value: RateValue,
}

/// What a model can consume and emit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// What the model accepts. `None` when the upstream published no modality
    /// block at all, which is not the same claim as an empty list.
    pub input_modalities: Option<Vec<Modality>>,
    /// What the model produces. `None` when unpublished, per `input_modalities`.
    pub output_modalities: Option<Vec<Modality>>,
    pub reasoning: Option<bool>,
    /// Whether models.dev claims the model supports structured output. `None`
    /// means the upstream did not state a value; it is never defaulted to false.
    pub structured_output: Option<bool>,
    /// Whether the serving API accepts temperature; absence remains unknown.
    pub temperature: Option<bool>,
    /// The reasoning settings the model accepts, exactly as models.dev
    /// publishes them under `reasoning_options`.
    ///
    /// Carried verbatim rather than parsed into a closed type. A consumer maps
    /// its own reasoning levels onto these entries and refuses anything the
    /// model does not list, so an unknown entry type or a `null` inside
    /// `values` must reach it intact for IT to decide; a parser that dropped
    /// them would turn "fusiform did not understand this" into "the model
    /// does not accept this". Order is part of the claim (effort levels are
    /// listed in order) and is preserved.
    ///
    /// `None` when the key is absent — the upstream said nothing. `Some([])`
    /// is the upstream stating the model takes no options, a different claim.
    pub reasoning_options: Option<serde_json::Value>,
    pub tool_call: Option<bool>,
    pub attachment: Option<bool>,
}

/// Declared capacities, with absence preserved.
///
/// Every field is `Option` and a zero from the upstream becomes `None`,
/// unconditionally. Measured: 434 models declare `context: 0` and 1,138 declare
/// `output: 0`, including image and video models where a token limit is not a
/// meaningful quantity at all. A zero here means "not stated", and a consumer
/// that reads it as a real capacity computes a budget of nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Limits {
    pub context_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// A published connection claim and the level that supplied it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WireClaim {
    pub value: String,
    pub from: WireOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireOrigin {
    Model,
    Provider,
}

/// Connection claims, with no inferred defaults or transport interpretation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WireFacts {
    pub npm: Option<WireClaim>,
    pub api: Option<WireClaim>,
    pub shape: Option<WireClaim>,
}

/// One model, normalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedModel {
    pub key: ModelKey,
    pub display_name: Option<String>,
    /// The provider's published display label, repeated on each model row.
    pub provider_name: Option<String>,
    pub family: Option<String>,
    /// The upstream's cross-provider model identity, never inferred from row ids.
    pub canonical_id: Option<String>,
    /// The upstream's lifecycle label, including values fusiform has not seen.
    pub status: Option<String>,
    /// The upstream's sparse model-type label, not the curated `model.kind`.
    pub model_type: Option<String>,
    pub release_date: Option<String>,
    pub last_updated: Option<String>,
    pub knowledge_cutoff: Option<String>,
    pub open_weights: Option<bool>,
    pub capabilities: Capabilities,
    pub limits: Limits,
    pub wire: WireFacts,
    pub rates: Vec<NormalizedRate>,
    /// Refused upstream claims are not withdrawals, even when no value was
    /// decoded for their keys.
    pub refused: BTreeSet<RefusedFactScope>,
    /// Presence of request overrides, distinct from the served connection claims.
    pub quarantined: Quarantine,
}

/// The facts a poll could not read for this model.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefusedFactScope {
    Model,
    Fact(FactKey),
    BaseAndTierRates,
    ModeRates(Option<String>),
}

impl RefusedFactScope {
    pub fn covers(&self, fact: &FactKey) -> bool {
        let key = fact.as_str();
        match self {
            Self::Model => key != "existence" && !key.starts_with(prefix::PROVIDER),
            Self::Fact(refused) => refused == fact,
            Self::BaseAndTierRates => key.starts_with(prefix::RATE) && !rate_key::is_mode_rate(key),
            Self::ModeRates(mode) => key
                .strip_prefix(prefix::RATE)
                .and_then(|rate| rate.split_once(rate_key::MODE))
                .is_some_and(|(_, name)| mode.as_deref().is_none_or(|refused| name == refused)),
        }
    }
}

/// Upstream fields that decide how a request is spoken rather than what a model
/// is.
///
/// These are kept because their PRESENCE is operationally interesting — an
/// `experimental` block appearing on a model is a real event — and because
/// silently dropping a field makes it impossible to tell a field that never
/// existed from one the parser threw away. They are never served.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Quarantine {
    /// A per-model `provider` override: literal headers and body parameters.
    /// Measured on 229 model rows, and one was observed being deleted within
    /// 36 minutes while `last_updated` stayed unchanged.
    pub provider_override: bool,
    /// The names of the model's `experimental.modes`, whether or not they
    /// carry a price. Measured 2026-09-25: 68 modes on 61 models. A mode's
    /// rates are served separately as `rate.<class>.mode.<name>`; what stays
    /// here is only that the mode exists, never the request bytes that switch
    /// it on.
    pub experimental_modes: Vec<String>,
}

/// One provider, reduced to what fusiform has decided to keep.
///
/// # Why the operator-metadata fields are not here
///
/// The upstream provider `name` is carried on each model as `provider.name`.
/// Operator metadata without a served use stays out of the domain:
///
/// - `doc` is a documentation URL, not a served fact.
/// - `env` names the credential environment variable a provider conventionally
///   uses. Not a secret, and not renderer selection either — but it is
///   credential-ADJACENT, and a credential-adjacent field with no decision
///   behind it is the last one that should sit in a serializable type on the
///   chance it turns out useful.
///
/// The `npm` and `api` defaults are resolved into each model's `wire` claims.
/// The legacy presence flags remain for operator inspection; they do not
/// supply or interpret a connection value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedProvider {
    pub provider_id: String,

    /// Whether the provider publishes an `api` default.
    pub api_base_quarantined: bool,
    pub models: Vec<NormalizedModel>,
    /// Whether the provider publishes an `npm` default.
    pub npm_quarantined: bool,
}

/// A whole upstream document, normalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedCatalog {
    pub source: SourceId,
    pub providers: Vec<NormalizedProvider>,
}

impl NormalizedCatalog {
    /// Every model in the document, in deterministic order.
    pub fn models(&self) -> impl Iterator<Item = &NormalizedModel> {
        self.providers.iter().flat_map(|p| p.models.iter())
    }

    pub fn model_count(&self) -> usize {
        self.providers.iter().map(|p| p.models.len()).sum()
    }
}

/// What went wrong, precisely enough to fix without re-fetching.
///
/// Model-level errors identify the affected row; document parse errors retain
/// the parser's diagnostic and source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizeError {
    /// The document's top level was not an object of providers.
    NotAProviderMap,
    /// The parser rejected the document's JSON or provider structure.
    DocumentParse { diagnostic: String },
    /// A rate literal could not be scaled to minor units.
    BadRate {
        model: String,
        field: String,
        value: String,
        detail: crate::money::MoneyError,
    },
    /// A pricing tier carried a `type` other than `context`.
    ///
    /// NOT a fallback to "treat it as context anyway". Measured 2026-08-11: all
    /// 335 tier rows say `context`, so any other value is a shape this parser
    /// has never seen, and reading a threshold out of it would be inferring a
    /// semantic from a field's name — the exact defect that shipped in the
    /// shared catalog crate.
    UnknownTierType { model: String, tier_type: String },
    /// A tier entry had no `tier.type` at all.
    TierMissingType { model: String },
    /// `context_over_200k` carried rates that no tier row reproduces.
    ///
    /// The two encodings coexist on 288 models and their RATES agreed on every
    /// one measured. If they ever disagree, one of them is wrong and fusiform
    /// cannot tell which, so it refuses the schedule rather than picking.
    ///
    /// Note what is compared: the rates, never the threshold. The key's name is
    /// not its threshold — measured, the tier size is 200,000 on only 126 of
    /// those 288 models. Checking the name against the size would be inferring
    /// a semantic from a field's name, which is the defect this parser exists
    /// to avoid.
    OrphanedLegacyTier {
        model: String,
        legacy_rates: Vec<(String, String)>,
    },
    /// A cost key whose charge unit cannot be established.
    ///
    /// `input_audio` sits in the same flat namespace as per-token rates with
    /// nothing in the payload to say whether it bills per token, per second or
    /// per minute. This is not fatal: the rate is recorded as
    /// `Unpriced(UnknownChargeBasis)` and the model still normalizes. The
    /// variant exists so the count is reportable.
    ///
    /// Not reported for `input_audio` when the [`ChargeUnitEvidence`] passed
    /// to [`normalize_models_dev_with`] covers this provider and model and the
    /// cost block has no conditional audio price: there the provider's own
    /// pricing documentation settles the unit, and the key becomes a priced
    /// `rate.input_audio`.
    UnattributableChargeUnit { model: String, key: String },
    /// An unreadable cost member made the model's base and tier rates unsafe to serve.
    RefusedBaseSchedule { model: String, member: String },
    /// A present model field had no readable claim.
    RefusedModelMember { model: String, member: &'static str },
    /// A named mode under `experimental.modes` whose price could not be served.
    ///
    /// The mode produces no new rate, and the model's base rates are unaffected.
    /// Ingest keeps a previously held mode price; no base rate is substituted.
    ///
    /// Two causes. The NAME falls outside `[a-z0-9_-]+`, and serving it would
    /// mint a fact key from arbitrary upstream text. Or the COST is not the
    /// shape a rate schedule has (not an object, a non-numeric value, a key
    /// with no known charge unit, or a literal the money boundary refuses).
    RefusedMode {
        model: String,
        mode: String,
        reason: String,
    },
}

impl std::fmt::Display for NormalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NormalizeError::NotAProviderMap => {
                write!(f, "document root is not a map of provider entries")
            }
            NormalizeError::DocumentParse { diagnostic } => {
                write!(f, "could not parse models.dev document: {diagnostic}")
            }
            NormalizeError::BadRate {
                model,
                field,
                value,
                detail,
            } => write!(f, "{model}: rate {field} = {value:?} is unusable: {detail}"),
            NormalizeError::UnknownTierType { model, tier_type } => write!(
                f,
                "{model}: pricing tier declares type {tier_type:?}, and only \"context\" has a known meaning"
            ),
            NormalizeError::TierMissingType { model } => {
                write!(f, "{model}: pricing tier has no readable type")
            }
            NormalizeError::OrphanedLegacyTier {
                model,
                legacy_rates,
            } => write!(
                f,
                "{model}: context_over_200k states rates {legacy_rates:?} that no tier row reproduces"
            ),
            NormalizeError::UnattributableChargeUnit { model, key } => {
                write!(f, "{model}: cost key {key:?} has no establishable charge unit")
            }
            NormalizeError::RefusedBaseSchedule { model, member } => {
                write!(f, "{model}: cannot read {member}; base and tier rates refused")
            }
            NormalizeError::RefusedModelMember { model, member } => {
                write!(f, "{model}: cannot read {member}; affected model facts refused")
            }
            NormalizeError::RefusedMode {
                model,
                mode,
                reason,
            } => write!(
                f,
                "{model}: experimental mode {mode:?} serves no rate: {reason}"
            ),
        }
    }
}

impl std::error::Error for NormalizeError {}

/// The outcome of normalizing a document.
///
/// Non-fatal findings travel alongside the catalog rather than being logged and
/// forgotten: a rate that could not be attributed to a charge unit is a fact an
/// operator needs to see counted, not a line in a file nobody greps.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(any(test, feature = "legacy-test-parser"))]
pub struct NormalizeOutcome {
    pub catalog: NormalizedCatalog,
    pub findings: Vec<NormalizeError>,
}

/// Which providers' own documentation settles the charge unit of a cost key
/// that models.dev publishes without one.
///
/// The payload's key name is never the evidence. models.dev puts
/// audio cost keys beside the per-token rates on rows whose providers bill audio
/// in different units, so the key alone cannot say what a number charges
/// for. A provider joins this set only when its OWN pricing page states that
/// the corresponding audio direction is priced per million tokens; that decision
/// is curated and reviewed outside this crate (the module's separate input and
/// output audio source files),
/// and handed in here as data so normalization stays free of file IO.
///
/// With no evidence supplied, every audio cost key stays
/// `Unpriced(UnknownChargeBasis)` and is counted. Poll and seed normalization
/// supply the curated evidence; a listed provider can still exclude single
/// models whose audio price its page does not support.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChargeUnitEvidence {
    input_audio_per_token: std::collections::BTreeSet<String>,
    input_audio_models: std::collections::BTreeSet<(String, String)>,
    /// `(provider_id, model_id)` rows left unattributed although the provider
    /// is listed.
    input_audio_excluded: std::collections::BTreeSet<(String, String)>,
    output_audio_per_token: std::collections::BTreeSet<String>,
    output_audio_models: std::collections::BTreeSet<(String, String)>,
    output_audio_excluded: std::collections::BTreeSet<(String, String)>,
}

impl ChargeUnitEvidence {
    /// No provider has established any unit: the strictest reading.
    pub fn none() -> Self {
        Self::default()
    }

    /// Providers whose pricing documentation states audio input is billed per
    /// million tokens.
    pub fn input_audio_per_token<I, S>(providers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            input_audio_per_token: providers.into_iter().map(Into::into).collect(),
            input_audio_excluded: Default::default(),
            ..Self::default()
        }
    }

    /// Leave one model's `input_audio` unattributed even though its provider
    /// is listed.
    pub fn excluding_input_audio_of(
        mut self,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        self.input_audio_excluded
            .insert((provider_id.into(), model_id.into()));
        self
    }

    /// Attribute only this model's audio input when its documentation does
    /// not establish the billing unit of the provider's other models.
    pub fn with_input_audio_model(
        mut self,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        self.input_audio_models
            .insert((provider_id.into(), model_id.into()));
        self
    }

    /// Whether this row's `cost.input_audio` is a per-million-token rate.
    pub fn bills_input_audio_per_token(&self, provider_id: &str, model_id: &str) -> bool {
        (self.input_audio_per_token.contains(provider_id)
            || self
                .input_audio_models
                .contains(&(provider_id.to_string(), model_id.to_string())))
            && !self
                .input_audio_excluded
                .contains(&(provider_id.to_string(), model_id.to_string()))
    }

    /// Add providers whose own pricing page establishes output-audio tokens.
    pub fn with_output_audio_per_token<I, S>(mut self, providers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.output_audio_per_token = providers.into_iter().map(Into::into).collect();
        self
    }

    /// Leave an unsupported model unpriced even when its provider is listed.
    pub fn excluding_output_audio_of(
        mut self,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        self.output_audio_excluded
            .insert((provider_id.into(), model_id.into()));
        self
    }

    /// Attribute audio output only to the model whose token unit was established.
    pub fn with_output_audio_model(
        mut self,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        self.output_audio_models
            .insert((provider_id.into(), model_id.into()));
        self
    }

    /// Whether this row's output-audio cost has an established token unit.
    pub fn bills_output_audio_per_token(&self, provider_id: &str, model_id: &str) -> bool {
        (self.output_audio_per_token.contains(provider_id)
            || self
                .output_audio_models
                .contains(&(provider_id.to_owned(), model_id.to_owned())))
            && !self
                .output_audio_excluded
                .contains(&(provider_id.to_string(), model_id.to_string()))
    }
}

/// Normalize a v2 provider map at the upstream observation time.
pub fn normalize_models_dev_with(
    bytes: &[u8],
    evidence: &ChargeUnitEvidence,
    observed_at: crate::Timestamp,
) -> Result<NormalizeV2Outcome, NormalizeError> {
    v2::normalize_models_dev_at(bytes, evidence, observed_at)
}

/// Normalize a models.dev document with no charge-unit evidence.
///
/// Every audio cost key stays unpriced. Production ingest uses
/// [`normalize_models_dev_with`] and the curated evidence.
pub fn normalize_models_dev(bytes: &[u8]) -> Result<NormalizeV2Outcome, NormalizeError> {
    normalize_models_dev_v2_with(bytes, &ChargeUnitEvidence::none())
}

/// Normalize a legacy fixture without charge-unit evidence for tests only.
#[cfg(any(test, feature = "legacy-test-parser"))]
pub fn normalize_legacy_models_dev_for_tests(
    bytes: &[u8],
) -> Result<NormalizeOutcome, NormalizeError> {
    normalize_legacy_models_dev_for_tests_with(bytes, &ChargeUnitEvidence::none())
}

/// Read the frozen v1 catalog for tests that still exercise legacy behavior.
#[cfg(any(test, feature = "legacy-test-parser"))]
pub fn legacy_models_dev_snapshot_for_tests() -> &'static str {
    static SNAPSHOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SNAPSHOT.get_or_init(|| {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/models-dev-v2/api-v1-20261125.json.gz"
        );
        let output = std::process::Command::new("gzip")
            .args(["-dc", path])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "could not decompress the legacy models.dev test snapshot"
        );
        String::from_utf8(output.stdout).unwrap()
    })
}

/// Normalize a models.dev document, using `evidence` to attribute eligible
/// input/output audio values as per-token rates. Excluded models, and cost blocks
/// that also price audio in a context tier or the legacy `context_over_200k`
/// block, stay unattributed.
#[cfg(any(test, feature = "legacy-test-parser"))]
pub fn normalize_legacy_models_dev_for_tests_with(
    bytes: &[u8],
    evidence: &ChargeUnitEvidence,
) -> Result<NormalizeOutcome, NormalizeError> {
    let document_parse = |error: serde_json::Error| NormalizeError::DocumentParse {
        diagnostic: error.to_string(),
    };
    let root = bytes
        .iter()
        .find(|&&b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'));
    if root != Some(&b'{') {
        // A non-object opening token alone is not a shape diagnosis: the
        // document might instead be malformed or incomplete.
        serde_json::from_slice::<serde_json::Value>(bytes).map_err(document_parse)?;
        return Err(NormalizeError::NotAProviderMap);
    }
    let doc: BTreeMap<String, RawProvider> =
        serde_json::from_slice(bytes).map_err(document_parse)?;

    let mut findings = Vec::new();
    let mut providers = Vec::with_capacity(doc.len());

    for (provider_id, raw_provider) in doc {
        let RawProvider {
            id: _,
            name,
            doc: _,
            env: _,
            npm,
            api,
            models: raw_models,
        } = raw_provider;
        let mut models = Vec::with_capacity(raw_models.len());
        for (model_id, raw_model) in &raw_models {
            let mut model = normalize_model(
                &provider_id,
                model_id,
                raw_model,
                npm.as_deref(),
                api.as_deref(),
                evidence,
                &mut findings,
            );
            model.provider_name = name.clone().filter(|name| !name.is_empty());
            models.push(model);
        }
        providers.push(NormalizedProvider {
            api_base_quarantined: api.is_some(),
            npm_quarantined: npm.is_some(),
            provider_id,
            models,
        });
    }

    Ok(NormalizeOutcome {
        catalog: NormalizedCatalog {
            source: SourceId::ModelsDev,
            providers,
        },
        findings,
    })
}

#[cfg(any(test, feature = "legacy-test-parser"))]
struct ModelRead<'a> {
    label: &'a str,
    findings: &'a mut Vec<NormalizeError>,
    refused: BTreeSet<RefusedFactScope>,
}

#[cfg(any(test, feature = "legacy-test-parser"))]
impl ModelRead<'_> {
    fn read<'raw, T, const OBJECT: bool, const N: usize>(
        &mut self,
        raw: &'raw raw::RawMember<T, OBJECT>,
        member: &'static str,
        scopes: impl FnOnce() -> [RefusedFactScope; N],
    ) -> Option<&'raw T> {
        if raw.unreadable {
            self.refused.extend(scopes());
            self.findings.push(NormalizeError::RefusedModelMember {
                model: self.label.to_owned(),
                member,
            });
            None
        } else {
            Some(&raw.value)
        }
    }

    fn fact<'raw, T>(
        &mut self,
        raw: &'raw raw::RawMember<Option<T>>,
        member: &'static str,
        fact: impl FnOnce() -> FactKey,
    ) -> Option<&'raw T> {
        self.read(raw, member, || [RefusedFactScope::Fact(fact())])
            .and_then(Option::as_ref)
    }
}

#[cfg(any(test, feature = "legacy-test-parser"))]
fn normalize_model(
    provider_id: &str,
    model_id: &str,
    raw: &raw::RawMember<RawModel, true>,
    provider_npm: Option<&str>,
    provider_api: Option<&str>,
    evidence: &ChargeUnitEvidence,
    findings: &mut Vec<NormalizeError>,
) -> NormalizedModel {
    let label = format!("{provider_id}/{model_id}");
    let key = ModelKey::new(SourceId::ModelsDev, provider_id, model_id);
    let mut read = ModelRead {
        label: &label,
        findings,
        refused: BTreeSet::new(),
    };
    let _ = read.read(raw, "model", || [RefusedFactScope::Model]);
    let raw = &raw.value;

    // Name every parsed model field at this boundary. A new raw field must be
    // carried, ignored, or quarantined deliberately rather than silently lost.
    let raw::RawModel {
        id,
        name,
        family,
        canonical_model_id,
        status,
        model_type,
        release_date,
        last_updated,
        knowledge,
        open_weights,
        reasoning,
        reasoning_options,
        structured_output,
        temperature,
        tool_call,
        attachment,
        limit,
        modalities,
        cost,
        provider,
        experimental,
    } = raw;
    read.read(id, "id", || []);

    let mut wire = WireFacts::default();
    if let Some(provider) = read.read(provider, "provider", || {
        ["npm", "api", "shape"].map(|key| RefusedFactScope::Fact(FactKey::wire(key)))
    }) {
        let claim = |read: &mut ModelRead<'_>,
                     raw: &raw::RawMember<Option<String>>,
                     path,
                     key,
                     inherited| {
            let value = read.fact(raw, path, || FactKey::wire(key));
            if raw.unreadable {
                None
            } else {
                wire_claim(value.map(String::as_str), inherited)
            }
        };
        if let Some(raw::RawModelProvider { npm, api, shape }) = provider {
            wire.npm = claim(&mut read, npm, "provider.npm", "npm", provider_npm);
            wire.api = claim(&mut read, api, "provider.api", "api", provider_api);
            wire.shape = claim(&mut read, shape, "provider.shape", "shape", None);
        } else if !read.refused.contains(&RefusedFactScope::Model) {
            wire.npm = wire_claim(None, provider_npm);
            wire.api = wire_claim(None, provider_api);
        }
    }

    let modalities = read
        .read(modalities, "modalities", || {
            ["input_modalities", "output_modalities"]
                .map(|key| RefusedFactScope::Fact(FactKey::capability(key)))
        })
        .and_then(Option::as_ref);
    let modality = |read: &mut ModelRead<'_>, raw: &raw::RawMember<Vec<String>>, member, key| {
        read.read(raw, member, || {
            [RefusedFactScope::Fact(FactKey::capability(key))]
        })
        .map(|list| list.iter().map(|s| Modality::parse(s)).collect())
    };

    // Modalities are `None` when the upstream publishes no block, and an empty
    // Vec only when it publishes an empty list.
    //
    // The difference is the project's own absent-vs-zero rule, which every
    // other field here follows: `reasoning` is `Option<bool>` so a missing flag
    // is unknown rather than false, and a zero limit becomes absence rather
    // than a capacity of zero. Collapsing a missing block to `[]` states that
    // the model accepts NO input modality — a positive claim about a model that
    // certainly accepts something, manufactured from silence.
    //
    // Measured 2026-08-11: zero of 6,254 models omit the block, so this is
    // latent rather than live. It is still worth fixing at the parse boundary,
    // because the day an upstream drops the block is the day the wrong reading
    // ships as data — and a consumer selecting models by modality would filter
    // out every affected model with nothing looking wrong.
    let capabilities = Capabilities {
        input_modalities: modalities
            .and_then(|m| modality(&mut read, &m.input, "modalities.input", "input_modalities")),
        output_modalities: modalities.and_then(|m| {
            modality(
                &mut read,
                &m.output,
                "modalities.output",
                "output_modalities",
            )
        }),
        reasoning: read
            .fact(reasoning, "reasoning", || FactKey::capability("reasoning"))
            .copied(),
        reasoning_options: reasoning_options.clone(),
        structured_output: read
            .fact(structured_output, "structured_output", || {
                FactKey::capability("structured_output")
            })
            .copied(),
        temperature: read
            .fact(temperature, "temperature", || {
                FactKey::capability("temperature")
            })
            .copied(),
        tool_call: read
            .fact(tool_call, "tool_call", || FactKey::capability("tool_call"))
            .copied(),
        attachment: read
            .fact(attachment, "attachment", || {
                FactKey::capability("attachment")
            })
            .copied(),
    };

    // TOTAL DESTRUCTURING, so a field added to `RawLimit` cannot be parsed and
    // silently dropped here.
    //
    // This is the layer neither other fence covers. `measured_fields_are_read_or
    // _declared` checks payload -> parser and counts a field as handled once the
    // parser touches it; `every_capability_field_reaches_a_fact` checks domain ->
    // wire. Between them sits raw -> domain, and `limit.input` lived there
    // unnoticed: parsed into `RawLimit`, never carried into `Limits`, never
    // served, while the payload-side fence reported it as read.
    let limits = match read
        .read(limit, "limit", || {
            ["context", "output"].map(|key| RefusedFactScope::Fact(FactKey::limit(key)))
        })
        .and_then(Option::as_ref)
    {
        None => Limits::default(),
        Some(crate::normalize::raw::RawLimit {
            context,
            output,
            // NOT CARRIED, deliberately, and the reason is what the field means
            // rather than that nobody asked for it.
            //
            // `input` is authored independently upstream — measured 2026-08-15,
            // 1,199 models publish it and `gpt-5-pro` carries context 400,000 /
            // input 272,000 / output 272,000, a set that no arithmetic on the
            // other two produces. On 470 models `input + output == context`
            // exactly, so it is real geometry rather than noise.
            //
            // What is NOT established is which access path it constrains. A
            // reseller's API-path ceiling and a first-party prompt ceiling are
            // both plausible readings and the payload distinguishes neither.
            // Serving it as `limit.input` beside `limit.context` would imply
            // they bound the same thing, and a consumer sizing a prompt against
            // it would be right for some providers and wrong for others with no
            // way to tell which.
            //
            // Carry sourced fields, but not a claim the source does not make.
            // A per-path ceiling requires a documented measurement naming the
            // access path it constrains.
            input,
        }) => {
            read.read(input, "limit.input", || []);
            Limits {
                context_tokens: absent_if_zero(
                    read.fact(context, "limit.context", || FactKey::limit("context"))
                        .copied(),
                ),
                output_tokens: absent_if_zero(
                    read.fact(output, "limit.output", || FactKey::limit("output"))
                        .copied(),
                ),
            }
        }
    };

    let mut rates = Vec::new();
    if cost.unreadable {
        read.refused.insert(RefusedFactScope::BaseAndTierRates);
        read.findings.push(NormalizeError::RefusedBaseSchedule {
            model: label.clone(),
            member: "cost".into(),
        });
    } else if let Some(cost) = &cost.value {
        if !cost.unreadable_members.is_empty() {
            read.refused.insert(RefusedFactScope::BaseAndTierRates);
        }
        match normalize_rates(
            &label,
            cost,
            evidence.bills_input_audio_per_token(provider_id, model_id),
            evidence.bills_output_audio_per_token(provider_id, model_id),
            read.findings,
        ) {
            Ok(base) => rates = base,
            Err(finding) => {
                read.refused.insert(RefusedFactScope::BaseAndTierRates);
                read.findings.push(finding);
            }
        }
    }

    // Mode rates are read whether or not the model has a base cost: five live
    // models (anyapi and snowflake-cortex rows) price a mode and publish no
    // base rate, and that is served as published rather than dropped.
    let mut experimental_modes = Vec::new();
    if let Some(experimental) = read
        .read(experimental, "experimental", || {
            [RefusedFactScope::ModeRates(None)]
        })
        .and_then(Option::as_ref)
    {
        if let Some(modes) = read.read(&experimental.modes, "experimental.modes", || {
            [RefusedFactScope::ModeRates(None)]
        }) {
            for (name, mode) in modes {
                experimental_modes.push(name.clone());
                match normalize_mode_rates(&label, name, mode) {
                    Ok(mode_rates) => rates.extend(mode_rates),
                    Err(finding) => {
                        if is_servable_mode_name(name) {
                            read.refused
                                .insert(RefusedFactScope::ModeRates(Some(name.clone())));
                        }
                        read.findings.push(finding);
                    }
                }
            }
        }
    }

    let quarantined = Quarantine {
        provider_override: provider.unreadable || provider.value.is_some(),
        experimental_modes,
    };

    NormalizedModel {
        key,
        display_name: read
            .fact(name, "name", || FactKey::model("name"))
            .filter(|name| !name.is_empty())
            .cloned(),
        provider_name: None,
        family: read
            .fact(family, "family", || FactKey::model("family"))
            .cloned(),
        canonical_id: read
            .fact(canonical_model_id, "canonical_model_id", || {
                FactKey::model("canonical_id")
            })
            .cloned(),
        status: read
            .fact(status, "status", || FactKey::model("status"))
            .cloned(),
        model_type: read
            .fact(model_type, "type", || FactKey::model("type"))
            .cloned(),
        release_date: read
            .read(release_date, "release_date", || [])
            .cloned()
            .flatten(),
        last_updated: read
            .read(last_updated, "last_updated", || [])
            .cloned()
            .flatten(),
        knowledge_cutoff: read.read(knowledge, "knowledge", || []).cloned().flatten(),
        open_weights: read
            .fact(open_weights, "open_weights", || {
                FactKey::model("open_weights")
            })
            .copied(),
        capabilities,
        limits,
        wire,
        rates,
        refused: read.refused,
        quarantined,
    }
}

fn wire_claim(model: Option<&str>, provider: Option<&str>) -> Option<WireClaim> {
    model
        .map(|value| (value, WireOrigin::Model))
        .or_else(|| provider.map(|value| (value, WireOrigin::Provider)))
        .map(|(value, from)| WireClaim {
            value: value.to_owned(),
            from,
        })
}

/// A declared zero capacity is absence, not a capacity of zero.
///
/// Applied to every limit field unconditionally, including on text models. The
/// tempting refinement — only treat zero as absence where a token limit is
/// meaningless — would make the rule depend on modality, and the measured data
/// has text models with `context: 0` too. A rule that holds everywhere is one
/// a consumer can rely on without knowing the modality.
fn absent_if_zero(value: Option<u64>) -> Option<u64> {
    match value {
        Some(0) | None => None,
        Some(v) => Some(v),
    }
}

#[cfg(any(test, feature = "legacy-test-parser"))]
fn normalize_rates(
    label: &str,
    cost: &raw::RawCost,
    input_audio_per_token: bool,
    output_audio_per_token: bool,
    findings: &mut Vec<NormalizeError>,
) -> Result<Vec<NormalizedRate>, NormalizeError> {
    if !cost.unreadable_members.is_empty() {
        findings.extend(cost.unreadable_members.iter().map(|member| {
            NormalizeError::RefusedBaseSchedule {
                model: label.to_string(),
                member: member.clone(),
            }
        }));
        // Serving the readable subset could understate the price. Named modes
        // have independent schedules and are normalized separately.
        return Ok(Vec::new());
    }

    let mut rates = Vec::new();

    // Base rates: the token classes, always unconditional.
    for (key, class) in TOKEN_COST_KEYS {
        if let Some(literal) = cost.token_rate(key) {
            rates.push(NormalizedRate {
                basis: ChargeBasis::PerMillionTokens { class: *class },
                condition: RateCondition::Always,
                value: rate_value(label, key, literal)?,
            });
        }
    }

    // Cost keys this parser has no charge unit for. Recorded as explicitly
    // unpriced so a consumer refuses rather than pricing audio as text, and
    // reported so the count is visible.
    for key in cost.unattributable_keys() {
        // Audio input on a provider whose own pricing page establishes
        // per-million-token billing: a token rate in the same representation
        // as `input`.
        //
        // Only the unconditional rate, and only when the cost block has no
        // audio price in a context tier or the legacy `context_over_200k`
        // block. Otherwise the base rate alone would understate every request
        // above the threshold, and tiered audio has no served key, so the
        // whole key stays unattributed instead.
        if key == INPUT_AUDIO_COST_KEY
            && input_audio_per_token
            && !cost.prices_input_audio_conditionally()
        {
            rates.push(NormalizedRate {
                basis: ChargeBasis::PerMillionTokens {
                    class: TokenClass::InputAudio,
                },
                condition: RateCondition::Always,
                value: rate_value(label, key, &cost.scalars[key.as_str()])?,
            });
            continue;
        }
        if key == "output_audio"
            && output_audio_per_token
            && !cost.prices_output_audio_conditionally()
        {
            rates.push(NormalizedRate {
                basis: ChargeBasis::PerMillionTokens {
                    class: TokenClass::OutputAudio,
                },
                condition: RateCondition::Always,
                value: rate_value(label, key, &cost.scalars[key.as_str()])?,
            });
            continue;
        }
        findings.push(NormalizeError::UnattributableChargeUnit {
            model: label.to_string(),
            key: key.clone(),
        });
        rates.push(NormalizedRate {
            // `PerRequest` is not a guess at the unit — it is the placeholder
            // basis for a rate whose value is explicitly unpriced, so the row
            // records that the key EXISTS without asserting what it charges.
            basis: ChargeBasis::PerRequest,
            condition: RateCondition::Always,
            value: RateValue::Unpriced {
                reason: UnpricedReason::UnknownChargeBasis,
                inherited_from: None,
            },
        });
    }

    // Context tiers. `tier.size` is read only where `tier.type` says
    // `context`; the size field's NAME is not evidence of what it thresholds.
    let mut tier_rate_sets: Vec<&BTreeMap<String, String>> = Vec::new();
    for tier in &cost.tiers {
        let Some(tier_type) = tier.tier.as_ref().and_then(|t| t.tier_type.as_deref()) else {
            return Err(NormalizeError::TierMissingType {
                model: label.to_string(),
            });
        };
        if tier_type != "context" {
            return Err(NormalizeError::UnknownTierType {
                model: label.to_string(),
                tier_type: tier_type.to_string(),
            });
        }
        tier_rate_sets.push(&tier.scalars);
        let Some(size) = tier.tier.as_ref().and_then(|t| t.size) else {
            continue;
        };
        for (key, class) in TOKEN_COST_KEYS {
            if let Some(literal) = tier.token_rate(key) {
                rates.push(NormalizedRate {
                    basis: ChargeBasis::PerMillionTokens { class: *class },
                    condition: RateCondition::MinContextTokens { tokens: size },
                    value: rate_value(label, key, literal)?,
                });
            }
        }
    }

    // `context_over_200k` is an older encoding of the same rates, measured on
    // 288 models and agreeing with a tier row on every one. It is read for
    // CORROBORATION only and contributes no rate of its own: if no tier row
    // reproduces its rates, the two encodings disagree, fusiform cannot tell
    // which is right, and it stops rather than choosing a winner.
    //
    // The comparison is on RATES, not on the threshold. The measured tier size
    // for models carrying this key is 200,000 on only 126 of the 288; the name
    // is a legacy label that stopped tracking the number it refers to. Matching
    // the name against the size would reject 162 correct documents.
    if let Some(legacy) = &cost.context_over_200k {
        if !tier_rate_sets.iter().any(|tier| {
            tier.len() == legacy.len()
                && legacy.iter().all(|(key, literal)| {
                    tier.get(key).is_some_and(|candidate| {
                        literal == candidate
                            || exact_decimal_key(literal)
                                .is_some_and(|value| Some(value) == exact_decimal_key(candidate))
                    })
                })
        }) {
            return Err(NormalizeError::OrphanedLegacyTier {
                model: label.to_string(),
                legacy_rates: legacy.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            });
        }
    }

    Ok(rates)
}

/// Canonical sign, significant digits and power of ten for a JSON number.
/// Comparing rounded nanounits would equate distinct prices below that resolution.
fn exact_decimal_key(literal: &str) -> Option<(bool, String, i128)> {
    let (mantissa, exponent) = literal.split_once(['e', 'E']).unwrap_or((literal, "0"));
    let (unsigned, negative) = mantissa
        .strip_prefix('-')
        .map_or((mantissa, false), |v| (v, true));
    let (integer, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let digits = format!("{integer}{fraction}");
    let nonzero = digits.trim_start_matches('0');
    if nonzero.is_empty() {
        return Some((false, String::new(), 0));
    }
    let significant = nonzero.trim_end_matches('0');
    let shift = (nonzero.len() - significant.len()) as i128 - fraction.len() as i128;
    let power = exponent.parse::<i128>().ok()?.checked_add(shift)?;
    Some((negative, significant.to_owned(), power))
}

/// Whether an upstream mode name may become part of a fact key.
///
/// `[a-z0-9_-]+`, which every measured name satisfies (`fast`, `priority`,
/// `pro`). The name is served verbatim inside `rate.<class>.mode.<name>`, so a
/// dot, a space or an uppercase letter would produce a key that a prefix filter
/// or a consumer splitting on `.` reads differently from the one written.
pub fn is_servable_mode_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// The rates one named mode publishes, or none.
///
/// A mode with no `cost` is a mode without a published price, not a refusal.
/// A refusal is reported separately so ingest keeps the held mode price rather
/// than mistaking an unreadable member for a withdrawal. Neither path falls
/// back to the base rate.
///
/// All or nothing per mode: if any key in the cost block is unusable the whole
/// mode is refused, because serving the dimensions that happened to parse would
/// present a partial schedule as the complete one.
#[cfg(any(test, feature = "legacy-test-parser"))]
fn normalize_mode_rates(
    label: &str,
    name: &str,
    mode: &raw::RawMode,
) -> Result<Vec<NormalizedRate>, NormalizeError> {
    let refuse = |reason: String| {
        Err(NormalizeError::RefusedMode {
            model: label.to_string(),
            mode: name.to_string(),
            reason,
        })
    };

    if mode.malformed {
        return refuse("the mode is not an object".to_string());
    }
    let Some(cost) = &mode.cost else {
        return Ok(Vec::new());
    };
    if !is_servable_mode_name(name) {
        return refuse("the name is outside [a-z0-9_-]+ and cannot form a fact key".to_string());
    }
    let Some(cost) = cost.as_object() else {
        return refuse(format!("cost is not an object: {cost}"));
    };

    let mut rates = Vec::new();
    for (key, value) in cost {
        let Some((_, class)) = TOKEN_COST_KEYS.iter().find(|(k, _)| k == key) else {
            // Tiers, legacy tier blocks and audio keys have never been measured
            // inside a mode, so their meaning there is unestablished.
            return refuse(format!(
                "cost key {key:?} has no known charge unit inside a mode"
            ));
        };
        let Some(number) = value.as_number() else {
            return refuse(format!("cost.{key} is not a number: {value}"));
        };
        let value = match rate_value(label, key, &number.to_string()) {
            Ok(v) => v,
            Err(e) => return refuse(e.to_string()),
        };
        rates.push(NormalizedRate {
            basis: ChargeBasis::PerMillionTokens { class: *class },
            condition: RateCondition::Mode {
                name: name.to_string(),
            },
            value,
        });
    }
    Ok(rates)
}

/// The models.dev cost key for audio input, whose unit only a provider's own
/// documentation can establish (see [`ChargeUnitEvidence`]).
#[cfg(any(test, feature = "legacy-test-parser"))]
const INPUT_AUDIO_COST_KEY: &str = "input_audio";

/// The cost keys whose charge unit is established: per million tokens.
#[cfg(any(test, feature = "legacy-test-parser"))]
const TOKEN_COST_KEYS: &[(&str, TokenClass)] = &[
    ("input", TokenClass::Input),
    ("output", TokenClass::Output),
    ("cache_read", TokenClass::CacheRead),
    ("cache_write", TokenClass::CacheWrite),
    ("reasoning", TokenClass::Reasoning),
];

/// Turn one rate literal into a value, keeping a stated zero distinct from a
/// price.
fn rate_value(label: &str, field: &str, literal: &str) -> Result<RateValue, NormalizeError> {
    match decimal_str_to_minor_units(literal, NANO_EXPONENT) {
        Ok(0) => Ok(RateValue::StatedZero {
            inherited_from: None,
        }),
        // `Floor::Unknown`, and it is the TRUE state rather than a placeholder.
        //
        // models.dev publishes no minimum-charge data of any kind, so nobody
        // has established whether a call to this model has a floor. Saying so
        // is what stops a consumer concluding there is none — which is exactly
        // what they must conclude today, when the dimension does not exist.
        Ok(units) => Ok(RateValue::Priced {
            floor: fusiform_protocol::money::Floor::Unknown,
            per: None,
            increment: fusiform_protocol::money::Increment::Unknown,
            minimum_quantity: fusiform_protocol::money::MinimumQuantity::Unknown,
            // `None` at NORMALIZATION, always: this is the upstream's own
            // published price for its own row. Inheritance is a serve-time
            // derivation that attaches one provider's price to another's row,
            // and it has no business in the normalizer — a stored fact must
            // record what the source said, which is the rule the August
            // provenance incident established at the cost of 17,455 false
            // price eras.
            inherited_from: None,
            amount: Amount {
                units,
                exponent: NANO_EXPONENT,
                currency: CurrencyCode::new(MODELS_DEV_ASSUMED_CURRENCY)
                    .expect("USD is a valid ISO 4217 code"),
                unit_provenance: UnitProvenance::AssumedByPolicy {
                    policy: PolicyId::models_dev_usd_v1(),
                },
            },
        }),
        Err(detail) => Err(NormalizeError::BadRate {
            model: label.to_string(),
            field: field.to_string(),
            value: literal.to_string(),
            detail,
        }),
    }
}
