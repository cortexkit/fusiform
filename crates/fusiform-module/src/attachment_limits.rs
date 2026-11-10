//! First-party attachment bounds joined only onto the current served view.
//! Missing evidence means unknown; neither zero nor "unlimited" is a bound.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

const DATA: &str = include_str!("../data/attachment-limits.json");
const REVIEW_INTERVAL_MS: i64 = 60 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
pub enum Fact {
    #[serde(rename = "limit.images_per_request")]
    ImagesPerRequest,
    #[serde(rename = "limit.image_bytes")]
    ImageBytes,
    #[serde(rename = "limit.image_long_edge_px")]
    ImageLongEdgePx,
    #[serde(rename = "limit.document_pages")]
    DocumentPages,
    #[serde(rename = "limit.document_bytes")]
    DocumentBytes,
}

impl Fact {
    pub fn key(self) -> &'static str {
        match self {
            Self::ImagesPerRequest => "limit.images_per_request",
            Self::ImageBytes => "limit.image_bytes",
            Self::ImageLongEdgePx => "limit.image_long_edge_px",
            Self::DocumentPages => "limit.document_pages",
            Self::DocumentBytes => "limit.document_bytes",
        }
    }

    fn modality(self) -> &'static str {
        match self {
            Self::ImagesPerRequest | Self::ImageBytes | Self::ImageLongEdgePx => "image",
            Self::DocumentPages | Self::DocumentBytes => "pdf",
        }
    }
}

pub const KEYS: &[&str] = &[
    "limit.images_per_request",
    "limit.image_bytes",
    "limit.image_long_edge_px",
    "limit.document_pages",
    "limit.document_bytes",
];

pub const IMAGE_COUNT_FAMILY: &str = "limit.image_long_edge_px.above_images.<n>";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageCountCondition {
    pub above_images: u64,
    pub value: u64,
    pub quote: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Rule {
    Fixed {
        value: u64,
    },
    ImageCount {
        value: u64,
        conditions: Vec<ImageCountCondition>,
    },
    ContextEquals {
        context: u64,
        equal: u64,
        other: u64,
    },
    ContextAtLeast {
        context: u64,
        below: u64,
        at_least: u64,
    },
}

impl Rule {
    fn value(&self, context: Option<u64>) -> Option<u64> {
        match *self {
            Self::Fixed { value } | Self::ImageCount { value, .. } => Some(value),
            Self::ContextEquals {
                context: threshold,
                equal,
                other,
            } => context.map(|context| if context == threshold { equal } else { other }),
            Self::ContextAtLeast {
                context: threshold,
                below,
                at_least,
            } => context.map(|context| {
                if context >= threshold {
                    at_least
                } else {
                    below
                }
            }),
        }
    }

    fn valid(&self) -> bool {
        match self {
            Self::Fixed { value } => *value > 0,
            Self::ImageCount { value, conditions } => {
                let mut thresholds = BTreeSet::new();
                *value > 0
                    && !conditions.is_empty()
                    && conditions.iter().all(|condition| {
                        condition.above_images > 0
                            && thresholds.insert(condition.above_images)
                            && condition.value > 0
                            && condition.value < *value
                            && !condition.quote.trim().is_empty()
                    })
            }
            Self::ContextEquals {
                context,
                equal,
                other,
            } => *context > 0 && *equal > 0 && *other > 0,
            Self::ContextAtLeast {
                context,
                below,
                at_least,
            } => *context > 0 && *below > 0 && *at_least > 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limit {
    pub fact: Fact,
    pub rule: Rule,
    pub source_ref: String,
    pub quote: String,
    pub notes: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub provider_id: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
    pub notes: String,
    pub limits: Vec<Limit>,
}

pub type Sources = BTreeMap<String, Provider>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema: String,
    providers: Vec<Value>,
}

fn first_party_source(provider: &str, source: &str) -> bool {
    let prefix = match provider {
        "anthropic" => "https://platform.claude.com/",
        "openai" => "https://platform.openai.com/",
        "google" => "https://ai.google.dev/",
        "xai" => "https://docs.x.ai/",
        "meta" => "https://dev.meta.ai/",
        "deepseek" => "https://api-docs.deepseek.com/",
        _ => return false,
    };
    source.starts_with(prefix)
}

pub fn load(text: &str) -> (Sources, Vec<String>) {
    let mut rows = Sources::new();
    let mut rejects = Vec::new();
    let doc: Document = match serde_json::from_str(text) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                rows,
                vec![format!("attachment limits did not parse: {error}")],
            )
        }
    };
    if doc.schema != "attachment-limits/v1" {
        return (rows, vec!["unknown attachment limits schema".into()]);
    }
    for raw in doc.providers {
        let row: Provider = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!("attachment limits row did not parse: {error}"));
                continue;
            }
        };
        let mut facts = BTreeSet::new();
        if row.notes.trim().is_empty()
            || row.limits.is_empty()
            || row.established_at_ms <= 0
            || row.review_by_ms.checked_sub(row.established_at_ms) != Some(REVIEW_INTERVAL_MS)
            || row.limits.iter().any(|limit| {
                !facts.insert(limit.fact)
                    || !limit.rule.valid()
                    || (matches!(limit.rule, Rule::ImageCount { .. })
                        && limit.fact != Fact::ImageLongEdgePx)
                    || !first_party_source(&row.provider_id, &limit.source_ref)
                    || limit.quote.trim().is_empty()
                    || limit.notes.trim().is_empty()
            })
            || rows.contains_key(&row.provider_id)
        {
            rejects.push(format!(
                "invalid or duplicate attachment limits: {}",
                row.provider_id
            ));
        } else {
            rows.insert(row.provider_id.clone(), row);
        }
    }
    (rows, rejects)
}

