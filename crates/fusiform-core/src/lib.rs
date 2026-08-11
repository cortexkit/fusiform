#![forbid(unsafe_code)]

//! The fusiform domain: what AI models exist, what they can do, what they
//! cost, and how fresh that knowledge is.
//!
//! This crate is pure. It holds no I/O opinions, opens no store, and speaks to
//! no daemon — those belong to `fusiform-module`. What lives here is the shape
//! of the catalog and the rules that make it honest.
//!
//! The design these types implement, with the measurements behind each rule,
//! is `docs/design/schema-and-store.md`. Three of its constraints are worth
//! restating at the door, because every type below is shaped by one of them:
//!
//! - **Fusiform describes; it never dispatches.** It may say what a model is,
//!   never how to speak to it. Fields that select a renderer are parsed, kept
//!   as provenance, and never served.
//! - **Upstreams are data, never authority.** Every fact carries its source,
//!   when it was observed, and how its boundary was determined, so a consumer
//!   can always answer "why does the catalog say this".
//! - **Absent, zero, unknown and retired are four different states.** Every
//!   place a default could stand in for a missing value is a place a bug
//!   hides, so the types make absence representable and force the caller to
//!   handle it.

pub mod money;

use serde::{Deserialize, Serialize};

pub use money::{Amount, CurrencyCode, MoneyError, PolicyId, UnitProvenance};

/// A declared upstream.
///
/// Adding one is a code change, deliberately. A source's identity carries
/// policy weight — whether it publishes real effective dates, what currency it
/// states — so it must not be mintable from the data it describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceId {
    ModelsDev,
    /// The compile-time bootstrap snapshot. Never a fetch: a seed says where
    /// the store started, never what an upstream did.
    Seed,
}

impl SourceId {
    pub const fn as_str(self) -> &'static str {
        match self {
            SourceId::ModelsDev => "models.dev",
            SourceId::Seed => "seed",
        }
    }
}

/// The identity of a model row.
///
/// The pair, never the bare id. Measured on 2026-08-11: models.dev carried
/// 6,253 model rows under only 2,957 distinct ids — `openai/gpt-oss-120b`
/// appears under 28 different providers at different prices. Anything keyed on
/// the id alone fuses 28 distinct offerings into one.
///
/// `source` is in the key because a second upstream describing the same model
/// is a different claim, not an overwrite.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModelKey {
    pub source: SourceId,
    pub provider_id: String,
    pub model_id: String,
}

impl ModelKey {
    pub fn new(
        source: SourceId,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            source,
            provider_id: provider_id.into(),
            model_id: model_id.into(),
        }
    }
}

impl std::fmt::Display for ModelKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}/{}",
            self.source.as_str(),
            self.provider_id,
            self.model_id
        )
    }
}

/// A millisecond instant in UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Timestamp(pub i64);

/// What a model consumes or emits.
///
/// Non-text modalities are not hypothetical: the 2026-08-11 payload carried
/// 164 image-output, 75 audio-output and 64 video-output models — inside an
/// upstream whose pricing cannot express how any of them bill.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    Audio,
    Video,
    Pdf,
    /// A value this version does not recognise, preserved verbatim.
    ///
    /// An unknown modality is a fact about the upstream, not a parse failure,
    /// and coercing it into a known variant would invent a capability claim.
    Other(String),
}

impl Modality {
    /// Parse without coercion: an unrecognised value becomes `Other`, never a
    /// near-neighbour and never a dropped field.
    pub fn parse(value: &str) -> Self {
        match value {
            "text" => Modality::Text,
            "image" => Modality::Image,
            "audio" => Modality::Audio,
            "video" => Modality::Video,
            "pdf" => Modality::Pdf,
            other => Modality::Other(other.to_string()),
        }
    }

    /// Whether this modality is counted in tokens.
    ///
    /// Used to reason about whether a token limit is even applicable — never
    /// to decide whether a zero limit is meaningful, which is unconditional.
    pub fn is_token_shaped(&self) -> bool {
        matches!(self, Modality::Text)
    }
}

