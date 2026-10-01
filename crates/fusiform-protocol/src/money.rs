//! The money vocabulary a consumer must decode, on the crate a consumer can
//! depend on.
//!
//! # Why these live here rather than in `fusiform-core`
//!
//! `catalog.get` types its envelope and left its payload as
//! `serde_json::Value`, so the types a reader actually needs — what a rate IS —
//! lived in the domain crate, which drags in normalisation and a JSON parser
//! configured for arbitrary precision. No consumer could depend on that, so
//! every consumer wrote its own decoder.
//!
//! A second copy of this vocabulary is worse than no copy, for a reason a
//! consumer named precisely: adding a variant to [`UnpricedReason`] would not FAIL a
//! hand-written decoder, it would fall into a default arm and price something
//! that should have been refused. A copy that still parses is the dangerous
//! kind, and it collapses exactly the absent/zero/unknown distinction this
//! catalog exists to keep apart.
//!
//! Nothing here parses. The decimal boundary — where an upstream's floats stop
//! existing — stays in `fusiform-core`, because it needs a JSON parser this
//! crate must never carry.

use serde::{Deserialize, Serialize};

/// An ISO 4217 alphabetic code. A code, never a symbol, and never defaulted:
/// how a currency was established travels beside it in [`UnitProvenance`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CurrencyCode(String);

/// A currency code that is not three ASCII letters.
///
/// Its own type rather than a variant of the domain crate's money error,
/// because the constructor has to travel with the type it constructs — the
/// inner field is private, so a caller outside this module could not build one
/// otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidCurrencyCode(pub String);

impl std::fmt::Display for InvalidCurrencyCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} is not an ISO 4217 alphabetic code (three ASCII letters)",
            self.0
        )
    }
}

impl std::error::Error for InvalidCurrencyCode {}

