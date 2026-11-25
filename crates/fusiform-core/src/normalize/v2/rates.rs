//! Project only price schedules that the existing fact keys can express.

mod condition;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::value::RawValue;

use super::raw::Object;
use super::{FindingKey, Findings, RecordFinding};
use crate::normalize::{
    is_servable_mode_name, rate_value, ChargeUnitEvidence, NormalizedRate, RefusedFactScope,
};
use crate::{ChargeBasis, MediaTokenClass, ModelKey, RateCondition, RateValue, TokenClass};
use condition::{factor_at, Decimal};

pub(super) fn normalize_rates(
    model: &ModelKey,
    cost: Option<&RawValue>,
    modes: Option<&Object<'_>>,
    evidence: &ChargeUnitEvidence,
    findings: &mut Findings,
    refused: &mut BTreeSet<RefusedFactScope>,
) -> Vec<NormalizedRate> {
    let mut prices = Prices {
        model: &model.model_id,
        input_audio: evidence.bills_input_audio_per_token(&model.provider_id, &model.model_id),
        output_audio: evidence.bills_output_audio_per_token(&model.provider_id, &model.model_id),
        findings,
        refused,
        rates: Vec::new(),
        base: BTreeMap::new(),
        overrides: BTreeMap::new(),
        modes: Vec::new(),
        mode_prices: BTreeMap::new(),
        shadowed_prices: BTreeMap::new(),
    };
    if let Some(modes) = modes {
        prices.read_modes(modes);
    }
    if let Some(cost) = cost.and_then(|raw| prices.cost(raw, None)) {
        prices.read_cost(&cost, None);
        prices.read_multipliers(&cost);
    }
    prices.finish()
}

struct Mode<'raw> {
    name: String,
    tier: String,
    cost: Option<&'raw RawValue>,
}

struct Price<'raw> {
    path: String,
    ttl: Option<String>,
    raw: &'raw RawValue,
}

struct Prices<'a, 'raw> {
    model: &'a str,
    input_audio: bool,
    output_audio: bool,
    findings: &'a mut Findings,
    refused: &'a mut BTreeSet<RefusedFactScope>,
    rates: Vec<NormalizedRate>,
    base: BTreeMap<ChargeBasis, Price<'raw>>,
    overrides: BTreeMap<(RateCondition, ChargeBasis), Price<'raw>>,
    modes: Vec<Mode<'raw>>,
    mode_prices: BTreeMap<(String, ChargeBasis), Decimal>,
    shadowed_prices: BTreeMap<(String, ChargeBasis), Decimal>,
}

struct Schedule<'a> {
    role: &'static str,
    unit: &'a str,
    token_basis: Option<ChargeBasis>,
    established: bool,
}

