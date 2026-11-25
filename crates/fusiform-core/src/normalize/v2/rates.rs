//! Admission for unconditional prices. Unit conversion applies to the
//! denominator only; the source's USD amount is not divided or estimated.

use serde_json::{value::RawValue, Number};

use super::raw::Object;
use super::{FindingKey, Findings, RecordFinding};
use crate::normalize::{rate_value, ChargeUnitEvidence, NormalizedRate};
use crate::{ChargeBasis, MediaTokenClass, ModelKey, RateCondition, RateValue, TokenClass};

pub(super) fn base_rates(
    model: &ModelKey,
    cost: &Object<'_>,
    evidence: &ChargeUnitEvidence,
    findings: &mut Findings,
) -> Vec<NormalizedRate> {
    use ChargeBasis::{PerMillionMediaTokens, PerMillionTokens};
    let input_audio = evidence.bills_input_audio_per_token(&model.provider_id, &model.model_id);
    let output_audio = evidence.bills_output_audio_per_token(&model.provider_id, &model.model_id);
    let mut prices = BasePrices {
        model: &model.model_id,
        findings,
        rates: Vec::new(),
    };
    for (modality, direction, basis, established) in [
        (
            "text",
            "input",
            Some(PerMillionTokens {
                class: TokenClass::Input,
            }),
            true,
        ),
        (
            "text",
            "output",
            Some(PerMillionTokens {
                class: TokenClass::Output,
            }),
            true,
        ),
        (
            "audio",
            "input",
            Some(PerMillionTokens {
                class: TokenClass::InputAudio,
            }),
            input_audio,
        ),
        (
            "audio",
            "output",
            Some(PerMillionTokens {
                class: TokenClass::OutputAudio,
            }),
            output_audio,
        ),
        (
            "image",
            "input",
            Some(PerMillionMediaTokens {
                class: MediaTokenClass::InputImage,
            }),
            true,
        ),
        (
            "image",
            "output",
            Some(PerMillionMediaTokens {
                class: MediaTokenClass::OutputImage,
            }),
            true,
        ),
        ("video", "input", None, true),
        (
            "video",
            "output",
            Some(PerMillionMediaTokens {
                class: MediaTokenClass::OutputVideo,
            }),
            true,
        ),
    ] {
        let Some(schedule) = cost
            .object(modality)
            .and_then(|entry| entry.object(direction))
        else {
            continue;
        };
        let role = format!("{modality}.{direction}");
        let unit = schedule.read::<String>("unit");
        let unit = match (unit.as_deref(), schedule.raw("unit")) {
            (Some(unit), _) => unit,
            (None, None) => "token",
            (None, Some(raw)) => raw.get(),
        };
        prices.add(&role, unit, basis, established, schedule.raw("price"));
        if let Some(cache) = schedule.object("cache") {
            prices.cache(&role, unit, input_audio, &cache);
        }
        if direction == "output" {
            let basis = (modality == "text").then_some(PerMillionTokens {
                class: TokenClass::Reasoning,
            });
            prices.add(
                &format!("{role}.reasoning"),
                unit,
                basis,
                true,
                schedule.raw("reasoning"),
            );
        }
        defer(&role, "overrides", &schedule, prices.model, prices.findings);
    }
    if let Some(search) = cost.object("web_search") {
        if search.raw("price").is_some() {
            prices.findings.add(
                FindingKey::UnservedCost {
                    role: "web_search".to_owned(),
                    unit: "thousand_searches".to_owned(),
                },
                Some(prices.model),
            );
        }
        defer(
            "web_search",
            "overrides",
            &search,
            prices.model,
            prices.findings,
        );
    }
    // A multiplier can cover more than one role; its schedule is not a base
    // price on any role. Keep its branch visible without changing any amount.
    if let Some(multipliers) = cost.array("multipliers") {
        for item in multipliers {
            let when = Object::parse(item)
                .ok()
                .and_then(|rule| rule.object("when"));
            let factor = Object::parse(item)
                .ok()
                .and_then(|rule| rule.object("factor"));
            let role = factor.filter(|factor| !factor.0.is_empty()).map_or_else(
                || "multipliers".to_owned(),
                |factor| format!("multipliers.{}", keys(&factor)),
            );
            prices.findings.add(
                FindingKey::ConditionDeferred {
                    role,
                    condition: condition(when),
                },
                Some(prices.model),
            );
        }
    }
    prices.rates
}

struct BasePrices<'a> {
    model: &'a str,
    findings: &'a mut Findings,
    rates: Vec<NormalizedRate>,
}

