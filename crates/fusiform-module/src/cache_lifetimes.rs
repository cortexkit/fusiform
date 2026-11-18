//! The embedded cache lifetimes document and its local adapter.

pub use fusiform_curate::cache_lifetimes::*;
#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet};

use std::sync::OnceLock;
const DOCUMENT: &str = include_str!("../data/cache-lifetimes.json");
pub fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

/// The same expanded per-model rules used for serving, ready for boot recording.
pub fn stored_rules(rules: &Rules) -> Vec<fusiform_store::NewCacheLifetime> {
    rules
        .iter()
        .map(
            |((provider, model), rule)| fusiform_store::NewCacheLifetime {
                provider_id: provider.clone(),
                model_id: model.clone(),
                rule: serde_json::to_value(rule).expect("cache lifetime rule is JSON"),
            },
        )
        .collect()
}

/// Load rules in force at the read's instant; tombstones deliberately add none.
pub fn at(
    store: &fusiform_store::CatalogStore,
    at_ms: i64,
    provider_id: Option<&str>,
) -> Result<Rules, fusiform_store::CatalogError> {
    from_eras(store.cache_lifetimes_at(at_ms, provider_id)?)
}

pub(crate) fn from_eras(
    eras: impl IntoIterator<Item = fusiform_store::CacheLifetimeEra>,
) -> Result<Rules, fusiform_store::CatalogError> {
    eras.into_iter()
        .filter_map(|era| {
            era.rule.map(|value| {
                let rule = serde_json::from_value(value.clone()).map_err(|_| {
                    fusiform_store::CatalogError::Decode {
                        column: "cache_lifetime_era.rule_json",
                        value: value.to_string(),
                    }
                })?;
                Ok(((era.provider_id, era.model_id), rule))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cache_lifetime_shipped_rules_are_valid_reviewed_and_in_seed() {
        let (rules, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rules.len(), 36);
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
        assert_eq!(load_reporting_rejects(&doc.to_string()).1.len(), 11);
        doc["rules"][0]["review_by_ms"] = json!(deadline + 1);
        assert!(!load_reporting_rejects(&doc.to_string()).1.is_empty());
    }
}
