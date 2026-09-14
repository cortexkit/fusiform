//! Reading models.dev, and refusing to guess.
//!
//! This module turns one upstream document into fusiform's domain. Everything
//! it does follows from a single stance: **the upstream is data, never
//! authority**. A field whose meaning cannot be established does not get a
//! plausible default — it gets an explicit unknown, or it stops the parse.
//!
//! The measurements that shaped each rule are in
//! `docs/design/schema-and-store.md`; the ones that decide code here are noted
//! at the rule.
//!
//! # What this module refuses to do
//!
//! - **Serve a renderer-selecting field.** `provider.npm`, per-model `provider`
//!   overrides and `experimental` decide how a request is spoken, not what a
//!   model is. They are parsed only far enough to be quarantined.
//! - **Infer a semantic from a field's name.** `tier.size` is read as a
//!   threshold only where `tier.type` says `context`; anywhere else it is a
//!   parse error rather than a guess.
//! - **Turn a missing value into a real one.** A zero limit is absence, a
//!   missing rate is `Unpriced`, and an unattributable charge unit is
//!   `UnknownChargeBasis` — never a number a consumer would spend against.

mod raw;

use std::collections::BTreeMap;

use crate::money::{
    decimal_str_to_minor_units, Amount, CurrencyCode, PolicyId, UnitProvenance, NANO_EXPONENT,
};
use crate::{
    ChargeBasis, Modality, ModelKey, RateCondition, RateValue, SourceId, TokenClass, UnpricedReason,
};

pub use raw::{RawModel, RawProvider};

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

/// One model, normalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedModel {
    pub key: ModelKey,
    pub display_name: Option<String>,
    pub family: Option<String>,
    pub release_date: Option<String>,
    pub last_updated: Option<String>,
    pub knowledge_cutoff: Option<String>,
    pub open_weights: Option<bool>,
    pub capabilities: Capabilities,
    pub limits: Limits,
    pub rates: Vec<NormalizedRate>,
    /// Renderer-selecting facts found on this model, recorded so an operator
    /// can see they exist and never exposed on the served surface.
    pub quarantined: Quarantine,
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
    /// An `experimental.modes` block. Measured on 38 models; all 38 carry
    /// per-mode `cost`, and 33 also carry a base rate.
    pub experimental_modes: Vec<String>,
}

/// One provider, reduced to what fusiform has decided to keep.
///
/// # Why the operator-metadata fields are not here
///
/// The upstream publishes `name`, `doc` and `env` per provider. They were
/// carried here as values and read by nothing, which is the state this type now
/// refuses: **an unread field is a decision nobody made.**
///
/// Each of the three needed a decision rather than a rule applied to all of
/// them, and the decision was "do not parse it into the domain":
///
/// - `name` is a display label, harmless and unused. If a consumer wants it, it
///   becomes a served fact with a name and a test, not a field that is already
///   in the struct waiting to be plumbed.
/// - `doc` is a documentation URL, same disposition.
/// - `env` names the credential environment variable a provider conventionally
///   uses. Not a secret, and not renderer selection either — but it is
///   credential-ADJACENT, and a credential-adjacent field with no decision
///   behind it is the last one that should sit in a serializable type on the
///   chance it turns out useful.
///
/// Contrast the two fields that ARE here as flags. `npm` and `api` are
/// renderer-selection: they decide which adapter speaks and which host
/// receives the request. Those are quarantined — recorded as present, never
/// carried — because fusiform must be able to say it never serves them.
///
/// So the disposition splits three ways rather than two: quarantine what
/// decides bytes, serve what a consumer has asked for, and do not parse the
/// rest. All three are recoverable in one line from `raw.rs` when someone has
/// an actual use; what is not recoverable is the refactor that quietly plumbs
/// an unread value onto a wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedProvider {
    pub provider_id: String,

    /// Whether the provider publishes an `api` base URL, WITHOUT its value.
    ///
    /// The value is deliberately not carried, for the same reason `npm` is not.
    /// A base URL decides which company receives the request, so serving a
    /// wrong one is not a degraded answer — it is a request sent to the wrong
    /// host, with credentials attached. That is renderer selection, which is a
    /// consumer's to author and never fusiform's to supply.
    ///
    /// It was previously carried as `api_base: Option<String>` and read by
    /// nothing. That is an UNREAD field rather than a quarantined one, and the
    /// difference is not academic: a quarantine is a promise the code makes, an
    /// unread field is an accident of parsing. A base URL sitting in a
    /// serializable domain type is one refactor away from a wire it must never
    /// reach, and nothing about its name would have warned the person doing it.
    ///
    /// Found 2026-08-13 when BROCA measured that 156 of 183 providers depend on
    /// this field for their endpoint and discovered they had it on the
    /// stays-mine side of a split while not authoring it.
    pub api_base_quarantined: bool,
    pub models: Vec<NormalizedModel>,
    /// The provider's `npm` field: which SDK adapter speaks to it. The single
    /// most renderer-selecting fact in the document, and never served.
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
/// Every variant carries the model it happened on. When one variant lacks the
/// context its siblings carry, that is nearly always an oversight rather than a
/// decision, and the cost lands on whoever reads the log at 3am with no way to
/// tell which of 6,000 rows caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizeError {
    /// The document's top level was not an object of providers.
    NotAProviderMap,
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
    /// cannot tell which — so it stops rather than picking.
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
    /// nothing to say whether it bills per token, per second or per minute.
    /// This is not fatal: the rate is recorded as `Unpriced(UnknownChargeBasis)`
    /// and the model still normalizes. The variant exists so the count is
    /// reportable.
    UnattributableChargeUnit { model: String, key: String },
}

