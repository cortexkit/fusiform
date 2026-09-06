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
pub mod normalize;

use serde::{Deserialize, Serialize};

pub use money::{Amount, CurrencyCode, MoneyError, PolicyId, UnitProvenance};
pub use normalize::{normalize_models_dev, NormalizeError, NormalizeOutcome, NormalizedCatalog};

/// A declared upstream.
///
/// Adding one is a code change, deliberately. A source's identity carries
/// policy weight — whether it publishes real effective dates, what currency it
/// states — so it must not be mintable from the data it describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceId {
    ModelsDev,
    /// **No row is written under this source, and none ever has been.**
    ///
    /// It was declared for the compile-time bootstrap snapshot, on the reading
    /// that a seed says where the store started rather than what an upstream
    /// did. The implementation went the other way and was right to: the
    /// snapshot IS models.dev's data, fetched earlier, so `seed.rs` stores it
    /// under `ModelsDev` and records how it was learned with
    /// `BoundaryKind::Seed`. Provenance of a value and identity of a speaker
    /// are different questions, and only the second belongs here.
    ///
    /// This comment used to describe the original intent, which is how the
    /// route arm accepting `"seed"` looked correct for as long as it did:
    /// every query for that source matched zero rows and returned an empty
    /// catalog with a success status. The route refuses it now.
    ///
    /// The variant stays because it carries a property the domain needs before
    /// a second source exists: identity is `(source, provider, model)`, and
    /// `the_same_model_from_two_sources_is_two_claims` needs two sources to
    /// prove it.
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
    /// A capacity limit fusiform misread.
    ///
    /// Per limit, never one id for "limits": a wrong context window and a wrong
    /// output cap affect different things at a consumer, so they cannot share a
    /// partition.
    Limit { limit: LimitId },
    /// A capability flag fusiform misread.
    ///
    /// Per capability, for the same reason. A wrong `reasoning` flag and a
    /// wrong `attachment` flag are unrelated defects with unrelated blast
    /// radii.
    Capability { capability: CapabilityId },
}

/// Which capacity limit a correction names.
///
/// A closed enum rather than a string, so a limit cannot enter the correction
/// vocabulary by being typed into a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitId {
    /// The context window. A consumer sizes its transform pressure on this.
    Context,
    /// The maximum output. A consumer renders this as a request parameter.
    Output,
}

/// Which capability flag a correction names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityId {
    /// Whether the model reasons. A consumer gates its reasoning policy on
    /// this, so a wrong value changes the bytes of every request.
    Reasoning,
    ToolCall,
    Attachment,
    InputModalities,
    OutputModalities,
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
// `UnpricedReason` and `RateValue` moved to `fusiform-protocol`, re-exported
// here so this crate's own code reads unchanged.
//
// They are the two types a consumer most needs and could least reach: a rate's
// three states have opposite consequences for a cap, and the reason a rate is
// unpriced decides whether a caller may proceed.
pub use fusiform_protocol::money::{RateValue, UnpricedReason};

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
    /// The embedded snapshot was loaded into an empty store.
    ///
    /// Not a poll, and that is why it has its own name. The instant is when the
    /// SNAPSHOT WAS FETCHED by the refresh script, not when the store was
    /// seeded — the upstream's content is known as of the former, and claiming
    /// the latter would assert fusiform looked at the upstream at install time.
    ///
    /// It confirms current values, so it can bound a window: the first real
    /// fetch that disagrees genuinely changed somewhere between the snapshot
    /// being cut and that poll. Without this the first disagreeing fetch has no
    /// left edge and has to be recorded as another `Seed` boundary, which
    /// claims the store came into existence twice and loses the link to the
    /// observation that detected the change.
    ///
    /// A separate variant rather than `Changed` with a detail string, because
    /// every other outcome is a POLL outcome. A consumer counting polls, or a
    /// future health check asking whether the loop is alive, must not be able
    /// to mistake a build-time fetch for a runtime one.
    Seeded,
}