/// How an era's boundary instant was determined.
///
/// The kind is load-bearing rather than descriptive: it says what quality of
/// fact a consumer is holding. A source that publishes real effective dates
/// must not be degraded to fusiform's observation cadence, and a source polled
/// blind must never be dressed up as if it published dates. Both directions
/// are lies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum BoundaryKind {
    /// The value differed between two successive observations, so the change
    /// happened somewhere in `(prior_observation_at, boundary_at]`.
    ///
    /// This is an *observation* boundary, not a provider announcement:
    /// models.dev publishes no effective dates.
    Observed,
    /// The source stated an effective date. `boundary_at` is the source's
    /// claim rather than fusiform's. No source does this today.
    Asserted,
    /// Bootstrapped from the embedded seed. Not evidence of anything about the
    /// upstream's timeline — a store came into existence, which is not an
    /// upstream event.
    Seed,
    /// Fusiform misread the upstream and is correcting its own record. The
    /// upstream did not move.
    Corrected(Correction),
}

/// The extent of a correction: which facts were wrong, and over what interval.
///
/// A bare annotation is a note; an auditable correction says which fields were
/// wrong over which prior interval, so a consumer can mechanically select
/// everything it derived from the bad region instead of eyeballing dates.
///
/// It is also what lets a point-in-time read *refuse*. Without the interval a
/// corrected era is indistinguishable from a later change, and the bad region
/// stays unmarked — so a query for an instant inside it returns a confident
/// wrong answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Correction {
    pub fields: Vec<FieldId>,
    /// A LOWER BOUND. When the true start of the bad region is not known
    /// precisely it goes to the earliest plausible instant, never the best
    /// guess: an over-inclusive partition costs review time, while an
    /// under-inclusive one leaves bad facts outside a partition that asserts
    /// they are fine — converting an unknown into a false clean bill.
    pub affected_from: Timestamp,
    /// Known exactly: when fusiform stopped recording the bad value.
    pub affected_until: Timestamp,
    /// Names the defect record under `docs/findings/`, so an audit is
    /// repeatable rather than dependent on prose.
    pub reason: String,
}

/// What a correction touches, in the vocabulary of the SERVED contract rather
/// than fusiform's storage.
///
/// A name bound to internal representation changes for reasons that are not
/// facts about the world; the served contract is the only surface where a
/// rename is already a breaking change both sides would notice.
///
/// Granularity is set by one test: two corrections with genuinely different
/// affected sets must not be able to share a `FieldId`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "field")]
pub enum FieldId {
    /// Per token class, never one id for "rates": a correction to one class
    /// need not partition the same charges as a correction to another.
    Rate { class: TokenClass },
    /// A misread unit is not a misread number. "This was per-image, not
    /// per-thousand-images" invalidates every derived charge even where the
    /// numeric value was right.
    ChargeUnit,
    /// Correcting a threshold partitions only charges where a request crossed
    /// the boundary.
    TierThreshold,
    /// Distinct from the threshold: if they shared an id, "the threshold was
    /// wrong" and "the over-threshold rate was wrong" could not be told apart,
    /// and they have different affected sets.
    TierRate,
    /// So a tombstone correction — this model was recorded as present after it
    /// was actually retired — is expressible without claiming a rate was wrong.
    Existence,
}

/// The token classes an LLM bills in.
///
/// These are the LLM special case, not the general shape of billing: video
/// bills per second, audio per minute, images per image at a size and quality.
/// Fusing those into "tokens" is the same failure as fusing currencies, which
/// is why [`ChargeBasis`] carries the unit explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenClass {
    Input,
    Output,
    CacheRead,
    CacheWrite,
    Reasoning,
}

/// A provider's named quality level for a generated artifact.
///
/// Left as a source-declared string rather than an enum: normalising quality
/// names across providers would assert that two providers' tiers are
/// equivalent, which is a claim fusiform has no basis to make.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct QualityTier(pub String);

