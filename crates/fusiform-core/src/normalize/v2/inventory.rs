//! Inventory of published object keys. A map with caller-chosen keys (model
//! ids, modes, TTLs, headers, etc.) is not a schema object.

use super::raw::Object;
use super::{FindingKey, Findings, RecordFinding};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    Provider,
    Model,
    Modalities,
    Capabilities,
    Tools,
    Reasoning,
    ReasoningDefault,
    ReasoningBudget,
    Cache,
    ImplicitCache,
    ExplicitCache,
    Limit,
    ImageLimit,
    PdfLimit,
    Api,
    Sdk,
    Compatibility,
    ApiCapabilities,
    ApiTools,
    Cost,
    ModalityCost,
    Input,
    Output,
    InputOverride,
    OutputOverride,
    CacheCost,
    CachePrices,
    WebSearch,
    SearchOverride,
    Multiplier,
    When,
    Predicate,
    Experimental,
    Mode,
}

#[derive(Clone, Copy)]
enum Child {
    Object(Kind),
    Array(Kind),
    Map(Kind),
}

struct Key {
    name: &'static str,
    unread: Option<&'static str>,
    child: Option<Child>,
}

macro_rules! keys {
    ($($name:literal $(: $reason:expr)? $(=> $shape:ident($kind:ident))?),* $(,)?) => {
        &[$(Key {
            name: $name,
            unread: keys!(@opt $($reason)?),
            child: keys!(@opt $(Child::$shape(Kind::$kind))?),
        }),*]
    };
    (@opt $value:expr) => { Some($value) };
    (@opt) => { None };
}

const LABEL: &str = "metadata, not a served model fact";
const NOT_SERVED: &str = "no admitted fact for this dimension";
const REQUEST: &str = "request shaping belongs to the caller";
const MODE_CAPABILITIES: &str = "mode-specific capabilities are not served";
const DEFERRED: &str = "conditional price projection is handled separately from base prices";

