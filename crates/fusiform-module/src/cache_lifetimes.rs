//! Current-only first-party prompt-cache lifetimes and guarded write prices.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

const DOCUMENT: &str = include_str!("../data/cache-lifetimes.json");
const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;

#[derive(Clone, Debug, Deserialize)]
pub struct Multiplier {
    pub numerator: u64,
    pub denominator: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Rule {
    pub provider_id: String,
    pub model_ids: Vec<String>,
    pub lifetimes: Vec<String>,
    #[serde(default)]
    pub multipliers: BTreeMap<String, Multiplier>,
    pub source_ref: String,
    pub quote: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

pub type Rules = BTreeMap<(String, String), Rule>;

pub fn load_reporting_rejects(doc: &str) -> (Rules, Vec<String>) {
    #[derive(Deserialize)]
    struct Document {
        schema: String,
        rules: Vec<Value>,
    }
    let mut rules = Rules::new();
    let mut rejects = Vec::new();
    let doc: Document = match serde_json::from_str(doc) {
        Ok(doc) => doc,
        Err(error) => return (rules, vec![error.to_string()]),
    };
    if doc.schema != "cache-lifetimes/v1" {
        return (rules, vec!["unknown cache-lifetimes schema".into()]);
    }
    for (index, raw) in doc.rules.into_iter().enumerate() {
        let rule: Rule = match serde_json::from_value(raw) {
            Ok(rule) => rule,
            Err(error) => {
                rejects.push(format!("rule {index}: {error}"));
                continue;
            }
        };
        let valid_lifetimes = match rule.provider_id.as_str() {
            "openai" => matches!(
                rule.lifetimes
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .as_slice(),
                ["30m", "12h"] | ["in_memory", "24h"] | ["24h"]
            ),
            "anthropic" => rule.lifetimes == ["5m", "1h"],
            _ => false,
        };
        let priced = rule
            .lifetimes
            .first()
            .is_some_and(|ttl| ttl == "30m" || ttl == "5m");
        if !valid_lifetimes
            || rule.model_ids.is_empty()
            || rule.model_ids.iter().any(|id| id.trim().is_empty())
            || rule.model_ids.iter().collect::<BTreeSet<_>>().len() != rule.model_ids.len()
            || rule.source_ref.trim().is_empty()
            || rule.quote.trim().is_empty()
            || rule.established_at_ms <= 0
            || rule.review_by_ms.checked_sub(rule.established_at_ms) != Some(REVIEW_INTERVAL_MS)
            || (priced
                && (rule.multipliers.len() != rule.lifetimes.len()
                    || rule
                        .lifetimes
                        .iter()
                        .any(|ttl| !rule.multipliers.contains_key(ttl))))
            || (!priced && !rule.multipliers.is_empty())
            || rule
                .multipliers
                .values()
                .any(|m| m.numerator == 0 || m.denominator == 0)
        {
            rejects.push(format!(
                "rule {index}: invalid scope, lifetimes, multipliers or provenance"
            ));
            continue;
        }
        for model in &rule.model_ids {
            let key = (rule.provider_id.clone(), model.clone());
            if let std::collections::btree_map::Entry::Vacant(entry) = rules.entry(key) {
                entry.insert(rule.clone());
            } else {
                rejects.push(format!(
                    "rule {index}: duplicate {}/{model}",
                    rule.provider_id
                ));
            }
        }
    }
    (rules, rejects)
}

pub fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

pub fn overdue_at(rules: &Rules, now_ms: i64) -> Vec<String> {
    rules
        .iter()
        .filter(|(_, rule)| rule.review_by_ms < now_ms)
        .map(|((provider, model), rule)| {
            format!(
                "{provider}/{model}: re-verify {} and re-date the 60-day review",
                rule.source_ref
            )
        })
        .collect()
}

pub fn missing_seed_rows(rules: &Rules, seed: &Value) -> Vec<String> {
    rules
        .keys()
        .filter(|(provider, model)| seed[provider]["models"].get(model).is_none())
        .map(|(provider, model)| format!("{provider}/{model}: missing from embedded seed"))
        .collect()
}

// Refuse rounding: the advertised multiplier must be representable exactly in
// nano-units, and a malformed or borrowed price cannot establish an own rate.
fn nano_amount(value: &Value) -> Option<i64> {
    if value["state"] != "priced" || value.get("inherited_from").is_some() {
        return None;
    }
    let amount = value["units"].as_i64()?;
    let exponent = value["exponent"].as_u64()?;
    if amount < 0 || exponent > 9 {
        return None;
    }
    amount.checked_mul(10_i64.checked_pow((9 - exponent) as u32)?)
}

fn multiplied(input: i64, multiplier: &Multiplier) -> Option<i64> {
    let product = i128::from(input).checked_mul(i128::from(multiplier.numerator))?;
    let divisor = i128::from(multiplier.denominator);
    if product % divisor != 0 {
        return None;
    }
    i64::try_from(product / divisor).ok()
}

fn input_suffix(key: &str) -> Option<&str> {
    let suffix = key.strip_prefix("rate.input")?;
    (suffix.is_empty()
        || suffix
            .strip_prefix(".above_context.")
            .is_some_and(|n| n.parse::<u64>().is_ok()))
    .then_some(suffix)
}

/// Return whether the row's cache-write prices disagree with the price
/// implied by the shortest TTL.
/// Lifetimes survive a mismatch, but no derived price from that row does.
pub fn apply(provider: &str, model: &str, facts: &mut BTreeMap<String, Value>) -> bool {
    let Some(rule) = rules().get(&(provider.to_string(), model.to_string())) else {
        return false;
    };
    facts.insert(
        "capability.cache_lifetimes".into(),
        serde_json::json!(rule.lifetimes),
    );
    if rule.multipliers.is_empty() {
        return false;
    }
    let shortest = &rule.multipliers[&rule.lifetimes[0]];
    let mut suffixes: BTreeSet<String> = facts
        .keys()
        .filter_map(|key| input_suffix(key).map(str::to_string))
        .collect();
    suffixes.insert(String::new());
    for key in facts.keys() {
        if let Some(suffix) = key.strip_prefix("rate.cache_write") {
            if suffix
                .strip_prefix(".above_context.")
                .is_some_and(|n| n.parse::<u64>().is_ok())
            {
                suffixes.insert(suffix.to_string());
            }
        }
    }
    let mut derived = Vec::new();
    for suffix in suffixes {
        let input = facts
            .get(&format!("rate.input{suffix}"))
            .and_then(nano_amount);
        let write = facts.get(&format!("rate.cache_write{suffix}"));
        let expected = input.and_then(|amount| multiplied(amount, shortest));
        if expected.is_none() || expected != write.and_then(nano_amount) {
            return true;
        }
        for ttl in &rule.lifetimes {
            let Some(amount) = multiplied(input.unwrap(), &rule.multipliers[ttl]) else {
                return true;
            };
            let mut value = write.unwrap().clone();
            value["units"] = amount.into();
            value["exponent"] = 9.into();
            derived.push((format!("rate.cache_write{suffix}.ttl.{ttl}"), value));
        }
    }
    facts.extend(derived);
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_lifetime_shipped_rules_are_valid_reviewed_and_in_seed() {
        let (rules, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rules.len(), 33);
        let seed = serde_json::from_str(include_str!("../data/models-dev-seed.json")).unwrap();
        assert!(missing_seed_rows(&rules, &seed).is_empty());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert!(overdue_at(&rules, now).is_empty());
        let anthropic: BTreeSet<_> = rules
            .keys()
            .filter(|(provider, _)| provider == "anthropic")
            .map(|(_, model)| model.as_str())
            .collect();
        let expected: BTreeSet<_> = seed["anthropic"]["models"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(anthropic, expected);
        assert!(!rules
            .keys()
            .any(|(_, model)| model.starts_with("gpt-daybreak-")));
        for rule in rules.values().filter(|rule| !rule.multipliers.is_empty()) {
            let short = &rule.multipliers[&rule.lifetimes[0]];
            assert_eq!((short.numerator, short.denominator), (5, 4));
            let long = &rule.multipliers[&rule.lifetimes[1]];
            assert_eq!(
                (long.numerator, long.denominator),
                if rule.provider_id == "openai" {
                    (5, 2)
                } else {
                    (2, 1)
                }
            );
        }
    }

    #[test]
    fn cache_lifetime_fences_detect_planted_missing_overdue_and_duplicate_rows() {
        let (identity, rule) = rules().first_key_value().unwrap();
        let deadline = rule.review_by_ms;
        let rules = BTreeMap::from([(identity.clone(), rule.clone())]);
        assert!(overdue_at(&rules, deadline).is_empty());
        assert_eq!(overdue_at(&rules, deadline + 1).len(), rules.len());
        let missing = missing_seed_rows(&rules, &json!({}));
        assert_eq!(missing.len(), rules.len());
        let mut doc: Value = serde_json::from_str(DOCUMENT).unwrap();
        let row = doc["rules"][0].clone();
        doc["rules"].as_array_mut().unwrap().push(row);
        assert_eq!(load_reporting_rejects(&doc.to_string()).1.len(), 9);
        doc["rules"][0]["review_by_ms"] = json!(deadline + 1);
        assert!(!load_reporting_rejects(&doc.to_string()).1.is_empty());
    }

    #[test]
    fn cache_lifetime_math_is_exact_and_tier_mismatch_withholds_the_whole_card() {
        assert_eq!(
            multiplied(
                1,
                &Multiplier {
                    numerator: 5,
                    denominator: 4
                }
            ),
            None
        );
        assert_eq!(
            multiplied(
                i64::MAX,
                &Multiplier {
                    numerator: 5,
                    denominator: 2
                }
            ),
            None
        );
        let rate = |units| json!({"state":"priced", "units":units, "exponent":9});
        let mut facts = BTreeMap::from([
            ("rate.input".into(), rate(2_000_000_000_i64)),
            ("rate.cache_write".into(), rate(2_500_000_000_i64)),
            (
                "rate.input.above_context.272000".into(),
                rate(4_000_000_000_i64),
            ),
            (
                "rate.cache_write.above_context.272000".into(),
                rate(4_999_999_999_i64),
            ),
        ]);
        assert!(apply("openai", "gpt-6.1-sol", &mut facts));
        assert!(facts.contains_key("capability.cache_lifetimes"));
        assert!(!facts.keys().any(|key| key.contains(".ttl.")));
        let mut unlisted = facts.clone();
        unlisted.remove("capability.cache_lifetimes");
        assert!(!apply("gateway", "gpt-6.1-sol", &mut unlisted));
        assert!(!unlisted.contains_key("capability.cache_lifetimes"));
    }
}