impl CurrencyCode {
    /// Three ASCII letters, upper-cased. Anything else is refused rather than
    /// normalised: a currency this type cannot recognise must not become one
    /// it can, because the failure would be a silently mispriced ledger.
    pub fn new(code: &str) -> Result<Self, InvalidCurrencyCode> {
        let trimmed = code.trim();
        if trimmed.len() != 3 || !trimmed.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(InvalidCurrencyCode(code.to_string()));
        }
        Ok(Self(trimmed.to_ascii_uppercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How the unit attached to an amount was established.
///
/// A producer that converts a currency stamps an invented rate with producer
/// authority, so fusiform never converts. But refusing to convert is not
/// enough on its own: models.dev states no currency at all, so serving USD
/// without saying *why* would launder a convention into a fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum UnitProvenance {
    /// The source stated the currency.
    Stated,
    /// Fusiform applied a named policy declared in its own source and
    /// versioned with it, so "why does the catalog say USD" resolves to an
    /// auditable rule rather than a habit.
    AssumedByPolicy { policy: PolicyId },
    /// No statement and no policy covers it. The rate is unpriced; this is
    /// what stops a non-USD provider being silently priced in dollars.
    Unknown,
}

/// The identifier of a unit policy. Declared in fusiform's source, versioned
/// with it, and carried on every amount that depended on it — so a policy
/// change can find every row it touched.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PolicyId(pub String);

impl PolicyId {
    /// Rates from the models.dev source carry no currency field at all. USD is
    /// the upstream's unstated convention; this policy is fusiform saying so
    /// out loud rather than assuming it silently.
    pub fn models_dev_usd_v1() -> Self {
        Self("models-dev-usd-v1".to_string())
    }
}

/// An exact amount: an integer count of minor units, the exponent that scales
/// them, and the currency they are denominated in.
///
/// Floats do not appear in this type and do not survive normalisation. They
/// exist only at the JSON parse boundary, where the upstream forces them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Amount {
    /// The value is `units × 10^(-exponent)` of `currency`.
    pub units: i64,
    /// Stated, never assumed. A consumer that has to infer the scale of an
    /// integer will eventually infer it wrong.
    pub exponent: u8,
    pub currency: CurrencyCode,
    pub unit_provenance: UnitProvenance,
}

/// Nanodollars, the scale the fleet's existing money paths already use: an
/// exponent of 9 means `units` counts billionths of a currency unit.
pub const NANO_EXPONENT: u8 = 9;

/// Why a rate could not be priced.
///
/// Adopted from the metering module's existing enum rather than paralleled, so
/// nothing has to be mapped at the boundary.
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

/// The minimum a call is charged regardless of how little it consumes.
///
/// Three states rather than `Option<i64>`, and the distinction is the whole
/// point. models.dev publishes no floor data at all, so an `Option` would be
/// `None` on every row — meaning both "no minimum applies" and "nobody
/// established whether one does". A consumer reads that `None` as no-minimum
/// and prices accordingly, which is exactly what happens today with no field
/// at all: the type would cost a breaking change, buy nothing, and LOOK like
/// the case was handled.
///
/// So every rate carries `Unknown` today. That is not a placeholder — it is the
/// true state, and it is actionable in a way silence is not: it tells a
/// consumer they cannot conclude absence.
///
/// Why it must exist before a floor is measured: a missing FIELD in a type
/// cannot be refused at a consumer's seam. A wrong value can be caught by
/// strictness; an absent dimension arrives looking ordinary, and no strictness
/// anywhere catches what does not arrive. The type is the only place the
/// question can be asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Floor {
    /// Nobody has established whether this call has a minimum charge. The
    /// state of every rate fusiform serves today.
    Unknown,
    /// Established that no minimum applies.
    None,
    /// A measured minimum, in the same units and exponent as the rate's
    /// [`Amount`].
    Minimum { units: i64 },
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
        /// Flattened, because the SERVED bytes are flat.
        ///
        /// This was wrong when the type moved here, and a consumer found it by
        /// building against the crate rather than reading the notice: its
        /// decoder refused every priced rate fusiform serves, with
        /// `missing field "amount"`.
        ///
        /// The cause is that this was a DOMAIN type promoted to a wire type
        /// without checking that it described the wire. The server never
        /// serializes through it — `json_rate` in fusiform-store hand-writes
        /// the stored string, and the serve path passes those bytes through —
        /// so the two shapes were free to disagree and nothing compared them.
        ///
        /// The golden fixture could not catch it. It is generated BY the real
        /// serve path, so it faithfully recorded bytes this type cannot decode,
        /// and its byte-identical result across the move confirmed the wire had
        /// not changed while saying nothing about whether the type matched.
        /// Two paths answering "what is a rate on the wire", neither compared
        /// to the other.
        #[serde(flatten)]
        amount: Amount,
        /// Absent on rows written before the floor dimension existed, which
        /// decode as [`Floor::Unknown`] rather than as no-minimum — the same
        /// distinction the enum exists to hold, applied to its own rollout.
        #[serde(default = "Floor::unknown")]
        floor: Floor,
        /// Present when this price is NOT this provider's own. The same
        /// marker, with the same meaning, rides on [`RateValue::StatedZero`]
        /// and [`RateValue::Unpriced`]: whatever state a borrowed rate is in,
        /// the state is the named row's statement, not this provider's.
        ///
        /// models.dev keeps an open-weight model's list price on the
        /// ORIGINATOR's provider entry, so a reseller serving the same weights
        /// often publishes nothing. Where that happens fusiform serves the
        /// creator's price and says so here.
        ///
        /// A CONSUMER MUST NOT READ THIS AS THE COST OF THE ROUTE. It is what
        /// the named provider charges for the same weights, which is reference
        /// data — the serving provider may be a subscription plane where the
        /// marginal cost is zero, and fusiform cannot tell those apart.
        ///
        /// Typed here rather than left as raw JSON because it was raw JSON,
        /// injected by the serve path, and therefore INVISIBLE to every
        /// consumer decoding through this crate: serde drops unknown fields
        /// silently, so the marker separating a published price from a borrowed
        /// one vanished at exactly the seam it exists to guard. A consumer
        /// reported their store collapsing that distinction; this type is why
        /// it was easy to collapse.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inherited_from: Option<InheritedFrom>,
    },
    /// The source stated exactly zero. Not "free" — a stated zero. Whether a
    /// zero is a real price is the consumer's policy, not fusiform's.
    StatedZero {
        /// Present when the zero is ANOTHER row's statement, borrowed by
        /// open-weight inheritance or through a curated alias. Without it a
        /// borrowed zero decodes as this provider stating the model is free,
        /// which is the published/borrowed collapse [`InheritedFrom`] exists
        /// to prevent. Absent on the provider's own zero, so those bytes are
        /// `{"state":"stated_zero"}` exactly as before the field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inherited_from: Option<InheritedFrom>,
    },
    Unpriced {
        reason: UnpricedReason,
        /// Present when the unpriced state is another row's, borrowed the same
        /// way as a price. See the field of the same name on
        /// [`RateValue::StatedZero`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inherited_from: Option<InheritedFrom>,
    },
}

