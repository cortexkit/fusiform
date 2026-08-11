//! Money, and the parse boundary where an upstream's floats stop existing.
//!
//! Every rule here is a rule the fleet learned from an incident, and the ones
//! that look pedantic are the ones that cost something. See
//! `docs/design/schema-and-store.md` §4.

use serde::{Deserialize, Serialize};

/// An ISO 4217 alphabetic code. A code, never a symbol, and never defaulted:
/// how a currency was established travels beside it in [`UnitProvenance`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CurrencyCode(String);

impl CurrencyCode {
    /// Three ASCII letters, upper-cased. Anything else is refused rather than
    /// normalised: a currency this type cannot recognise must not become one
    /// it can, because the failure would be a silently mispriced ledger.
    pub fn new(code: &str) -> Result<Self, MoneyError> {
        let trimmed = code.trim();
        if trimmed.len() != 3 || !trimmed.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(MoneyError::CurrencyCode(code.to_string()));
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoneyError {
    /// Not a plain decimal number, or out of range for the target integer.
    NotExact(String),
    /// A nonzero published price that would round to zero at the money
    /// resolution. Rounding it would fabricate a free model.
    RoundsToZero(String),
    /// A negative rate. No catalog publishes one; a corrupted snapshot must
    /// fail loudly here rather than flow into a consumer's signed money path.
    Negative(String),
    /// Not three ASCII letters.
    CurrencyCode(String),
}

impl std::fmt::Display for MoneyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoneyError::NotExact(v) => write!(f, "rate {v} cannot scale exactly to minor units"),
            MoneyError::RoundsToZero(v) => write!(f, "nonzero rate {v} would round to zero"),
            MoneyError::Negative(v) => write!(f, "rate {v} is negative"),
            MoneyError::CurrencyCode(v) => write!(f, "currency {v:?} is not an ISO 4217 code"),
        }
    }
}

impl std::error::Error for MoneyError {}

