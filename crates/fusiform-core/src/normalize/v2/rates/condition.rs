use serde_json::{value::RawValue, Number};

use super::super::raw::Object;
use crate::normalize::{exact_decimal_key, is_servable_mode_name};
use crate::RateCondition;

pub(super) fn parse(when: &Object<'_>) -> Result<RateCondition, String> {
    let [(key, predicate)] = when.0.as_slice() else {
        return if when.0.is_empty() {
            Ok(RateCondition::Always)
        } else {
            Err(format!("combined conditions: {}", describe(when)))
        };
    };
    let fields = Object::parse(predicate)
        .map_err(|_| format!("{key} predicate is not an object: {}", predicate.get()))?;
    let [(op, value)] = fields.0.as_slice() else {
        return Err(format!("{key} requires one operator: {}", predicate.get()));
    };
    match (key.as_str(), op.as_str()) {
        ("context", "gt" | "gte") => {
            // Both spellings describe an integer-token boundary. The fact
            // key's threshold is exclusive, not the first count in the band.
            let tokens = serde_json::from_str::<u64>(value.get())
                .ok()
                .and_then(|n| n.checked_sub(u64::from(op == "gte")))
                .filter(|&n| n > 0)
                .ok_or_else(|| {
                    format!(
                        "context {op} {} has no positive exclusive threshold",
                        value.get()
                    )
                })?;
            Ok(RateCondition::MinContextTokens { tokens })
        }
        ("service_tier", "equals") => {
            let name =
                read_name(value).ok_or_else(|| format!("invalid service_tier: {}", value.get()))?;
            Ok(RateCondition::Mode { name })
        }
        ("context" | "service_tier", _) => Err(format!("unsupported {key} operator {op}")),
        _ => Err(format!("unsupported condition {key} ({op})")),
    }
}

pub(super) fn tier_hint(when: &Object<'_>) -> Option<String> {
    read_name(when.object("service_tier")?.raw("equals")?)
}

pub(super) fn read_name(raw: &RawValue) -> Option<String> {
    serde_json::from_str::<String>(raw.get())
        .ok()
        .filter(|name| is_servable_mode_name(name))
}

fn describe(when: &Object<'_>) -> String {
    when.0
        .iter()
        .map(|(key, value)| format!("{key}: {}", value.get()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Decimal {
    coefficient: u128,
    power: i128,
}

impl Decimal {
    pub(super) fn parse(raw: &RawValue) -> Result<Self, String> {
        let number = serde_json::from_str::<Number>(raw.get()).map_err(|e| e.to_string())?;
        Self::from_number(&number)
    }

    fn from_number(number: &Number) -> Result<Self, String> {
        let invalid = || format!("decimal out of range: {number}");
        let (negative, digits, power) = exact_decimal_key(number.as_str()).ok_or_else(invalid)?;
        if negative {
            return Err(format!("negative price or factor: {number}"));
        }
        let coefficient = if digits.is_empty() {
            0
        } else {
            digits.parse().map_err(|_| invalid())?
        };
        Ok(Self { coefficient, power })
    }

    pub(super) fn product(self, rhs: Self) -> Result<Self, String> {
        if self.coefficient == 0 || rhs.coefficient == 0 {
            return Ok(Self {
                coefficient: 0,
                power: 0,
            });
        }
        let overflow = || "decimal product out of range".to_owned();
        let mut coefficient = self
            .coefficient
            .checked_mul(rhs.coefficient)
            .ok_or_else(overflow)?;
        let mut power = self.power.checked_add(rhs.power).ok_or_else(overflow)?;
        while coefficient % 10 == 0 {
            coefficient /= 10;
            power = power.checked_add(1).ok_or_else(overflow)?;
        }
        Ok(Self { coefficient, power })
    }

    pub(super) fn literal(self) -> String {
        format!("{}e{}", self.coefficient, self.power)
    }
}

pub(super) fn factor_at<'a>(
    factor: &'a RawValue,
    path: &str,
    ttl: Option<&str>,
) -> Option<&'a RawValue> {
    let mut node = factor;
    for key in path.split('.').chain(ttl) {
        let Ok(branch) = Object::parse(node) else {
            return Some(node);
        };
        node = branch.raw(key)?;
    }
    if Object::parse(node).is_ok() {
        None
    } else {
        Some(node)
    }
}