impl BasePrices<'_> {
    fn add(
        &mut self,
        role: &str,
        unit: &str,
        token_basis: Option<ChargeBasis>,
        established: bool,
        price: Option<&RawValue>,
    ) {
        let Some(price) = price else {
            return;
        };
        let basis = match (unit, role) {
            ("token", _) if established => token_basis,
            ("token", _) => {
                self.findings.add(
                    FindingKey::UnattributableChargeUnit {
                        role: role.to_owned(),
                    },
                    Some(self.model),
                );
                // A known legacy audio key can carry an unpriced marker, but
                // not a number unless documentation establishes token billing.
                if let Some(basis @ ChargeBasis::PerMillionTokens { .. }) = token_basis {
                    self.rates.push(NormalizedRate {
                        basis,
                        condition: RateCondition::Always,
                        value: RateValue::Unpriced {
                            reason: crate::UnpricedReason::UnknownChargeBasis,
                            inherited_from: None,
                        },
                    });
                }
                return;
            }
            ("character", "text.input") => Some(ChargeBasis::PerCharacter),
            ("second", "audio.input") => Some(ChargeBasis::PerSecond),
            ("minute", "audio.input") => Some(ChargeBasis::PerMinute),
            ("character" | "second" | "minute" | "image" | "megapixel", _) => None,
            _ => {
                self.findings.add(
                    FindingKey::UnknownCostUnit {
                        role: role.to_owned(),
                        value: unit.to_owned(),
                    },
                    Some(self.model),
                );
                return;
            }
        };
        let Some(basis) = basis else {
            self.findings.add(
                FindingKey::UnservedCost {
                    role: role.to_owned(),
                    unit: unit.to_owned(),
                },
                Some(self.model),
            );
            return;
        };
        let parsed = serde_json::from_str::<Number>(price.get())
            .map_err(|error| error.to_string())
            .and_then(|number| {
                if number.as_str().starts_with('-') {
                    // The price schema forbids negative USD amounts, including a
                    // negative literal that would otherwise round to zero.
                    let value = number.as_str().trim_start_matches('-');
                    if value.split(['e', 'E']).next().is_some_and(|digits| {
                        digits.bytes().any(|digit| matches!(digit, b'1'..=b'9'))
                    }) {
                        return Err("negative price".to_owned());
                    }
                }
                rate_value(self.model, role, number.as_str()).map_err(|error| error.to_string())
            });
        match parsed {
            Ok(mut value) => {
                if let RateValue::Priced { per, .. } = &mut value {
                    *per = basis.unit_denominator();
                }
                self.rates.push(NormalizedRate {
                    basis,
                    condition: RateCondition::Always,
                    value,
                });
            }
            Err(detail) => self.findings.add(
                FindingKey::RefusedPrice {
                    role: role.to_owned(),
                    detail,
                },
                Some(self.model),
            ),
        }
    }

    fn cache(&mut self, role: &str, unit: &str, input_audio: bool, cache: &Object<'_>) {
        let explicit = cache.object("explicit");
        let (read, write, established) = match role {
            "text.input" => (
                Some(ChargeBasis::PerMillionTokens {
                    class: TokenClass::CacheRead,
                }),
                Some(ChargeBasis::PerMillionTokens {
                    class: TokenClass::CacheWrite,
                }),
                true,
            ),
            "audio.input" => (
                Some(ChargeBasis::PerMillionMediaTokens {
                    class: MediaTokenClass::CachedInputAudio,
                }),
                None,
                input_audio,
            ),
            _ => (None, None, true),
        };
        self.add(
            &format!("{role}.cache.read"),
            unit,
            read,
            established,
            cache.raw("read"),
        );
        // The ordered TTL map's first figure is the default cache-write price;
        // don't sort TTLs or synthesize a price by averaging offers.
        let write_price = cache
            .raw("write")
            .or_else(|| explicit.as_ref()?.raw("write"))
            .map(first_price);
        self.add(
            &format!("{role}.cache.write"),
            unit,
            write,
            true,
            write_price,
        );
        for (path, price) in [
            ("cache.storage", cache.raw("storage")),
            (
                "cache.explicit.read",
                explicit.as_ref().and_then(|entry| entry.raw("read")),
            ),
            (
                "cache.explicit.storage",
                explicit.as_ref().and_then(|entry| entry.raw("storage")),
            ),
        ] {
            if price.is_some() {
                self.findings.add(
                    FindingKey::UnservedCost {
                        role: format!("{role}.{path}"),
                        unit: unit.to_owned(),
                    },
                    Some(self.model),
                );
            }
        }
    }
}

fn first_price(raw: &RawValue) -> &RawValue {
    Object::parse(raw)
        .ok()
        .and_then(|prices| prices.0.first().map(|(_, price)| *price))
        .unwrap_or(raw)
}

fn keys(fields: &Object<'_>) -> String {
    let mut keys = fields
        .0
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys.join("+")
}

fn condition(when: Option<Object<'_>>) -> String {
    when.filter(|when| !when.0.is_empty())
        .map_or_else(|| "<missing when>".to_owned(), |when| keys(&when))
}

fn defer(role: &str, field: &str, schedule: &Object<'_>, model: &str, findings: &mut Findings) {
    if let Some(rules) = schedule.array(field) {
        for rule in rules {
            let when = Object::parse(rule)
                .ok()
                .and_then(|rule| rule.object("when"));
            findings.add(
                FindingKey::ConditionDeferred {
                    role: role.to_owned(),
                    condition: condition(when),
                },
                Some(model),
            );
        }
    }
}

pub(super) fn defer_mode(model: &str, mode: &str, cost: &Object<'_>, findings: &mut Findings) {
    for (modality, value) in &cost.0 {
        match (modality.as_str(), Object::parse(value)) {
            ("text" | "image" | "audio" | "video", Ok(roles)) => {
                for (direction, _) in roles
                    .0
                    .iter()
                    .filter(|(name, _)| matches!(name.as_str(), "input" | "output"))
                {
                    findings.add(
                        FindingKey::ConditionDeferred {
                            role: format!("{modality}.{direction}"),
                            condition: format!("mode={mode}"),
                        },
                        Some(model),
                    );
                }
            }
            ("web_search" | "multipliers", _) => findings.add(
                FindingKey::ConditionDeferred {
                    role: modality.clone(),
                    condition: format!("mode={mode}"),
                },
                Some(model),
            ),
            _ => {}
        }
    }
}