impl std::fmt::Display for NormalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NormalizeError::NotAProviderMap => {
                write!(f, "document root is not a map of provider entries")
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
                // Says what is known, and no more.
                //
                // This fires from `tier.tier == None`, which has TWO causes that
                // are indistinguishable here: the `tier` key was absent, or it
                // was present and did not parse — `RawTier`'s conversion drops
                // the parse error, and both fields of `RawTierSpec` are
                // optional, so a failure means the value was not an object or a
                // key had the wrong type (`"size": "200000"` as a string is the
                // ordinary way an upstream drifts).
                //
                // The old wording — "carries a size with no type" — asserted
                // both halves. On the malformed case neither is known, and an
                // operator greps the payload for a tier with a size and no type,
                // finds one with both, and hunts a phantom. A message that
                // travels to a reader and names a specific cause it cannot
                // support is the sentinel problem in prose.
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
pub struct NormalizeOutcome {
    pub catalog: NormalizedCatalog,
    pub findings: Vec<NormalizeError>,
}

/// Normalize a models.dev document.
pub fn normalize_models_dev(bytes: &[u8]) -> Result<NormalizeOutcome, NormalizeError> {
    let doc: BTreeMap<String, RawProvider> =
        serde_json::from_slice(bytes).map_err(|_| NormalizeError::NotAProviderMap)?;

    let mut findings = Vec::new();
    let mut providers = Vec::with_capacity(doc.len());

    for (provider_id, raw_provider) in doc {
        let mut models = Vec::with_capacity(raw_provider.models.len());
        for (model_id, raw_model) in &raw_provider.models {
            models.push(normalize_model(
                &provider_id,
                model_id,
                raw_model,
                &mut findings,
            )?);
        }
        providers.push(NormalizedProvider {
            api_base_quarantined: raw_provider.api.is_some(),
            npm_quarantined: raw_provider.npm.is_some(),
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

fn normalize_model(
    provider_id: &str,
    model_id: &str,
    raw: &RawModel,
    findings: &mut Vec<NormalizeError>,
) -> Result<NormalizedModel, NormalizeError> {
    let label = format!("{provider_id}/{model_id}");
    let key = ModelKey::new(SourceId::ModelsDev, provider_id, model_id);

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
        input_modalities: raw
            .modalities
            .as_ref()
            .map(|m| m.input.iter().map(|s| Modality::parse(s)).collect()),
        output_modalities: raw
            .modalities
            .as_ref()
            .map(|m| m.output.iter().map(|s| Modality::parse(s)).collect()),
        reasoning: raw.reasoning,
        tool_call: raw.tool_call,
        attachment: raw.attachment,
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
    let limits = match raw.limit.as_ref() {
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
            // Same posture as the tiered-rate scheme: carry what is sourced,
            // refuse to carry a claim the source does not make. The overlay is
            // where a per-path ceiling belongs, because a cell there names the
            // path it was measured on.
            input: _,
        }) => Limits {
            context_tokens: absent_if_zero(*context),
            output_tokens: absent_if_zero(*output),
        },
    };

    let rates = match &raw.cost {
        Some(cost) => normalize_rates(&label, cost, findings)?,
        // No cost object at all. Measured on 420 models. Recording nothing here
        // is correct: the absence is expressed by there being no rate rows, and
        // a consumer asking for a rate gets `NoCatalogCoverage` from the store
        // rather than a fabricated `MissingRate` row for every basis fusiform
        // can imagine.
        None => Vec::new(),
    };

    let quarantined = Quarantine {
        provider_override: raw.provider.is_some(),
        experimental_modes: raw
            .experimental
            .as_ref()
            .map(|e| e.modes.keys().cloned().collect())
            .unwrap_or_default(),
    };

    Ok(NormalizedModel {
        key,
        display_name: raw.name.clone(),
        family: raw.family.clone(),
        release_date: raw.release_date.clone(),
        last_updated: raw.last_updated.clone(),
        knowledge_cutoff: raw.knowledge.clone(),
        open_weights: raw.open_weights,
        capabilities,
        limits,
        rates,
        quarantined,
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

fn normalize_rates(
    label: &str,
    cost: &raw::RawCost,
    findings: &mut Vec<NormalizeError>,
) -> Result<Vec<NormalizedRate>, NormalizeError> {
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
        if !tier_rate_sets.contains(&legacy) {
            return Err(NormalizeError::OrphanedLegacyTier {
                model: label.to_string(),
                legacy_rates: legacy.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            });
        }
    }

    Ok(rates)
}

/// The cost keys whose charge unit is established: per million tokens.
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
        Ok(0) => Ok(RateValue::StatedZero),
        // `Floor::Unknown`, and it is the TRUE state rather than a placeholder.
        //
        // models.dev publishes no minimum-charge data of any kind, so nobody
        // has established whether a call to this model has a floor. Saying so
        // is what stops a consumer concluding there is none — which is exactly
        // what they must conclude today, when the dimension does not exist.
        Ok(units) => Ok(RateValue::Priced {
            floor: fusiform_protocol::money::Floor::Unknown,
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
