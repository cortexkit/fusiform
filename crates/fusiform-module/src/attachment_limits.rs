//! The embedded attachment limits document and its local adapter.

pub use fusiform_curate::attachment_limits::*;
use std::sync::OnceLock;
#[cfg(test)]
use {serde_json::Value, std::collections::BTreeMap};

const DATA: &str = include_str!("../data/attachment-limits.json");
pub fn sources() -> &'static Sources {
    static ROWS: OnceLock<Sources> = OnceLock::new();
    ROWS.get_or_init(|| load(DATA).0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shipped_attachment_limits_load_without_rejects() {
        let (rows, rejects) = load(DATA);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.len(), 6);
        assert_eq!(rows.values().map(|row| row.limits.len()).sum::<usize>(), 16);
    }

    #[test]
    fn malformed_or_duplicate_attachment_evidence_is_rejected() {
        let doc: Value = serde_json::from_str(DATA).unwrap();
        for (field, value) in [
            ("provider_id", json!("openrouter")),
            ("established_at_ms", json!(0)),
            ("review_by_ms", json!(1794268800001_i64)),
            ("notes", json!("")),
            ("limits", json!([])),
        ] {
            let mut bad = doc.clone();
            bad["providers"][0][field] = value;
            let (rows, rejects) = load(&bad.to_string());
            assert_eq!(rows.len(), 5, "{field}");
            assert_eq!(rejects.len(), 1, "{field}");
        }
        for (field, value) in [
            ("fact", json!("limit.documents_per_request")),
            ("rule", json!({"kind":"fixed","value":0})),
            ("rule", json!({"kind":"fixed","value":1.5})),
            ("rule", json!({"kind":"fixed","value":-1})),
            (
                "rule",
                json!({"kind":"context_equals","context":0,"equal":1,"other":2}),
            ),
            (
                "rule",
                json!({"kind":"context_at_least","context":100,"below":0,"at_least":2}),
            ),
            (
                "source_ref",
                json!("https://platform.claude.com.evil.example/vision"),
            ),
            ("source_ref", json!("http://platform.claude.com/vision")),
            ("quote", json!("")),
            ("notes", json!("")),
        ] {
            let mut bad = doc.clone();
            bad["providers"][0]["limits"][0][field] = value;
            let (rows, rejects) = load(&bad.to_string());
            assert_eq!(rows.len(), 5, "{field}");
            assert_eq!(rejects.len(), 1, "{field}");
        }
        let mut bad = doc.clone();
        let duplicate = bad["providers"][0]["limits"][0].clone();
        bad["providers"][0]["limits"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        assert_eq!(load(&bad.to_string()).1.len(), 1);
        let mut bad = doc.clone();
        bad["providers"]
            .as_array_mut()
            .unwrap()
            .push(doc["providers"][0].clone());
        assert_eq!(load(&bad.to_string()).1.len(), 1);
        assert_eq!(load("{}").1.len(), 1);
        let mut bad = doc;
        bad["schema"] = json!("unknown");
        assert_eq!(load(&bad.to_string()).1.len(), 1);
    }

    #[test]
    fn image_count_attachment_rules_require_stricter_quoted_unique_conditions() {
        let doc: Value = serde_json::from_str(DATA).unwrap();
        for rule in [
            json!({"kind":"image_count","value":0,"conditions":[{"above_images":20,"value":3000,"quote":"sourced"}]}),
            json!({"kind":"image_count","value":8000,"conditions":[]}),
            json!({"kind":"image_count","value":8000,"conditions":[{"above_images":0,"value":3000,"quote":"sourced"}]}),
            json!({"kind":"image_count","value":8000,"conditions":[{"above_images":"020","value":3000,"quote":"sourced"}]}),
            json!({"kind":"image_count","value":8000,"conditions":[{"above_images":20,"value":0,"quote":"sourced"}]}),
            json!({"kind":"image_count","value":8000,"conditions":[{"above_images":20,"value":8000,"quote":"sourced"}]}),
            json!({"kind":"image_count","value":8000,"conditions":[{"above_images":20,"value":9000,"quote":"sourced"}]}),
            json!({"kind":"image_count","value":8000,"conditions":[{"above_images":20,"value":3000,"quote":" "}]}),
            json!({"kind":"image_count","value":8000,"conditions":[{"above_images":20,"value":3000}]}),
            json!({"kind":"image_count","value":8000,"conditions":[{"above_images":20,"value":3000,"quote":"sourced"},{"above_images":20,"value":2000,"quote":"sourced"}]}),
        ] {
            let mut bad = doc.clone();
            bad["providers"][0]["limits"][1]["rule"] = rule.clone();
            let (rows, rejects) = load(&bad.to_string());
            assert_eq!(rows.len(), 5, "{rule}");
            assert_eq!(rejects.len(), 1, "{rule}");
        }
        let mut bad = doc;
        bad["providers"][0]["limits"][0]["rule"] = bad["providers"][0]["limits"][1]["rule"].clone();
        assert_eq!(load(&bad.to_string()).1.len(), 1);
    }

    #[test]
    fn attachment_limits_are_within_their_review_dates() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let overdue = overdue_at(sources(), now);
        assert!(overdue.is_empty(), "{}", overdue.join("\n"));
    }

    #[test]
    fn planted_overdue_attachment_evidence_requires_rereading_or_removal() {
        let row = sources()["anthropic"].clone();
        let rows = Sources::from([(row.provider_id.clone(), row.clone())]);
        assert!(overdue_at(&rows, row.review_by_ms).is_empty());
        let overdue = overdue_at(&rows, row.review_by_ms + 1);
        assert_eq!(overdue.len(), 1);
        assert!(overdue[0].contains(&row.limits[0].source_ref));
        assert!(overdue[0].contains("or remove"));
    }

    fn seed() -> Value {
        serde_json::from_str(include_str!("../data/models-dev-seed.json")).unwrap()
    }

    #[test]
    fn every_attachment_limit_has_eligible_rows_in_the_embedded_seed() {
        let unused = unused_rows(sources(), &seed());
        assert!(unused.is_empty(), "{}", unused.join("\n"));
    }

    #[test]
    fn planted_seed_without_image_or_pdf_input_names_each_unused_limit() {
        let mut seed = seed();
        for model in seed["anthropic"]["models"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            model["modalities"]["input"] = json!(["text"]);
        }
        let unused = unused_rows(sources(), &seed);
        assert_eq!(unused.len(), 4, "{unused:?}");
        for key in [
            "limit.images_per_request",
            "limit.image_bytes",
            "limit.image_long_edge_px",
            "limit.document_pages",
        ] {
            assert!(unused
                .iter()
                .any(|line| line.contains(key) && line.starts_with("anthropic:")));
        }
    }

    #[test]
    fn absent_null_or_zero_context_never_implies_a_context_dependent_bound() {
        for context in [None, Some(Value::Null), Some(json!(0))] {
            let mut facts = BTreeMap::from([(
                "capability.input_modalities".into(),
                json!(["image", "pdf"]),
            )]);
            if let Some(context) = context {
                facts.insert("limit.context".into(), context);
            }
            apply(sources(), "anthropic", &mut facts);
            assert!(!facts.contains_key("limit.images_per_request"));
            assert!(!facts.contains_key("limit.document_pages"));
            assert_eq!(facts["limit.image_bytes"], 7500000);
        }
    }

    #[test]
    fn absent_null_or_wrong_modalities_never_get_attachment_bounds() {
        for modalities in [
            None,
            Some(Value::Null),
            Some(json!(["audio"])),
            Some(json!("image")),
        ] {
            let mut facts = BTreeMap::from([("limit.context".into(), json!(1000000))]);
            if let Some(modalities) = modalities {
                facts.insert("capability.input_modalities".into(), modalities);
            }
            apply(sources(), "anthropic", &mut facts);
            assert!(KEYS.iter().all(|key| !facts.contains_key(*key)));
            assert!(facts.keys().all(|key| !key.contains(".above_images.")));
        }
    }
}