/// What a rate is charged for.
///
/// A closed enum rather than a string, so a consumer can *fail* on a basis it
/// does not understand rather than guess — and the failure already has a name
/// in the metering vocabulary: `UnknownChargeBasis`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "basis")]
pub enum ChargeBasis {
    PerMillionTokens {
        class: TokenClass,
    },
    PerImage {
        width: u32,
        height: u32,
        quality: QualityTier,
    },
    PerSecond,
    PerMinute,
    PerCharacter,
    PerRequest,
}

/// A condition that selects between rates for the same model.
///
/// Measured: 320 models price by prompt size, so a request property already
/// participates in rate selection — a pricing consumer cannot resolve a rate
/// from a flat `(model, class)` key. Every discriminator a rate needs travels
/// in the rate's own key; if a consumer ever has to join capability rows to
/// price rows, this schema has failed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "condition")]
pub enum RateCondition {
    Always,
    /// Applies at or above this prompt size in tokens.
    MinContextTokens {
        tokens: u64,
    },
}

/// Why a rate has no price.
///
/// Adopted from the metering module's existing enum rather than paralleled, so
/// nothing has to be mapped at the boundary. Their remaining variants
/// (degraded pricing time, arithmetic out of range) are structurally not a
/// producer's: fusiform has no pricing instant and no ledger arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnpricedReason {
    /// The source describes this model but published no rate for this basis.
    MissingRate,
    /// The source does not describe this model at all.
    NoCatalogCoverage,
    /// A rate exists but what it is charged for cannot be established — the
    /// audio keys are the live case, sitting in the same flat namespace as
    /// token rates with no unit distinguishing them.
    UnknownChargeBasis,
}

/// A rate's value: priced, a stated zero, or unpriced with a reason.
///
/// Three states because they have opposite consequences for a cap. Measured on
/// 2026-08-11: 420 models carried no cost object at all, and 1,423 cost
/// entries were exactly `0` — some genuinely free, some certainly "not
/// published", and the upstream cannot distinguish them. Neither can fusiform,
/// so it reports what was said instead of resolving it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum RateValue {
    Priced {
        amount: Amount,
    },
    /// The source stated exactly zero. Not "free" — a stated zero. Whether a
    /// zero is a real price is the consumer's policy, not fusiform's.
    StatedZero,
    Unpriced {
        reason: UnpricedReason,
    },
}

/// What happened when fusiform looked at a source.
///
/// Every poll produces one, including the ones that changed nothing, because
/// an era boundary derived from polling is only boundable if both edges are
/// known. A failed poll observed nothing and must never narrow a window; a 304
/// and a same-hash 200 both confirm the current values and legitimately do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum ObservationOutcome {
    /// The normalized document differed from the last one.
    Changed { snapshot_seq: i64 },
    /// A 200 whose normalized hash matched the last observation.
    Unchanged,
    /// A conditional GET the upstream answered with 304 and no body.
    NotModified,
    /// Nothing was observed. This must not narrow any window.
    Failed { class: FailureClass },
}

impl ObservationOutcome {
    /// Whether this observation confirms the currently held values, and may
    /// therefore act as the near edge of a subsequent era's window.
    ///
    /// The distinction is the whole point of recording failures separately: a
    /// poll that failed is not a poll that returned nothing.
    pub fn confirms_current_values(&self) -> bool {
        matches!(
            self,
            ObservationOutcome::Changed { .. }
                | ObservationOutcome::Unchanged
                | ObservationOutcome::NotModified
        )
    }
}