/// Scale a decimal string to integer minor units at `exponent`.
///
/// The input is a decimal STRING, never a float: the JSON number's
/// shortest-roundtrip form is scaled exactly by powers of ten, so no binary
/// floating point participates in the arithmetic.
///
/// Precision beyond the target resolution rounds HALF-EVEN. Real catalogs
/// carry upstream float artifacts — models.dev publishes `0.8299999999999998`
/// where `0.83` is meant, and 72 such values were present on 2026-08-11 — and
/// the rounding error is below the money resolution. The one dangerous case
/// stays a loud error: a NONZERO rate that would round to ZERO is refused,
/// because rounding it would fabricate a free model.
pub fn decimal_str_to_minor_units(s: &str, exponent: u8) -> Result<i64, MoneyError> {
    let err = || MoneyError::NotExact(s.to_string());

    // serde prints tiny values in exponent form, e.g. `1e-7`.
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(idx) => {
            let parsed: i32 = s[idx + 1..].parse().map_err(|_| err())?;
            (&s[..idx], parsed)
        }
        None => (s, 0),
    };

    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches(['-', '+']);
    let (int_part, frac_part) = match mantissa.find('.') {
        Some(idx) => (&mantissa[..idx], &mantissa[idx + 1..]),
        None => (mantissa, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(err());
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return Err(err());
    }

    // Trailing zeros in the fraction carry no value, so drop them before the
    // significand is assembled. This is exactness, not tidiness: `0.0000000025`
    // and `0.0000000025000…` are the same number, and without this the padded
    // form builds a 39-digit significand that does not fit i128 and is refused
    // while the bare form is priced.
    //
    // A rate that is accepted or refused depending on how the upstream chose to
    // render it is the same defect class as change detection that fires on
    // reformatting: the producer's behaviour must not depend on a serialiser's
    // preferences.
    let frac_part = frac_part.trim_end_matches('0');

    let digits: String = format!("{int_part}{frac_part}");
    let trimmed = digits.trim_start_matches('0');
    let value: i128 = if trimmed.is_empty() {
        0
    } else {
        trimmed.parse().map_err(|_| err())?
    };

    // value × 10^(exp - frac_len) currency units
    //   → minor = value × 10^(exponent + exp - frac_len)
    let shift = i32::from(exponent) + exp - frac_part.len() as i32;
    let scaled: i128 = if shift >= 0 {
        let factor = 10i128
            .checked_pow(u32::try_from(shift).map_err(|_| err())?)
            .ok_or_else(err)?;
        value.checked_mul(factor).ok_or_else(err)?
    } else {
        let divisor = 10i128
            .checked_pow(u32::try_from(-shift).map_err(|_| err())?)
            .ok_or_else(err)?;
        let quotient = value / divisor;
        let remainder = value % divisor;
        // Compare the remainder against HALF the divisor rather than doubling
        // the remainder. Both express the same test, but doubling can overflow
        // i128 for a ~38-digit fractional significand while halving never can:
        // `divisor` is a power of ten at least 10, so `divisor / 2` is exact
        // and strictly smaller than an already-representable value.
        //
        // This is not a stylistic preference. The doubling form rejects
        // `0.00000000086` as an overflow, and that value has a correct answer:
        // 0.86 nanodollars, which rounds half-even to 1. Refusing a rate that
        // can be represented is the same class of defect as fabricating one —
        // a loud error is only the right answer when there is no right answer.
        let half = divisor / 2;
        let rounded = match remainder.cmp(&half) {
            std::cmp::Ordering::Greater => quotient + 1,
            std::cmp::Ordering::Less => quotient,
            // Exact half: round to even.
            std::cmp::Ordering::Equal => {
                if quotient % 2 == 0 {
                    quotient
                } else {
                    quotient + 1
                }
            }
        };
        if rounded == 0 && value != 0 {
            return Err(MoneyError::RoundsToZero(s.to_string()));
        }
        rounded
    };

    let scaled = if negative { -scaled } else { scaled };
    if scaled < 0 {
        return Err(MoneyError::Negative(s.to_string()));
    }
    i64::try_from(scaled).map_err(|_| err())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same number rendered two ways must price the same.
    ///
    /// Found by mutation testing: a padded significand overflowed i128 and was
    /// refused while the bare form priced fine, so acceptance depended on the
    /// upstream's serialiser rather than on the value.
    #[test]
    fn rendering_does_not_change_whether_a_rate_is_priceable() {
        let bare = decimal_str_to_minor_units("0.0000000025", NANO_EXPONENT);
        let padded = decimal_str_to_minor_units(
            "0.00000000250000000000000000000000000000000000000",
            NANO_EXPONENT,
        );
        assert_eq!(
            bare, padded,
            "a trailing-zero rendering changed the outcome"
        );
        assert_eq!(bare, Ok(2));

        // And at the scale where whole units are involved.
        assert_eq!(
            decimal_str_to_minor_units("3", NANO_EXPONENT),
            decimal_str_to_minor_units("3.000000000000000000000000", NANO_EXPONENT)
        );
    }

    #[test]
    fn plain_decimals_scale_exactly() {
        assert_eq!(
            decimal_str_to_minor_units("3", NANO_EXPONENT),
            Ok(3_000_000_000)
        );
        assert_eq!(
            decimal_str_to_minor_units("0.3", NANO_EXPONENT),
            Ok(300_000_000)
        );
        assert_eq!(
            decimal_str_to_minor_units("3.75", NANO_EXPONENT),
            Ok(3_750_000_000)
        );
        // A real long-but-legitimate models.dev rate, which must survive intact.
        assert_eq!(
            decimal_str_to_minor_units("0.003625", NANO_EXPONENT),
            Ok(3_625_000)
        );
    }

    /// The artifacts are real and current: these three strings were present in
    /// the 2026-08-11 payload. The intended decimal must be recovered exactly.
    #[test]
    fn upstream_float_artifacts_round_half_even() {
        assert_eq!(
            decimal_str_to_minor_units("0.024999999999999998", NANO_EXPONENT),
            Ok(25_000_000)
        );
        assert_eq!(
            decimal_str_to_minor_units("0.39999999999999997", NANO_EXPONENT),
            Ok(400_000_000)
        );
        assert_eq!(
            decimal_str_to_minor_units("1.5999999999999999", NANO_EXPONENT),
            Ok(1_600_000_000)
        );
    }

    #[test]
    fn half_even_ties_go_to_even() {
        // 1.5 minor units → 2 (even), 2.5 → 2 (even). A naive half-up rounding
        // returns 2 and 3, so this test fails against it.
        assert_eq!(
            decimal_str_to_minor_units("0.0000000015", NANO_EXPONENT),
            Ok(2)
        );
        assert_eq!(
            decimal_str_to_minor_units("0.0000000025", NANO_EXPONENT),
            Ok(2)
        );
        assert_eq!(
            decimal_str_to_minor_units("0.0000000035", NANO_EXPONENT),
            Ok(4)
        );
    }

    #[test]
    fn a_true_zero_is_not_a_rounded_zero() {
        // Zero stays zero: it was not rounded, it was stated.
        assert_eq!(decimal_str_to_minor_units("0", NANO_EXPONENT), Ok(0));
        assert_eq!(
            decimal_str_to_minor_units("0.000000000", NANO_EXPONENT),
            Ok(0)
        );
    }

    #[test]
    fn a_nonzero_rate_never_becomes_free() {
        // The dangerous case: a real price below the money resolution. Rounding
        // it to zero would fabricate a free model, so it is refused instead.
        assert_eq!(
            decimal_str_to_minor_units("1e-10", NANO_EXPONENT),
            Err(MoneyError::RoundsToZero("1e-10".to_string()))
        );
        assert_eq!(
            decimal_str_to_minor_units("0.0000000001", NANO_EXPONENT),
            Err(MoneyError::RoundsToZero("0.0000000001".to_string()))
        );
    }

    #[test]
    fn negative_rates_are_refused() {
        assert_eq!(
            decimal_str_to_minor_units("-15", NANO_EXPONENT),
            Err(MoneyError::Negative("-15".to_string()))
        );
    }

    /// A pathological significand must round CORRECTLY, not error.
    ///
    /// 47 fractional digits reach the rounding branch with divisor 10^38,
    /// where the natural `remainder * 2 > divisor` formulation overflows i128.
    /// The value is nonetheless perfectly representable: 0.86 nanodollars,
    /// which rounds half-even to 1. An implementation that refuses it is
    /// rejecting a rate it could have priced.
    #[test]
    fn pathological_significand_rounds_rather_than_overflowing() {
        assert_eq!(
            decimal_str_to_minor_units(
                "0.00000000086000000000000000000000000000000000000",
                NANO_EXPONENT
            ),
            Ok(1)
        );
        // And the other side of the same boundary: 0.4 nanodollars rounds to 0,
        // which is a nonzero rate rounding to zero and therefore refused.
        assert_eq!(
            decimal_str_to_minor_units(
                "0.00000000040000000000000000000000000000000000000",
                NANO_EXPONENT
            ),
            Err(MoneyError::RoundsToZero(
                "0.00000000040000000000000000000000000000000000000".to_string()
            ))
        );
        // A sane long fraction still rounds normally.
        assert_eq!(
            decimal_str_to_minor_units("0.99999999999999999999999999999999999999", NANO_EXPONENT),
            Ok(1_000_000_000)
        );
    }

    /// The tie case at the overflow boundary: exactly half a minor unit, with a
    /// significand large enough that the doubling formulation would wrap.
    /// Half-even must still apply, and to the EVEN side.
    #[test]
    fn ties_round_to_even_even_at_the_overflow_boundary() {
        // 0.5 nanodollars -> 0 (even), but zero from a nonzero rate is refused.
        assert_eq!(
            decimal_str_to_minor_units(
                "0.00000000050000000000000000000000000000000000000",
                NANO_EXPONENT
            ),
            Err(MoneyError::RoundsToZero(
                "0.00000000050000000000000000000000000000000000000".to_string()
            ))
        );
        // 1.5 nanodollars -> 2 (even).
        assert_eq!(
            decimal_str_to_minor_units(
                "0.00000000150000000000000000000000000000000000000",
                NANO_EXPONENT
            ),
            Ok(2)
        );
        // 2.5 nanodollars -> 2 (even), not 3.
        assert_eq!(
            decimal_str_to_minor_units(
                "0.00000000250000000000000000000000000000000000000",
                NANO_EXPONENT
            ),
            Ok(2)
        );
    }

    #[test]
    fn non_numeric_input_is_refused() {
        assert!(decimal_str_to_minor_units("", NANO_EXPONENT).is_err());
        assert!(decimal_str_to_minor_units("abc", NANO_EXPONENT).is_err());
        assert!(decimal_str_to_minor_units("1.2.3", NANO_EXPONENT).is_err());
        assert!(decimal_str_to_minor_units("0x10", NANO_EXPONENT).is_err());
    }

    #[test]
    fn currency_codes_are_validated_not_coerced() {
        assert_eq!(CurrencyCode::new("usd").unwrap().as_str(), "USD");
        assert_eq!(CurrencyCode::new(" CNY ").unwrap().as_str(), "CNY");
        assert!(CurrencyCode::new("US").is_err());
        assert!(CurrencyCode::new("US$").is_err());
        assert!(CurrencyCode::new("DOLLAR").is_err());
    }
}
