//! First-party attachment bounds joined only onto the current served view.
//! Missing evidence means unknown; neither zero nor "unlimited" is a bound.

use std::collections::{BTreeMap, BTreeSet};

const REVIEW_INTERVAL_MS: i64 = 60 * 24 * 60 * 60 * 1000;

use serde::Deserialize;
use serde_json::Value;

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

pub fn apply(rows: &Sources, provider: &str, facts: &mut BTreeMap<String, Value>) {
    let Some(row) = rows.get(provider) else {
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
                                modalities.iter().any(|m| {
                                    m.as_str().map(fusiform_core::normalize::v2::modality_label)
                                        == Some(limit.fact.modality())
                                })
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
