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
//! - **Infer a semantic from a field's name.** A price override becomes a
//!   context tier only when its predicate can be represented exactly as a
//!   context threshold. An unsupported predicate refuses only its own price.
//! - **Turn a missing value into a real one.** A zero limit is absence, a
//!   missing rate is `Unpriced`, and an unattributable charge unit is
//!   `UnknownChargeBasis` — never a number a consumer would spend against.

pub mod v2;

use std::collections::BTreeSet;

use crate::money::{
    decimal_str_to_minor_units, Amount, CurrencyCode, PolicyId, UnitProvenance, NANO_EXPONENT,
};
use crate::upstream_facts::{prefix, rate_key, FactKey};
use crate::{ChargeBasis, Modality, ModelKey, RateCondition, RateValue, SourceId};
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
    /// Historical metadata for an upstream model-level request override.
    /// V2 does not populate this flag; it keeps such values out of facts.
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
/// V2 carries resolved endpoint and SDK claims on each model. The historical
/// provider-level flags remain comparable with saved normalized catalogs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedProvider {
    pub provider_id: String,

    /// Whether the provider published an `api` default before the v2 cutover.
    pub api_base_quarantined: bool,
    pub models: Vec<NormalizedModel>,
    /// Whether the provider published an `npm` default before the v2 cutover.
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
        }
    }
}

impl std::error::Error for NormalizeError {}

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