impl Kind {
    fn keys(self) -> &'static [Key] {
        match self {
            Self::Provider => keys!["id": LABEL, "name", "doc": LABEL, "env": LABEL, "models"],
            Self::Model => keys![
                "id": "the containing map supplies the model identity", "type", "name",
                "description": LABEL, "canonical_id", "family", "open_weights", "knowledge",
                "release_date", "last_updated", "deprecation_date": NOT_SERVED, "status",
                "modalities" => Object(Modalities), "capabilities" => Object(Capabilities),
                "limit" => Object(Limit), "cost" => Object(Cost), "api" => Map(Api),
                "recommended_api": "an upstream recommendation, not the first published API",
                "experimental" => Object(Experimental), "metadata": LABEL,
            ],
            Self::Modalities => keys!["input", "output"],
            Self::Capabilities => keys![
                "tools" => Object(Tools), "reasoning" => Object(Reasoning),
                "cache": NOT_SERVED => Object(Cache), "structured_output", "temperature",
                "top_p": REQUEST, "top_k": REQUEST, "system_updates": NOT_SERVED,
                "effort_updates": NOT_SERVED, "store": REQUEST, "stream_usage": REQUEST,
                "prompt_cache_key": REQUEST, "service_tier": REQUEST,
            ],
            Self::Tools => keys!["supported"],
            Self::Reasoning => keys![
                "supported", "toggle", "effort", "budget" => Object(ReasoningBudget),
                "default": "only supported options are served" => Object(ReasoningDefault),
            ],
            Self::ReasoningDefault => keys!["type": NOT_SERVED, "value": NOT_SERVED],
            Self::ReasoningBudget => keys!["min", "max"],
            Self::Cache => keys![
                "implicit": NOT_SERVED => Object(ImplicitCache),
                "explicit": NOT_SERVED => Object(ExplicitCache),
            ],
            Self::ImplicitCache => keys!["ttl": NOT_SERVED, "min_tokens": NOT_SERVED],
            Self::ExplicitCache => keys![
                "ttl": NOT_SERVED, "min_tokens": NOT_SERVED, "max_breakpoints": NOT_SERVED,
                "targets": NOT_SERVED,
            ],
            Self::Limit => keys![
                "context", "input": NOT_SERVED, "output", "request_bytes": NOT_SERVED,
                "image": NOT_SERVED => Object(ImageLimit), "pdf": NOT_SERVED => Object(PdfLimit),
            ],
            Self::ImageLimit => keys![
                "max_per_message": NOT_SERVED, "max_per_request": NOT_SERVED,
                "max_bytes": NOT_SERVED, "min_width": NOT_SERVED, "min_height": NOT_SERVED,
                "max_width": NOT_SERVED, "max_height": NOT_SERVED,
            ],
            Self::PdfLimit => keys![
                "max_per_request": NOT_SERVED, "max_bytes": NOT_SERVED, "max_pages": NOT_SERVED,
            ],
            Self::Api => keys![
                "base_url", "path": "the wire fact describes a base URL, not a request path",
                "sdk" => Object(Sdk), "compatibility": REQUEST => Object(Compatibility),
                "capabilities": NOT_SERVED => Object(ApiCapabilities),
            ],
            Self::Sdk => keys!["aisdk", "opencodeai": "no admitted wire fact for this SDK"],
            Self::Compatibility => keys![
                "reasoning_field": REQUEST, "max_tokens_field": REQUEST, "media_input": REQUEST,
            ],
            Self::ApiCapabilities => keys![
                "system_updates": NOT_SERVED, "effort_updates": NOT_SERVED,
                "tools": NOT_SERVED => Object(ApiTools),
            ],
            Self::ApiTools => keys![
                "strict": NOT_SERVED, "choice": NOT_SERVED, "search": NOT_SERVED,
                "freeform": NOT_SERVED, "updates": NOT_SERVED,
            ],
            Self::Cost => keys![
                "text" => Object(ModalityCost), "image" => Object(ModalityCost),
                "audio" => Object(ModalityCost), "video" => Object(ModalityCost),
                "web_search" => Object(WebSearch), "multipliers" => Array(Multiplier),
            ],
            Self::ModalityCost => keys!["input" => Object(Input), "output" => Object(Output)],
            Self::Input => keys![
                "price", "unit", "cache" => Object(CacheCost), "overrides" => Array(InputOverride),
            ],
            Self::Output => {
                keys!["price", "unit", "reasoning", "overrides" => Array(OutputOverride)]
            }
            Self::InputOverride => keys![
                "when" => Object(When), "price": DEFERRED, "cache": DEFERRED => Object(CacheCost),
            ],
            Self::OutputOverride => {
                keys!["when" => Object(When), "price": DEFERRED, "reasoning": DEFERRED]
            }
            Self::CacheCost => keys![
                "read", "write", "storage": NOT_SERVED, "explicit" => Object(CachePrices),
            ],
            Self::CachePrices => keys!["read": NOT_SERVED, "write", "storage": NOT_SERVED],
            Self::WebSearch => keys!["price": NOT_SERVED, "overrides" => Array(SearchOverride)],
            Self::SearchOverride => keys!["price": NOT_SERVED, "when" => Object(When)],
            Self::Multiplier => keys!["when" => Object(When), "factor": DEFERRED],
            Self::When => keys![
                "context" => Object(Predicate), "duration" => Object(Predicate),
                "megapixels" => Object(Predicate), "input_images" => Object(Predicate),
                "service_tier" => Object(Predicate), "region" => Object(Predicate),
                "date" => Object(Predicate), "utc_days" => Object(Predicate), "utc_hours" => Object(Predicate),
                "holiday" => Object(Predicate), "thinking" => Object(Predicate), "resolution" => Object(Predicate),
                "quality" => Object(Predicate), "aspect_ratio" => Object(Predicate), "generate_audio" => Object(Predicate),
                "mode" => Object(Predicate), "input_video" => Object(Predicate), "fps" => Object(Predicate),
                "steps" => Object(Predicate), "search_context_size" => Object(Predicate),
                "speaker_labels" => Object(Predicate), "keyterms_prompt" => Object(Predicate),
                "domain" => Object(Predicate), "websocket" => Object(Predicate), "style" => Object(Predicate),
                "voice_control" => Object(Predicate), "all" => Array(When), "any" => Array(When), "not" => Object(When),
            ],
            Self::Predicate => keys![
                "equals": DEFERRED, "in": DEFERRED, "gt": DEFERRED, "gte": DEFERRED,
                "lt": DEFERRED, "lte": DEFERRED, "min_items": DEFERRED,
            ],
            Self::Experimental => keys!["modes" => Map(Mode)],
            Self::Mode => keys![
                "service_tier": DEFERRED, "cost": DEFERRED => Object(Cost),
                "capabilities": MODE_CAPABILITIES => Object(Capabilities), "body": REQUEST, "headers": REQUEST,
            ],
        }
    }

    pub(super) fn inventory(
        self,
        path: &str,
        object: &Object<'_>,
        model: Option<&str>,
        findings: &mut Findings,
    ) {
        for (name, value) in &object.0 {
            let Some(key) = self.keys().iter().find(|key| key.name == name) else {
                findings.add(FindingKey::unknown_key(self, path, name), model);
                continue;
            };
            debug_assert!(key.unread.is_none_or(|reason| !reason.is_empty()));
            match key.child {
                Some(Child::Object(kind)) => {
                    if let Ok(child) = Object::parse(value) {
                        kind.inventory(&format!("{path}.{name}"), &child, model, findings);
                    }
                }
                Some(Child::Array(kind)) => {
                    if let Some(items) = object.array(name) {
                        let path = format!("{path}.{name}[]");
                        for item in items {
                            if let Ok(child) = Object::parse(item) {
                                kind.inventory(&path, &child, model, findings);
                            }
                        }
                    }
                }
                Some(Child::Map(kind)) => {
                    if let Ok(entries) = Object::parse(value) {
                        let path = format!("{path}.{name}.*");
                        for (_, item) in entries.0 {
                            if let Ok(child) = Object::parse(item) {
                                kind.inventory(&path, &child, model, findings);
                            }
                        }
                    }
                }
                None => {}
            }
        }
    }
}