impl<'raw> Prices<'_, 'raw> {
    fn finding(&mut self, key: FindingKey) {
        self.findings.add(key, Some(self.model));
    }

    fn refuse(&mut self, scope: RefusedFactScope, role: &str, detail: impl Into<String>) {
        self.refused.insert(scope);
        self.finding(FindingKey::RefusedPrice {
            role: role.to_owned(),
            detail: detail.into(),
        });
    }

    fn cost(&mut self, raw: &'raw RawValue, owner: Option<&str>) -> Option<Object<'raw>> {
        match Object::parse(raw) {
            Ok(cost) => Some(cost),
            Err(_) => {
                self.refuse(scope(owner), "cost", "cost is not an object");
                None
            }
        }
    }

    fn read_modes(&mut self, modes: &Object<'raw>) {
        for (name, raw) in &modes.0 {
            let Ok(mode) = Object::parse(raw) else {
                self.refuse(scope(Some(name)), name, "mode is not an object");
                continue;
            };
            let tier = if let Some(raw) = mode.raw("service_tier") {
                let Some(tier) = condition::read_name(raw) else {
                    self.refuse(
                        scope(Some(name)),
                        name,
                        format!("invalid service_tier: {}", raw.get()),
                    );
                    continue;
                };
                tier
            } else {
                name.clone()
            };
            let cost = mode.raw("cost");
            self.modes.push(Mode {
                name: name.clone(),
                tier,
                cost,
            });
            if !is_servable_mode_name(name) {
                self.refuse(scope(Some(name)), name, "mode name cannot form a fact key");
            } else if let Some(cost) = cost.and_then(|raw| self.cost(raw, Some(name))) {
                self.read_cost(&cost, Some(name));
                if cost.raw("multipliers").is_some() {
                    self.refuse(
                        scope(Some(name)),
                        name,
                        "multipliers inside a named mode are not representable",
                    );
                }
            }
        }
    }

    fn read_cost(&mut self, cost: &Object<'raw>, owner: Option<&str>) {
        use ChargeBasis::{PerMillionMediaTokens, PerMillionTokens};
        for (role, token_basis, established) in [
            (
                "text.input",
                Some(PerMillionTokens {
                    class: TokenClass::Input,
                }),
                true,
            ),
            (
                "text.output",
                Some(PerMillionTokens {
                    class: TokenClass::Output,
                }),
                true,
            ),
            (
                "audio.input",
                Some(PerMillionTokens {
                    class: TokenClass::InputAudio,
                }),
                self.input_audio,
            ),
            (
                "audio.output",
                Some(PerMillionTokens {
                    class: TokenClass::OutputAudio,
                }),
                self.output_audio,
            ),
            (
                "image.input",
                Some(PerMillionMediaTokens {
                    class: MediaTokenClass::InputImage,
                }),
                true,
            ),
            (
                "image.output",
                Some(PerMillionMediaTokens {
                    class: MediaTokenClass::OutputImage,
                }),
                true,
            ),
            ("video.input", None, true),
            (
                "video.output",
                Some(PerMillionMediaTokens {
                    class: MediaTokenClass::OutputVideo,
                }),
                true,
            ),
            ("web_search", None, true),
        ] {
            let block = if let Some((modality, direction)) = role.split_once('.') {
                cost.object(modality)
                    .and_then(|entry| entry.object(direction))
            } else {
                cost.object(role)
            };
            let Some(block) = block else { continue };
            let unit = block.read::<String>("unit");
            let unit = match (unit.as_deref(), block.raw("unit")) {
                (Some(unit), _) => unit,
                (None, Some(raw)) => raw.get(),
                (None, None) if role == "web_search" => "thousand_searches",
                (None, None) => "token",
            };
            let schedule = Schedule {
                role,
                unit,
                token_basis,
                established,
            };
            if block.raw("when").is_some() {
                self.refuse(
                    scope(owner),
                    role,
                    "when belongs on an override, not the base price",
                );
                continue;
            }
            self.read_fields(&schedule, &block, &RateCondition::Always, owner);
            if let Some(raw) = block.raw("overrides") {
                let Ok(rules) = serde_json::from_str::<Vec<&RawValue>>(raw.get()) else {
                    self.refuse(scope(owner), role, "overrides is not an array");
                    continue;
                };
                for rule in rules {
                    let Ok(rule) = Object::parse(rule) else {
                        self.refuse(scope(owner), role, "override is not an object");
                        continue;
                    };
                    if let Some(condition) = self.read_condition(&rule, role, owner) {
                        self.read_fields(&schedule, &rule, &condition, owner);
                    }
                }
            }
        }
    }

    fn read_condition(
        &mut self,
        rule: &Object<'_>,
        role: &str,
        owner: Option<&str>,
    ) -> Option<RateCondition> {
        let Some(when) = rule.object("when") else {
            self.refuse(scope(owner), role, "missing or malformed when");
            return None;
        };
        match condition::parse(&when) {
            Ok(condition) if owner.is_none() || condition == RateCondition::Always => {
                Some(condition)
            }
            Ok(_) => {
                self.refuse(
                    scope(owner),
                    role,
                    format!(
                        "condition inside named mode cannot be represented: {}",
                        rule.raw("when").unwrap().get()
                    ),
                );
                None
            }
            Err(detail) => {
                if let (None, Some(tier)) = (owner, condition::tier_hint(&when)) {
                    for (name, _) in self.targets(&tier) {
                        self.refuse(scope(Some(&name)), role, &detail);
                    }
                } else {
                    let refused = if owner.is_none() && when.raw("service_tier").is_some() {
                        RefusedFactScope::ModeRates(None)
                    } else {
                        scope(owner)
                    };
                    self.refuse(refused, role, detail);
                }
                None
            }
        }
    }

    fn read_fields(
        &mut self,
        schedule: &Schedule<'_>,
        fields: &Object<'raw>,
        condition: &RateCondition,
        owner: Option<&str>,
    ) {
        let role = schedule.role;
        self.add(
            schedule,
            condition,
            owner,
            format!("{role}.price"),
            fields.raw("price"),
            None,
        );
        if role.ends_with(".output") {
            let reasoning = Schedule {
                token_basis: (role == "text.output").then_some(ChargeBasis::PerMillionTokens {
                    class: TokenClass::Reasoning,
                }),
                established: true,
                ..*schedule
            };
            self.add(
                &reasoning,
                condition,
                owner,
                format!("{role}.reasoning"),
                fields.raw("reasoning"),
                None,
            );
        }
        if let Some(cache) = fields.object("cache") {
            self.cache(schedule, condition, owner, &cache);
        }
    }
}