pub fn sources() -> &'static Sources {
    static ROWS: OnceLock<Sources> = OnceLock::new();
    ROWS.get_or_init(|| load(DATA).0)
}

pub fn apply(provider: &str, facts: &mut BTreeMap<String, Value>) {
    let Some(row) = sources().get(provider) else {
        return;
    };
    let context = facts
        .get("limit.context")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0);
    for limit in &row.limits {
        let accepts = facts
            .get("capability.input_modalities")
            .and_then(Value::as_array)
            .is_some_and(|modalities| {
                modalities
                    .iter()
                    .any(|m| m.as_str() == Some(limit.fact.modality()))
            });
        if accepts {
            if let Some(value) = limit.rule.value(context) {
                facts.insert(limit.fact.key().into(), value.into());
            }
            if let Rule::ImageCount { conditions, .. } = &limit.rule {
                for condition in conditions {
                    facts.insert(
                        format!(
                            "{}.above_images.{}",
                            limit.fact.key(),
                            condition.above_images
                        ),
                        condition.value.into(),
                    );
                }
            }
        }
    }
}

pub fn overdue_at(rows: &Sources, now_ms: i64) -> Vec<String> {
    rows.values().filter(|row| row.review_by_ms < now_ms).map(|row| {
        format!("NOT A BUILD FAILURE: attachment limits review date passed for {}: re-read {}, confirm the quotes and bounds, then re-date established_at_ms and review_by_ms (60 days later), or remove the row", row.provider_id, row.limits.iter().map(|limit| limit.source_ref.as_str()).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>().join(", "))
    }).collect()
}

pub fn unused_rows(rows: &Sources, seed: &Value) -> Vec<String> {
    let mut unused = Vec::new();
    for row in rows.values() {
        for limit in &row.limits {
            let used = seed[&row.provider_id]["models"]
                .as_object()
                .is_some_and(|models| {
                    models.values().any(|model| {
                        model["modalities"]["input"]
                            .as_array()
                            .is_some_and(|modalities| {
                                modalities
                                    .iter()
                                    .any(|m| m.as_str() == Some(limit.fact.modality()))
                            })
                            && limit
                                .rule
                                .value(model["limit"]["context"].as_u64().filter(|n| *n > 0))
                                .is_some()
                    })
                });
            if !used {
                unused.push(format!("{}: no eligible embedded seed row for {}; correct or remove the attachment limit", row.provider_id, limit.fact.key()));
            }
        }
    }
    unused
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
            apply("anthropic", &mut facts);
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
            apply("anthropic", &mut facts);
            assert!(KEYS.iter().all(|key| !facts.contains_key(*key)));
            assert!(facts.keys().all(|key| !key.contains(".above_images.")));
        }
    }
}