/// Why an observation failed. Coarse on purpose: a consumer branches on the
/// class, and producer detail travels alongside for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The request never completed: DNS, connection, timeout.
    Network,
    /// The upstream answered with a status fusiform cannot use.
    HttpStatus,
    /// The body arrived and could not be normalised.
    Parse,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_identity_is_the_pair_not_the_id() {
        // The measured case: one model id under two providers, at different
        // prices. If these compared equal the catalog would fuse them.
        let a = ModelKey::new(SourceId::ModelsDev, "groq", "openai/gpt-oss-120b");
        let b = ModelKey::new(SourceId::ModelsDev, "cerebras", "openai/gpt-oss-120b");
        assert_ne!(a, b);
        assert_eq!(a.to_string(), "models.dev:groq/openai/gpt-oss-120b");
    }

    #[test]
    fn the_same_model_from_two_sources_is_two_claims() {
        // A second upstream describing a model is a different claim, never an
        // overwrite — so the source is part of identity.
        let upstream = ModelKey::new(SourceId::ModelsDev, "anthropic", "claude-sonnet-4-5");
        let seeded = ModelKey::new(SourceId::Seed, "anthropic", "claude-sonnet-4-5");
        assert_ne!(upstream, seeded);
    }

    #[test]
    fn an_unknown_modality_is_preserved_not_coerced() {
        assert_eq!(Modality::parse("text"), Modality::Text);
        assert_eq!(Modality::parse("video"), Modality::Video);
        // The failure this prevents: a future modality silently becoming a
        // known one, which would invent a capability claim.
        assert_eq!(
            Modality::parse("hologram"),
            Modality::Other("hologram".to_string())
        );
    }

    #[test]
    fn a_failed_poll_does_not_confirm_anything() {
        // The distinction that keeps observation windows honest: a failed poll
        // observed nothing, so it must never act as a window edge, while a 304
        // and an unchanged 200 both genuinely confirm current values.
        assert!(!ObservationOutcome::Failed {
            class: FailureClass::Network
        }
        .confirms_current_values());
        assert!(!ObservationOutcome::Failed {
            class: FailureClass::Parse
        }
        .confirms_current_values());
        assert!(ObservationOutcome::NotModified.confirms_current_values());
        assert!(ObservationOutcome::Unchanged.confirms_current_values());
        assert!(ObservationOutcome::Changed { snapshot_seq: 7 }.confirms_current_values());
    }

    #[test]
    fn tier_threshold_and_tier_rate_are_different_field_ids() {
        // The test the FieldId vocabulary is built to satisfy: two corrections
        // with different affected sets must not share an id. A wrong threshold
        // partitions only boundary-crossing requests; a wrong over-threshold
        // rate partitions all of them.
        assert_ne!(FieldId::TierThreshold, FieldId::TierRate);
        // And one id per token class, never one for "rates".
        assert_ne!(
            FieldId::Rate {
                class: TokenClass::Input
            },
            FieldId::Rate {
                class: TokenClass::CacheRead
            }
        );
    }

    #[test]
    fn unpriced_zero_and_priced_zero_are_distinguishable() {
        let stated = RateValue::StatedZero;
        let unpriced = RateValue::Unpriced {
            reason: UnpricedReason::MissingRate,
        };
        // "No price" and "the price is zero" have opposite consequences for a
        // cap, so they must never compare equal or serialise alike.
        assert_ne!(stated, unpriced);
        let stated_json = serde_json::to_string(&stated).unwrap();
        let unpriced_json = serde_json::to_string(&unpriced).unwrap();
        assert_ne!(stated_json, unpriced_json);
        assert!(stated_json.contains("stated_zero"));
        assert!(unpriced_json.contains("missing_rate"));
    }

    #[test]
    fn a_correction_carries_extent_not_just_a_kind() {
        // A bare annotation is a note. The interval is what lets a consumer
        // select everything it derived from the bad region, and what lets a
        // point-in-time read refuse rather than answer confidently.
        let correction = Correction {
            fields: vec![FieldId::TierThreshold],
            affected_from: Timestamp(1_784_494_281_391),
            affected_until: Timestamp(1_786_000_000_000),
            reason: "docs/findings/2026-08-11-commons-tier-threshold.md".to_string(),
        };
        let kind = BoundaryKind::Corrected(correction);
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("affected_from"));
        assert!(json.contains("affected_until"));
        let round_tripped: BoundaryKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, round_tripped);
    }
}
