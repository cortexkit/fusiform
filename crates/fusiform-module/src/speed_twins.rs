//! The embedded speed twins document and its local adapter.

pub use fusiform_curate::speed_twins::*;

const DOCUMENT: &str = include_str!("../data/speed-twins.json");
/// Embedded curation is immutable for the process, so parse and validate it once.
pub fn rows() -> &'static SpeedTwins {
    static ROWS: std::sync::OnceLock<SpeedTwins> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

pub fn twin(provider: &str, model: &str) -> Option<&'static SpeedTwin> {
    rows()
        .values()
        .find(|row| row.provider_id == provider && row.twin_model_id == model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> serde_json::Value {
        serde_json::from_str(DOCUMENT).unwrap()
    }

    #[test]
    fn shipped_speed_twins_load_and_match_the_seed() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.len(), 2);
        let seed = serde_json::from_str(include_str!("../data/models-dev-seed.json")).unwrap();
        assert!(missing_seed_rows(&rows, &seed).is_empty());
        assert!(native_mode_collisions(&rows, &crate::seed::catalog().unwrap()).is_empty());
        assert!(twin("kimi-code-plan-global", "kimi-for-coding-highspeed").is_none());
    }

    #[test]
    fn planted_missing_base_and_twin_fire_the_seed_fence() {
        let mut seed: serde_json::Value =
            serde_json::from_str(include_str!("../data/models-dev-seed.json")).unwrap();
        for model in ["k3.1-pro", "k3.1-pro-highspeed"] {
            let saved = seed["kimi-code-plan-global"]["models"]
                .as_object_mut()
                .unwrap()
                .remove(model)
                .unwrap();
            let missing = missing_seed_rows(rows(), &seed);
            assert_eq!(missing.len(), 1);
            assert!(missing[0].contains(model));
            seed["kimi-code-plan-global"]["models"][model] = saved;
        }
    }

    #[test]
    fn planted_native_mode_fires_the_collision_fence() {
        let mut catalog = crate::seed::catalog().unwrap();
        let base = catalog
            .providers
            .iter_mut()
            .flat_map(|provider| &mut provider.models)
            .find(|model| {
                model.key.provider_id == "kimi-code-plan-global" && model.key.model_id == "k3.1-pro"
            })
            .unwrap();
        let mut rate = base.rates[0].clone();
        rate.condition = fusiform_core::RateCondition::Mode {
            name: "fast".into(),
        };
        base.rates.push(rate);
        let collisions = native_mode_collisions(rows(), &catalog);
        assert_eq!(collisions.len(), 1);
        assert!(collisions[0].contains(".mode.fast"));
    }

    #[test]
    fn planted_twin_as_base_is_rejected() {
        let mut doc = document();
        let mut chained = doc["speed_twins"][0].clone();
        chained["base_model_id"] = json!("k3.1-pro-highspeed");
        chained["twin_model_id"] = json!("another-model");
        doc["speed_twins"].as_array_mut().unwrap().push(chained);
        let (_, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("also a base"));
    }

    #[test]
    fn planted_unknown_mode_is_rejected() {
        let mut doc = document();
        doc["speed_twins"][0]["mode"] = json!("highspeed");
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rows.len(), 1);
        assert!(rows.values().all(|row| row.provider_id == "moonshotai"));
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("fast or ultrafast"));
        doc["speed_twins"][0]["mode"] = json!("ultrafast");
        assert!(load_reporting_rejects(&doc.to_string()).1.is_empty());
    }

    #[test]
    fn malformed_duplicate_and_self_referential_claims_are_rejected() {
        for (key, value) in [
            ("quote", json!(" ")),
            ("api_card", json!("missing-provider")),
            ("review_by_ms", json!(1)),
            ("unknown", json!(true)),
            ("twin_model_id", json!("k3.1-pro")),
        ] {
            let mut doc = document();
            doc["speed_twins"][0][key] = value;
            assert_eq!(load_reporting_rejects(&doc.to_string()).1.len(), 1, "{key}");
        }
        let mut doc = document();
        let duplicate = doc["speed_twins"][0].clone();
        doc["speed_twins"].as_array_mut().unwrap().push(duplicate);
        assert_eq!(load_reporting_rejects(&doc.to_string()).1.len(), 1);
    }

    #[test]
    fn speed_twins_are_within_their_review_dates() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let overdue = overdue_at(rows(), now);
        assert!(
            overdue.is_empty(),
            "NOT A BUILD FAILURE. A speed twin review date passed.\n{}",
            overdue.join("\n")
        );
    }

    #[test]
    fn planted_overdue_claim_names_the_source_and_remedy() {
        for (key, row) in rows() {
            let pair = SpeedTwins::from([(key.clone(), row.clone())]);
            assert!(overdue_at(&pair, row.review_by_ms).is_empty());
            let overdue = overdue_at(&pair, row.review_by_ms + 1);
            assert_eq!(overdue.len(), 1);
            assert!(overdue[0].contains(&row.source_ref));
            assert!(overdue[0].contains("60 days later"));
        }
    }
}
