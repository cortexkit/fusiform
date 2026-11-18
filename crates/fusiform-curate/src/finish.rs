use std::collections::{BTreeMap, BTreeSet};

use crate::{
    apply_effort_facts,
    billing_planes::{self, Plane, PlaneKind, RuleState},
    cache_lifetimes, fact_for_the_wire, is_effort_fact, is_mode_rate,
    name_the_provenance_of_a_pre_0_11_row, narrow_to_requested,
    speed_twins::{SpeedTwin, SpeedTwins},
    CurationSet, Facts, RawFacts, RecordedEffortGrade, RecordedWorkloadEffort, ServedState, RATE,
};
use fusiform_protocol::{BillingRuleWire, CatalogGetResponse, PlaneWire};

#[derive(Clone, Default)]
pub struct Quality {
    pub grades: Vec<RecordedGrade>,
    pub efforts: Vec<RecordedEffortGrade>,
    pub workloads: Vec<RecordedWorkloadEffort>,
    /// The currently decommissioned model keys and the marks' recording times.
    pub marks: BTreeMap<String, i64>,
}

#[derive(Clone)]
pub struct RecordedGrade {
    pub model: String,
    pub kind: Option<String>,
    pub grade: Grade,
    pub graded_effort: Option<String>,
    pub recorded_at_ms: i64,
}

#[derive(Clone)]
pub enum Grade {
    Chat {
        elo: serde_json::Number,
        eq: serde_json::Number,
    },
    NonChat {
        grade: serde_json::Number,
    },
}

/// Add the facts shared by the plain and billing-plane views. A dated read
/// supplies only the lifetime rules in force at the requested instant.
pub fn derive_row_facts(
    response: &mut ServedState,
    current: Option<&CurationSet<'_>>,
    lifetimes: &cache_lifetimes::Rules,
    quality: &Quality,
    requested_prefixes: Option<&Vec<String>>,
) {
    for (identity, facts) in &mut response.models {
        if let Some((provider, model)) = identity.split_once('/') {
            crate::cache_lifetimes::apply(lifetimes, provider, model, facts);
        }
    }
    if let Some(current) = current {
        for (identity, facts) in &mut response.models {
            if let Some((provider, model)) = identity.split_once('/') {
                // Attachment limits are rules of the endpoint that receives the
                // request, not of the model. An alias routes through another
                // backend (Code Assist for the Antigravity rows), whose limits no
                // source states, so it gets none rather than its target's.
                let alias = response
                    .aliased
                    .iter()
                    .find(|alias| alias.model == *identity);
                if alias.is_none() {
                    crate::attachment_limits::apply(current.attachment_limits, provider, facts);
                    crate::temperature_ranges::apply(current.temperature_ranges, provider, facts);
                }
                // A curated model can state a more specific conversion than
                // the provider default; keep that sourced claim intact.
                if facts.contains_key("rate.input_audio")
                    && !facts.contains_key("limit.audio_tokens_per_second")
                {
                    // An alias borrows the target's conversion as well as its audio rate.
                    let (audio_provider, audio_model) = alias
                        .map(|alias| {
                            (
                                alias.target_provider_id.as_str(),
                                alias.target_model_id.as_str(),
                            )
                        })
                        .unwrap_or((provider, model));
                    if let Some(audio) = current
                        .input_audio
                        .get(audio_provider)
                        .and_then(|row| row.audio_tokens_per_second.as_ref())
                    {
                        facts.insert(
                            "limit.audio_tokens_per_second".to_string(),
                            serde_json::json!(audio.value_for(audio_model)),
                        );
                    }
                }
            }
        }
        apply_quality_scores(quality, response, requested_prefixes);
        apply_model_lineage(current.model_lineage, response);
    }
    apply_effort_facts(quality, response, requested_prefixes);
}