impl ObservationOutcome {
    /// Whether this observation saw the currently held values STILL PUBLISHED,
    /// and may therefore act as the near edge of a subsequent era's window.
    ///
    /// The distinction is the whole point of recording failures separately: a
    /// poll that failed is not a poll that returned nothing.
    ///
    /// # It confirms PRESENCE at T, never CORRECTNESS at T
    ///
    /// The name reads like the second and means the first, so the boundary is
    /// worth stating where the predicate lives. A confirming observation says
    /// the upstream still published these bytes. It says nothing about whether
    /// the bytes were still true, because nothing in a fetch can.
    ///
    /// This is not hypothetical. Nineteen reseller entries in the live catalog
    /// publish a first-party price card that was retired weeks earlier: fetched,
    /// re-fetched, and confirming on every poll. An upstream that stops
    /// maintaining a row goes on serving it, and re-reading it is not a second
    /// opinion — it is the same claim, again.
    ///
    /// The consequence is on the WINDOW, which is why it matters here rather
    /// than in a comment about staleness. These edges make eras that say a
    /// value held continuously across an interval. For an abandoned row that
    /// interval is asserted with the same confidence as any other, so the
    /// history does not merely lack the truth — it affirms the falsehood over
    /// a bounded span, and an audit landing inside that span finds
    /// corroboration where it should find nothing.
    ///
    /// What the store CAN say about that row is when the fact last changed,
    /// which is measured rather than inferred. What it cannot say is whether
    /// anyone still stands behind it.
    ///
    /// # A surviving mutant here is EQUIVALENT, not a gap
    ///
    /// Rewriting this as `!matches!(self, Failed { .. })` survives the whole
    /// suite, and it should: the variants are exactly the four confirming ones
    /// plus `Failed`, so the two expressions compute the same predicate today.
    /// Recorded because a sweeper who finds it and reads it as missing coverage
    /// will go looking for a test of a case that cannot exist.
    ///
    /// The lookup is still the right form. The negation is only equivalent
    /// while `Failed` is the sole non-confirming outcome — an `Implausible` or
    /// `Refused` variant would make it silently wrong, and the list would
    /// force the decision at the moment the variant is added.
    pub fn confirms_current_values(&self) -> bool {
        CONFIRMING_OUTCOMES.contains(&self.wire_str())
    }

    /// The stored spelling of this outcome.
    ///
    /// One function rather than a match at each storage site: the wire string
    /// is what the database holds and what every query compares against, so it
    /// must have exactly one definition.
    pub const fn wire_str(&self) -> &'static str {
        match self {
            ObservationOutcome::Changed { .. } => "changed",
            ObservationOutcome::Unchanged => "unchanged",
            ObservationOutcome::NotModified => "not_modified",
            ObservationOutcome::Seeded => "seeded",
            ObservationOutcome::Failed { .. } => "failed",
        }
    }
}

/// The outcomes that CONFIRM the currently held values.
///
/// The single definition of the rule. Both the domain predicate above and the
/// store's SQL are built from this array, because the alternative — a `matches!`
/// in Rust and an `IN (...)` list in a query — is one belief written twice with
/// nothing comparing the two. A mutation to the Rust half survived exactly that
/// way: the helper was wrong and every test still passed, because the behaviour
/// lived in the SQL.
///
/// Membership rule: an outcome belongs here when fusiform LOOKED and learned
/// what the upstream currently says. A failed poll learned nothing. A seed
/// learned it at build time, which is still learning it.
pub const CONFIRMING_OUTCOMES: &[&str] = &["changed", "unchanged", "not_modified", "seeded"];

/// Every outcome's wire string, so a test can assert the two lists partition
/// the enum rather than drifting from it.
pub const ALL_OUTCOMES: &[&str] = &["changed", "unchanged", "not_modified", "seeded", "failed"];

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
    /// The body parsed, and describes so much less than the last one that
    /// fusiform will not believe it.
    ///
    /// A separate class from `Parse` because the document is well formed and
    /// nothing is wrong with fusiform's reading of it. What is wrong is the
    /// document, and the fix is on the upstream's side rather than in a parser.
    /// A consumer branching on `Parse` would be told to check for a schema
    /// change that did not happen.
    Implausible,
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