impl<'raw> Prices<'_, 'raw> {
    fn add(
        &mut self,
        schedule: &Schedule<'_>,
        condition: &RateCondition,
        owner: Option<&str>,
        path: String,
        raw: Option<&'raw RawValue>,
        ttl: Option<String>,
    ) {
        let Some(raw) = raw else { return };
        let role = path.strip_suffix(".price").unwrap_or(&path);
        let basis = match (schedule.unit, role) {
            ("token", _) if schedule.established => schedule.token_basis.clone(),
            ("token", _) => {
                self.finding(FindingKey::UnattributableChargeUnit {
                    role: role.to_owned(),
                });
                // An unpriced base audio marker does not assert any billing unit.
                if owner.is_none() && *condition == RateCondition::Always {
                    if let Some(basis @ ChargeBasis::PerMillionTokens { .. }) =
                        &schedule.token_basis
                    {
                        self.rates.push(NormalizedRate {
                            basis: basis.clone(),
                            condition: RateCondition::Always,
                            value: RateValue::Unpriced {
                                reason: crate::UnpricedReason::UnknownChargeBasis,
                                inherited_from: None,
                            },
                        });
                    }
                }
                return;
            }
            ("character", "text.input") => Some(ChargeBasis::PerCharacter),
            ("second", "audio.input") => Some(ChargeBasis::PerSecond),
            ("minute", "audio.input") => Some(ChargeBasis::PerMinute),
            (
                "character" | "second" | "minute" | "image" | "megapixel" | "thousand_searches",
                _,
            ) => None,
            _ => {
                self.finding(FindingKey::UnknownCostUnit {
                    role: role.to_owned(),
                    value: schedule.unit.to_owned(),
                });
                return;
            }
        };
        let Some(basis) = basis else {
            self.finding(FindingKey::UnservedCost {
                role: role.to_owned(),
                unit: schedule.unit.to_owned(),
            });
            return;
        };
        if schedule.unit != "token" && (owner.is_some() || *condition != RateCondition::Always) {
            let detail = "conditional rate has no admitted key for this unit";
            if let RateCondition::Mode { name } = condition {
                for (name, _) in self.targets(name) {
                    self.refuse(scope(Some(&name)), role, detail);
                }
            } else {
                self.refuse(scope(owner), role, detail);
            }
            return;
        }
        if let Some(name) = owner {
            match Decimal::parse(raw) {
                Ok(value) => {
                    self.mode_prices.insert((name.to_owned(), basis), value);
                }
                Err(detail) => self.refuse(scope(owner), role, detail),
            }
            return;
        }
        let price = Price { path, ttl, raw };
        if *condition == RateCondition::Always {
            self.base.insert(basis, price);
        } else {
            self.overrides.insert((condition.clone(), basis), price);
        }
    }

    fn cache(
        &mut self,
        schedule: &Schedule<'_>,
        condition: &RateCondition,
        owner: Option<&str>,
        cache: &Object<'raw>,
    ) {
        let role = schedule.role;
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
                self.input_audio,
            ),
            _ => (None, None, true),
        };
        let read = Schedule {
            token_basis: read,
            established,
            ..*schedule
        };
        self.add(
            &read,
            condition,
            owner,
            format!("{role}.cache.read"),
            cache.raw("read"),
            None,
        );
        let write = Schedule {
            token_basis: write,
            established: true,
            ..*schedule
        };
        let write_price = cache
            .raw("write")
            .map(|raw| ("cache.write", raw))
            .or_else(|| {
                explicit
                    .as_ref()?
                    .raw("write")
                    .map(|raw| ("cache.explicit.write", raw))
            });
        if let Some((path, raw)) = write_price {
            // Preserve document order: the first listed TTL is the default.
            let (price, ttl) = Object::parse(raw)
                .ok()
                .and_then(|prices| prices.0.into_iter().next())
                .map_or((raw, None), |(ttl, price)| (price, Some(ttl)));
            self.add(
                &write,
                condition,
                owner,
                format!("{role}.{path}"),
                Some(price),
                ttl,
            );
        }
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
                self.finding(FindingKey::UnservedCost {
                    role: format!("{role}.{path}"),
                    unit: schedule.unit.to_owned(),
                });
            }
        }
    }

    fn targets(&self, tier: &str) -> Vec<(String, ModeSource)> {
        // A declared alias is the spelling clients already use. A synthetic
        // tier key alongside it would describe the same request twice.
        let mut targets: Vec<_> = self
            .modes
            .iter()
            .filter(|mode| mode.tier == tier)
            .map(|mode| {
                (
                    mode.name.clone(),
                    if mode.cost.is_some() {
                        ModeSource::OwnCost
                    } else {
                        ModeSource::BaseCost
                    },
                )
            })
            .collect();
        if targets.is_empty() {
            // A declared mode's name may be the label of a different service
            // tier. They can share a key only if the published prices agree.
            let source = if self.modes.iter().any(|mode| mode.name == tier) {
                ModeSource::ShadowedTier
            } else {
                ModeSource::Tier
            };
            targets.push((tier.to_owned(), source));
        }
        targets
    }

    fn read_multipliers(&mut self, cost: &Object<'raw>) {
        let Some(raw) = cost.raw("multipliers") else {
            return;
        };
        let Ok(rules) = serde_json::from_str::<Vec<&RawValue>>(raw.get()) else {
            self.refuse(scope(None), "multipliers", "multipliers is not an array");
            return;
        };
        let mut products = BTreeMap::new();
        let mut shadowed = BTreeMap::new();
        let mut derived = BTreeSet::new();
        for raw in rules {
            let Ok(rule) = Object::parse(raw) else {
                self.refuse(scope(None), "multipliers", "multiplier is not an object");
                continue;
            };
            let Some(condition) = self.read_condition(&rule, "multipliers", None) else {
                continue;
            };
            let RateCondition::Mode { name: tier } = condition else {
                self.refuse(scope(None), "multipliers", "a multiplier requires a service_tier condition, not an unconditional or context-tier price");
                continue;
            };
            for (name, source) in self.targets(&tier) {
                if source == ModeSource::BaseCost && !derived.insert(name.clone()) {
                    continue;
                }
                let prices = if source == ModeSource::ShadowedTier {
                    &mut shadowed
                } else {
                    &mut products
                };
                if let Err(detail) = self.multiply(rule.raw("factor"), &name, source, prices) {
                    self.refuse(scope(Some(&name)), "multipliers", detail);
                }
            }
        }
        self.mode_prices.extend(products);
        self.shadowed_prices.extend(shadowed);
    }

    fn multiply(
        &self,
        factor: Option<&RawValue>,
        name: &str,
        source: ModeSource,
        products: &mut BTreeMap<(String, ChargeBasis), Decimal>,
    ) -> Result<(), String> {
        let factor = factor.ok_or("missing multiplier factor")?;
        let mut matched = false;
        for (basis, base) in &self.base {
            let Some(factor) = factor_at(factor, &base.path, base.ttl.as_deref()) else {
                continue;
            };
            matched = true;
            if basis.unit_denominator().is_some() {
                return Err(format!(
                    "{} has no admitted key for a conditional unit price",
                    base.path
                ));
            }
            // Models.dev's v1 export derived a named mode from the first
            // matching service-tier multiplier and scaled only base prices.
            // Keep those named prices, but don't apply this shorthand to a new
            // standalone tier key: it cannot express context size as well as
            // service tier.
            if source != ModeSource::BaseCost
                && self.overrides.keys().any(|(condition, role)| {
                    role == basis && matches!(condition, RateCondition::MinContextTokens { .. })
                })
            {
                return Err(format!("multiplier on tiered price {}", base.path));
            }
            let factor = Decimal::parse(factor)?;
            // A mode's own cost takes precedence over a multiplier.
            if source == ModeSource::OwnCost {
                continue;
            }
            let key = (name.to_owned(), basis.clone());
            let amount = match products.get(&key) {
                Some(amount) => *amount,
                None => Decimal::parse(base.raw)?,
            };
            products.insert(key, amount.product(factor)?);
        }
        if !matched {
            return Err("multiplier factor names no priced base roles".to_owned());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModeSource {
    OwnCost,
    BaseCost,
    Tier,
    ShadowedTier,
}

fn scope(mode: Option<&str>) -> RefusedFactScope {
    mode.map_or(RefusedFactScope::BaseAndTierRates, |name| {
        RefusedFactScope::ModeRates(Some(name.to_owned()))
    })
}

impl Prices<'_, '_> {
    fn finish(mut self) -> Vec<NormalizedRate> {
        let overrides = std::mem::take(&mut self.overrides);
        let base = std::mem::take(&mut self.base);
        // Models.dev's v1 context bands carry both text input and output
        // prices. A partial v2 override leaves the other base price unchanged.
        let text_pair = [TokenClass::Input, TokenClass::Output]
            .map(|class| ChargeBasis::PerMillionTokens { class });
        let text_thresholds: BTreeSet<_> = overrides
            .keys()
            .filter_map(|(condition, basis)| {
                (matches!(condition, RateCondition::MinContextTokens { .. })
                    && text_pair.contains(basis))
                .then_some(condition.clone())
            })
            .collect();
        for condition in text_thresholds {
            for basis in &text_pair {
                if !overrides.contains_key(&(condition.clone(), basis.clone())) {
                    if let Some(price) = base.get(basis) {
                        self.publish(
                            basis.clone(),
                            condition.clone(),
                            &price.path,
                            Decimal::parse(price.raw),
                        );
                    }
                }
            }
        }
        for (basis, price) in base {
            self.publish(
                basis,
                RateCondition::Always,
                &price.path,
                Decimal::parse(price.raw),
            );
        }
        for ((condition, basis), price) in overrides {
            let amount = Decimal::parse(price.raw);
            if let RateCondition::Mode { name: tier } = condition {
                for (name, source) in self.targets(&tier) {
                    match &amount {
                        Ok(amount) => {
                            let prices = if source == ModeSource::ShadowedTier {
                                &mut self.shadowed_prices
                            } else {
                                &mut self.mode_prices
                            };
                            let slot = prices
                                .entry((name.clone(), basis.clone()))
                                .or_insert(*amount);
                            if slot != amount {
                                self.refuse(
                                    scope(Some(&name)),
                                    &price.path,
                                    format!("conflicting prices for mode {name}"),
                                );
                            }
                        }
                        Err(detail) => self.refuse(scope(Some(&name)), &price.path, detail),
                    }
                }
            } else {
                self.publish(basis, condition, &price.path, amount);
            }
        }
        for ((name, basis), amount) in std::mem::take(&mut self.shadowed_prices) {
            if self.mode_prices.get(&(name.clone(), basis.clone())) != Some(&amount) {
                self.refuse(
                    scope(Some(&name)),
                    &format!("{basis:?}"),
                    format!("conflicting prices for tier and declared mode both named {name}"),
                );
            }
        }
        for ((name, basis), amount) in std::mem::take(&mut self.mode_prices) {
            let role = format!("{basis:?}");
            self.publish(basis, RateCondition::Mode { name }, &role, Ok(amount));
        }
        self.rates
    }

    fn publish(
        &mut self,
        basis: ChargeBasis,
        condition: RateCondition,
        role: &str,
        amount: Result<Decimal, String>,
    ) {
        let parsed = amount.and_then(|amount| {
            rate_value(self.model, role, &amount.literal()).map_err(|e| e.to_string())
        });
        match parsed {
            Ok(mut value) => {
                if let RateValue::Priced { per, .. } = &mut value {
                    *per = basis.unit_denominator();
                }
                self.rates.push(NormalizedRate {
                    basis,
                    condition,
                    value,
                });
            }
            Err(detail) => {
                let owner = if let RateCondition::Mode { name } = &condition {
                    Some(name.as_str())
                } else {
                    None
                };
                self.refuse(scope(owner), role, detail);
            }
        }
    }
}