/// Get the exact upstream card a declared quota proxy may borrow for this row.
pub fn plane_rate_target<'a>(
    speed_twins: &'a SpeedTwins,
    identity: &str,
    facts: &'a Facts,
    provider: &str,
) -> Option<&'a str> {
    let api_card = identity.split_once('/').and_then(|(p, m)| {
        speed_twins
            .values()
            .find(|row| row.provider_id == p && row.twin_model_id == m)
            .map(|row| row.api_card.as_str())
    });
    api_card
        .or_else(|| facts.get("model.canonical_id").and_then(|v| v.as_str()))
        .and_then(|id| id.split_once('/'))
        .filter(|(p, id)| *p == provider && !id.is_empty())
        .map(|(_, id)| id)
}

/// Project a billing plane from raw upstream cards already read at the same
/// instant. The key of each card is its model id under the borrowing provider.
pub fn project_plane(
    response: &mut ServedState,
    wire: PlaneWire,
    declared: Option<&Plane>,
    speed_twins: &SpeedTwins,
    cards: &BTreeMap<String, RawFacts>,
) {
    if wire.auth_method == "gateway" {
        // Endpoint corrections describe the upstream API address, which the
        // gateway view omits in favor of the credential's own endpoint.
        response.wire_corrections.clear();
    }
    response.plane = Some(wire);
    for (identity, facts) in &mut response.models {
        if let Some(declared) = declared {
            if let Some(provider) = declared.borrows_rates_from() {
                let origin = plane_rate_target(speed_twins, identity, facts, provider)
                    .and_then(|id| cards.get_key_value(id))
                    .map(|(id, facts)| (id.as_str(), facts));
                borrow_plane_rates(facts, provider, origin);
            }
            response
                .billing_rules
                .extend(apply_billing_plane(identity, facts, declared));
            if !response.wire_corrections.iter().any(|correction| {
                correction.withheld
                    && format!("{}/{}", correction.provider, correction.model) == *identity
            }) {
                if let Some(family) = &declared.wire_family {
                    facts.insert("wire.family".to_string(), serde_json::json!(family));
                }
            }
        }
    }
}

/// Narrow only after the plane and speed modes have used the supporting facts.
pub fn finish_view(
    response: &mut ServedState,
    speed_twins: Option<&SpeedTwins>,
    requested_prefixes: Option<&Vec<String>>,
) {
    for facts in response.models.values_mut() {
        narrow_to_requested(facts, requested_prefixes);
    }
    if let Some(rows) = speed_twins {
        retain_speed_twins(rows, response);
    }
    response.billing_rules.retain(|rule| {
        response
            .models
            .get(&rule.model)
            .is_some_and(|facts| facts.contains_key(&rule.fact_key))
    });
    response.canonical_links.retain(|link| {
        response
            .models
            .get(&link.model)
            .is_some_and(|facts| facts.contains_key("model.canonical_id"))
    });
    retain_served_overrides(response);
    retain_quality_disclosure(response);
    retain_requested_wire_corrections(response, requested_prefixes);
}

pub fn annotate_speed_twin(row: &SpeedTwin, response: &mut ServedState) {
    let twin = format!("{}/{}", row.provider_id, row.twin_model_id);
    if let Some(facts) = response.models.get_mut(&twin) {
        facts.insert(
            "model.mode_of".into(),
            serde_json::json!({
                "model_id": row.base_model_id, "mode": row.mode,
            }),
        );
    }
}

/// Join a twin's rates only after billing projection, so a base-mode call and
/// a call to the twin id use the same rate.
pub fn apply_speed_twin(row: &SpeedTwin, base: &str, card: Facts, response: &mut ServedState) {
    let Some(facts) = response.models.get_mut(base) else {
        return;
    };
    for (key, value) in card {
        let Some(dimension) = key.strip_prefix("rate.") else {
            continue;
        };
        if is_mode_rate(&key) {
            continue;
        }
        let (dimension, suffix) = dimension
            .split_once('.')
            .map_or((dimension, String::new()), |(dimension, suffix)| {
                (dimension, format!(".{suffix}"))
            });
        facts.insert(format!("rate.{dimension}.mode.{}{suffix}", row.mode), value);
    }
    for (key, value) in [
        ("route.mode_wire_ids", &row.twin_model_id),
        ("route.mode_labels", &row.label),
    ] {
        facts
            .entry(key.into())
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .expect("speed route fact is an object")
            .insert(row.mode.clone(), serde_json::json!(value));
    }
}