/// Whose price this is, when it is not the serving provider's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InheritedFrom {
    /// The provider whose published price this is.
    pub provider_id: String,
    /// The weights family both rows share, as the upstream names it.
    ///
    /// Under `basis: "alias"` this is the TARGET row's `model.family`, and an
    /// empty string only when the target publishes no family at all.
    pub family: String,
    /// Why the two rows are relatable at all.
    ///
    /// - `open_weights`: two providers serving the same OPEN weights serve the
    ///   same artefact, which is what makes one's price say anything about the
    ///   other's row. A closed model served by two providers is two offerings
    ///   sharing a name.
    /// - `alias`: the serving id is a curated alias of another row (see
    ///   `CatalogGetResponse::aliased`). An alias is an authored identity
    ///   claim, served on current reads only, and a real row for the alias id
    ///   always wins over it.
    pub basis: String,
    /// The model id of the row whose price this is, when it differs from the
    /// serving row's.
    ///
    /// Set under `basis: "alias"`, where the alias id and the target id are
    /// different strings. Absent under `open_weights`, where both rows carry
    /// the same model id, so rates inherited that way serialize exactly as
    /// they did before this field existed.
    ///
    /// A rate carrying this is the target's API LIST PRICE, not the cost of
    /// the route that serves the alias id: that route may be a subscription
    /// plane whose marginal cost is zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

impl Floor {
    /// The serde default for a rate written before floors existed.
    ///
    /// A free function rather than `Default`, because deriving `Default` would
    /// make `Floor::default()` available everywhere and the whole argument for
    /// this type is that a floor must be stated rather than assumed.
    pub fn unknown() -> Self {
        Floor::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider's OWN zero and unpriced rate keep the exact bytes they had
    /// before those states could carry a marker, in both directions.
    #[test]
    fn an_unmarked_zero_and_unpriced_keep_their_bytes() {
        for bytes in [
            r#"{"state":"stated_zero"}"#,
            r#"{"state":"unpriced","reason":"missing_rate"}"#,
        ] {
            let decoded: RateValue = serde_json::from_str(bytes).expect("decodes");
            assert_eq!(serde_json::to_string(&decoded).unwrap(), bytes);
        }
        assert_eq!(
            serde_json::from_str::<RateValue>(r#"{"state":"stated_zero"}"#).unwrap(),
            RateValue::StatedZero {
                inherited_from: None
            }
        );
    }

    /// A borrowed zero keeps its marker through the type.
    #[test]
    fn a_marked_zero_round_trips_its_marker() {
        let bytes = r#"{"state":"stated_zero","inherited_from":{"provider_id":"zai","family":"glm","basis":"open_weights"}}"#;
        let decoded: RateValue = serde_json::from_str(bytes).expect("decodes");
        let RateValue::StatedZero {
            inherited_from: Some(origin),
        } = &decoded
        else {
            panic!("the marker must decode: {decoded:?}");
        };
        assert_eq!(origin.provider_id, "zai");
        assert_eq!(serde_json::to_string(&decoded).unwrap(), bytes);
    }
}