fn retain_speed_twins(rows: &SpeedTwins, response: &mut CatalogGetResponse) {
    response.speed_twins = rows
        .values()
        .filter_map(|row| {
            let model = format!("{}/{}", row.provider_id, row.base_model_id);
            let twin = format!("{}/{}", row.provider_id, row.twin_model_id);
            let base_served = response.models.get(&model).is_some_and(|facts| {
                facts.contains_key("route.mode_wire_ids")
                    || facts.contains_key("route.mode_labels")
                    || facts.keys().any(|key| {
                        key.starts_with("rate.") && key.contains(&format!(".mode.{}", row.mode))
                    })
            });
            let twin_served = response
                .models
                .get(&twin)
                .is_some_and(|facts| facts.contains_key("model.mode_of"));
            (base_served || twin_served).then(|| fusiform_protocol::SpeedTwinWire {
                model,
                twin,
                mode: if row.mode == "fast" {
                    fusiform_protocol::ServiceTier::Fast
                } else {
                    fusiform_protocol::ServiceTier::Ultrafast
                },
                label: row.label.clone(),
                source_ref: row.source_ref.clone(),
                review_by_ms: row.review_by_ms,
            })
        })
        .collect();
}

fn retain_requested_wire_corrections(
    response: &mut CatalogGetResponse,
    prefixes: Option<&Vec<String>>,
) {
    // A wire.family read also fetches wire.api to discover whether the
    // endpoint is withheld. Disclose endpoint corrections only if the caller
    // requested wire.api itself.
    if prefixes.is_some_and(|prefixes| !prefixes.iter().any(|p| "wire.api".starts_with(p))) {
        response.wire_corrections.clear();
    }
}

fn apply_model_lineage(
    classifications: &crate::model_lineage::ModelLineage,
    response: &mut CatalogGetResponse,
) {
    for (identity, facts) in &mut response.models {
        let lineage = classifications.lookup(
            identity,
            facts
                .get("model.family")
                .and_then(serde_json::Value::as_str),
            facts
                .get("model.canonical_id")
                .and_then(serde_json::Value::as_str),
            response
                .aliased
                .iter()
                .any(|alias| &alias.model == identity),
        );
        if let Some(lineage) = lineage {
            facts.insert("model.lineage".into(), serde_json::json!(lineage));
        }
    }
}

fn apply_quality_scores(
    quality: &Quality,
    response: &mut CatalogGetResponse,
    requested_prefixes: Option<&Vec<String>>,
) {
    let requested = |key: &str| {
        requested_prefixes.is_none_or(|prefixes| prefixes.iter().any(|p| key.starts_with(p)))
    };
    let scores = quality
        .grades
        .iter()
        .fold(BTreeMap::<&str, Vec<_>>::new(), |mut scores, row| {
            if !row.model.starts_with("local:") {
                scores.entry(&row.model).or_default().push(row);
            }
            scores
        });
    let mut newest = None;
    for (identity, facts) in &mut response.models {
        let canonical = facts
            .get("model.canonical_id")
            .and_then(serde_json::Value::as_str);
        // An alias without a served canonical identity has no independent grade.
        if canonical.is_none()
            && response
                .aliased
                .iter()
                .any(|alias| &alias.model == identity)
        {
            continue;
        }
        let key = canonical.unwrap_or(identity).to_owned();
        if let Some(&marked_at_ms) = quality
            .marks
            .get(&key)
            .filter(|_| requested("quality.decommissioned"))
        {
            facts.insert(
                "quality.decommissioned".into(),
                serde_json::Value::Bool(true),
            );
            newest = Some(newest.map_or(marked_at_ms, |n: i64| n.max(marked_at_ms)));
        }
        let Some(rows) = scores.get(key.as_str()) else {
            continue;
        };
        for row in rows {
            let grade = &row.grade;
            let non_chat = facts
                .get("model.type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind != "chat");
            let values = match grade {
                Grade::Chat { elo, eq } if !non_chat => {
                    vec![
                        ("quality.elo".to_string(), elo),
                        ("quality.eq".to_string(), eq),
                    ]
                }
                Grade::NonChat { grade } => {
                    let Some(kind) = &row.kind else { continue };
                    vec![(format!("quality.grade.{kind}"), grade)]
                }
                _ => continue,
            };
            for (key, value) in values {
                if !requested(&key) {
                    continue;
                }
                facts.insert(key, serde_json::Value::Number(value.clone()));
                newest =
                    Some(newest.map_or(row.recorded_at_ms, |n: i64| n.max(row.recorded_at_ms)));
            }
        }
    }
    response.quality_scores = newest.map(|graded_at_ms| fusiform_protocol::QualityScoresWire {
        graded_by: "operator".to_string(),
        graded_at_ms,
    });
    retain_quality_disclosure(response);
}

pub fn counts_for_quality_disclosure(facts: &Facts) -> bool {
    facts.contains_key("quality.elo")
        || facts.contains_key("quality.eq")
        || facts.keys().any(|key| key.starts_with("quality.grade."))
        || facts.contains_key("quality.decommissioned")
        || facts.keys().any(|key| is_effort_fact(key))
}

pub fn retain_quality_disclosure(response: &mut CatalogGetResponse) {
    if !response.models.values().any(counts_for_quality_disclosure) {
        response.quality_scores = None;
    }
}

/// Keep only override disclosures whose value `models` still serves under the
/// disclosed identity.
///
/// Steps after rendering can remove or replace an overridden fact: the plane
/// view's final fact filter, or a curated row standing in for a withdrawn
/// upstream row. A disclosure left behind would claim a value the response
/// does not carry.
fn retain_served_overrides(response: &mut CatalogGetResponse) {
    let models = &response.models;
    response.fact_overrides.retain(|row| {
        models
            .get(&row.model)
            .and_then(|facts| facts.get(&row.fact_key))
            == Some(&row.value)
    });
}

fn borrow_plane_rates(facts: &mut Facts, provider: &str, origin: Option<(&str, &RawFacts)>) {
    if let Some((model_id, origin)) = origin {
        // InheritedFrom requires a family string. Use an empty one when the
        // family is absent or null: the canonical id identifies this API row.
        let family = origin
            .get("model.family")
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let marker = serde_json::to_value(fusiform_protocol::money::InheritedFrom {
            provider_id: provider.to_string(),
            model_id: Some(model_id.to_string()),
            mapping: None,
            family,
            basis: "quota_proxy".to_string(),
        })
        .expect("a struct of strings serialises");
        facts.retain(|key, _| !key.starts_with(RATE));
        for (key, raw) in origin {
            if !key.starts_with(RATE) {
                continue;
            }
            let mut value = fact_for_the_wire(raw);
            name_the_provenance_of_a_pre_0_11_row(&mut value);
            if let Some(obj) = value.as_object_mut() {
                obj.insert("inherited_from".into(), marker.clone());
            }
            facts.insert(key.clone(), value);
        }
    } else {
        let unpriced = serde_json::to_value(fusiform_protocol::RateValue::Unpriced {
            reason: fusiform_protocol::UnpricedReason::NoApiCard,
            inherited_from: None,
        })
        .expect("a rate value serialises");
        for (key, value) in facts.iter_mut() {
            if key.starts_with(RATE) {
                *value = unpriced.clone();
            }
        }
        // A plan can omit rate facts entirely. Still give callers an explicit
        // reason for the unavailable base quota weights rather than silence.
        for class in ["input", "output", "cache_read", "cache_write", "reasoning"] {
            facts.insert(format!("rate.{class}"), unpriced.clone());
        }
    }
}

fn apply_billing_plane(
    identity: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    plane: &Plane,
) -> Vec<BillingRuleWire> {
    if plane.auth_method == "gateway" {
        // A gateway's address belongs to the credential. Keeping the upstream
        // endpoint here could send the gateway credential to the wrong host.
        facts.remove("wire.api");
    }
    if plane.provider_id == "openai" && plane.auth_method == "chatgpt" {
        // ChatGPT bills cache writes as input and accepts no cache lifetime.
        facts.retain(|key, _| {
            key != "capability.cache_lifetimes" && !(key.starts_with(RATE) && key.contains(".ttl."))
        });
    }
    if plane.kind == PlaneKind::QuotaProxy {
        // A subscription may not offer an API service mode. Keeping its price
        // would claim a charge for a mode whose availability is not established.
        facts.retain(|key, _| !is_mode_rate(key));
    }
    if plane.kind == PlaneKind::QuotaProxy && plane.borrows_rates_from().is_none() {
        let marker = serde_json::to_value(fusiform_protocol::money::InheritedFrom {
            provider_id: plane.provider_id.clone(),
            family: facts
                .get("model.family")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            basis: "quota_proxy".to_string(),
            model_id: None,
            mapping: None,
        })
        .expect("a struct of strings serialises");
        for (key, value) in facts.iter_mut() {
            if !key.starts_with(RATE) {
                continue;
            }
            let Some(obj) = value.as_object_mut() else {
                continue;
            };
            if obj.contains_key("inherited_from") {
                // Already borrowed (inherited or aliased): keep the original
                // marker, which names the row the price really comes from.
                continue;
            }
            obj.insert("inherited_from".to_string(), marker.clone());
        }
    }

    // Targets and tier suffixes come from the pre-rule view. Resolving against
    // a progressively modified map would make rule order invent billing chains.
    let before_rules = facts.clone();
    let model_id = identity
        .split_once('/')
        .map_or(identity, |(_, model)| model);
    let mut applied = Vec::new();
    for rule in plane.rules.iter().filter(|rule| rule.applies_to(model_id)) {
        let class_key = format!("rate.{}", billing_planes::class_name(rule.class));
        let target_key = rule
            .as_class
            .map(|class| format!("rate.{}", billing_planes::class_name(class)));
        let mut suffixes = BTreeSet::from([String::new()]);
        for key in before_rules.keys() {
            for base in std::iter::once(&class_key).chain(
                target_key
                    .as_ref()
                    .filter(|_| rule.state == RuleState::BilledAs),
            ) {
                if let Some(suffix) = key.strip_prefix(base) {
                    if suffix
                        .strip_prefix(".above_context.")
                        .is_some_and(|n| n.parse::<u64>().is_ok())
                    {
                        suffixes.insert(suffix.to_string());
                    }
                }
            }
        }
        for suffix in suffixes {
            let fact_key = format!("{class_key}{suffix}");
            if plane.kind == PlaneKind::Upstream && before_rules.contains_key(&fact_key) {
                // A published price, zero or tombstone is authoritative for this
                // key, not a row-level veto of rules for other missing tiers.
                continue;
            }
            let mut state = rule.state;
            let value = match rule.state {
                RuleState::BilledAs => {
                    let key = format!(
                        "{}{suffix}",
                        target_key
                            .as_ref()
                            .expect("loaded billed_as rules have a target")
                    );
                    if before_rules.contains_key(&key) {
                        fusiform_protocol::RateValue::BilledAs { key }
                    } else {
                        state = RuleState::NotEstablished;
                        fusiform_protocol::RateValue::Unpriced {
                            reason: fusiform_protocol::UnpricedReason::NotEstablished,
                            inherited_from: None,
                        }
                    }
                }
                RuleState::StatedZero => fusiform_protocol::RateValue::StatedZero {
                    inherited_from: None,
                },
                RuleState::NotEstablished => fusiform_protocol::RateValue::Unpriced {
                    reason: fusiform_protocol::UnpricedReason::NotEstablished,
                    inherited_from: None,
                },
            };
            facts.insert(
                fact_key.clone(),
                serde_json::to_value(value).expect("a rate value serialises"),
            );
            applied.push(BillingRuleWire {
                model: identity.to_string(),
                fact_key,
                state: state.as_str().to_string(),
                as_class: rule
                    .as_class
                    .map(|class| billing_planes::class_name(class).to_string()),
                source_ref: rule.source_ref.clone(),
                established_by: rule.established_by.clone(),
                established_at_ms: rule.established_at_ms,
                review_by_ms: rule.review_by_ms,
            });
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::mark_rates_borrowed_through_alias;
    use fusiform_core::TokenClass;
    use serde_json::{json, Value};

    fn planes() -> billing_planes::Planes {
        billing_planes::load_reporting_rejects(include_str!(
            "../../fusiform-module/data/billing-planes.json"
        ))
        .0
    }
    fn priced(units: i64) -> Value {
        json!({"state":"priced", "units":units, "exponent":9, "currency":"USD", "unit_provenance":"source"})
    }

    #[test]
    fn every_rule_state_uses_class_tiers_and_exact_model_scopes() {
        // Rule states and exact-model scopes the loader accepts but no shipped
        // plane uses yet. Test them on a cloned plane rather than adding an
        // invented plane to the compiled-in file, which would then be served.
        let mut plane = planes()[&("openai".into(), "chatgpt".into())].clone();
        plane.rules = vec![
            billing_planes::BillingRule {
                class: TokenClass::Output,
                state: RuleState::StatedZero,
                as_class: None,
                model_ids: Some(vec!["scoped/model".into()]),
                ..plane.rules[0].clone()
            },
            billing_planes::BillingRule {
                class: TokenClass::Reasoning,
                state: RuleState::NotEstablished,
                as_class: None,
                model_ids: None,
                ..plane.rules[0].clone()
            },
        ];
        for (model, expected) in [("scoped/model", "stated_zero"), ("other", "priced")] {
            let mut facts = BTreeMap::from([
                ("rate.output".into(), priced(100)),
                ("rate.output.above_context.42".into(), priced(200)),
                ("rate.reasoning.above_context.43".into(), priced(300)),
            ]);
            let rules = apply_billing_plane(&format!("openai/{model}"), &mut facts, &plane);
            for key in ["rate.output", "rate.output.above_context.42"] {
                assert_eq!(facts[key]["state"], expected);
            }
            for key in ["rate.reasoning", "rate.reasoning.above_context.43"] {
                assert_eq!(
                    facts[key],
                    json!({"state":"unpriced", "reason":"not_established"})
                );
            }
            assert_eq!(rules.len(), if model == "scoped/model" { 4 } else { 2 });
        }
    }

    #[test]
    fn billed_as_targets_are_resolved_before_any_rule_outputs() {
        // A billed-as rule points at a price the upstream published. The loader
        // already refuses a rule pointing at another rule's output; this checks
        // that evaluation also reads targets from published prices only.
        let mut plane = planes()[&("openai".into(), "chatgpt".into())].clone();
        let write = plane.rules[0].clone();
        plane.rules.insert(
            0,
            billing_planes::BillingRule {
                class: TokenClass::Input,
                state: RuleState::StatedZero,
                as_class: None,
                ..write
            },
        );
        let mut facts = BTreeMap::from([("rate.cache_write.above_context.42".into(), priced(100))]);
        let rules = apply_billing_plane("openai/test-model", &mut facts, &plane);
        assert_eq!(facts["rate.input"], json!({"state":"stated_zero"}));
        assert_eq!(
            facts["rate.cache_write"],
            json!({"state":"unpriced", "reason":"not_established"})
        );
        assert_eq!(
            facts["rate.cache_write.above_context.42"],
            json!({"state":"unpriced", "reason":"not_established"})
        );
        assert_eq!(rules[1].state, "not_established");
    }

    #[test]
    fn quota_proxies_do_not_erase_alias_markers() {
        let plane = &planes()[&("openai".into(), "chatgpt".into())];
        let alias = crate::aliases::Alias {
            target_provider_id: "other-provider".into(),
            target_model_id: "target-model".into(),
            model_name: None,
            wire_family: None,
            basis: "same backend model".into(),
            source_ref: "routing code".into(),
            established_at_ms: 1_000,
            review_by_ms: i64::MAX,
            reasoning_wire_ids: BTreeMap::new(),
            reasoning_default: None,
            limit_output: None,
        };
        let mut facts = BTreeMap::from([
            ("rate.input".into(), priced(100)),
            ("model.family".into(), json!("real-family")),
        ]);
        mark_rates_borrowed_through_alias(&mut facts, &alias);
        let before = facts["rate.input"].clone();
        apply_billing_plane("openai/alias-model", &mut facts, plane);
        assert_eq!(facts["rate.input"], before);
        assert_eq!(
            facts["rate.cache_write"],
            json!({"state":"billed_as", "key":"rate.input"})
        );
    }
}
