//! Normalize the published models.dev v2 provider map without a v1 fallback.

mod inventory;
mod rates;
mod raw;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::Deserializer;
use serde_json::{json, value::RawValue, Value};

use super::{
    absent_if_zero, wire_claim, Capabilities, ChargeUnitEvidence, Limits, NormalizeError,
    NormalizedCatalog, NormalizedModel, NormalizedProvider, Quarantine, RefusedFactScope,
    WireFacts,
};
use crate::upstream_facts::FactKey;
use crate::{Modality, ModelKey, SourceId};
use inventory::Kind;
use raw::{Object, RawModel, RawProvider};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingKey {
    UnknownKey { object_kind: String, path: String },
    UnknownModelType { value: String },
    UnknownApiProtocol { value: String },
    UnknownCostUnit { role: String, value: String },
    UnservedCost { role: String, unit: String },
    UnattributableChargeUnit { role: String },
    RefusedPrice { role: String, detail: String },
    ConditionDeferred { role: String, condition: String },
}

impl FindingKey {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::UnknownKey { .. } => "unknown_key",
            Self::UnknownModelType { .. } => "unknown_model_type",
            Self::UnknownApiProtocol { .. } => "unknown_api_protocol",
            Self::UnknownCostUnit { .. } => "unknown_cost_unit",
            Self::UnservedCost { .. } => "unserved_cost",
            Self::UnattributableChargeUnit { .. } => "unattributable_charge_unit",
            Self::RefusedPrice { .. } => "refused_price",
            Self::ConditionDeferred { .. } => "condition_deferred",
        }
    }

    fn unknown_key(kind: Kind, path: &str, name: &str) -> Self {
        Self::UnknownKey {
            object_kind: format!("{kind:?}"),
            path: format!("{path}.{name}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub key: FindingKey,
    pub occurrences: usize,
    /// The first affected model id in document order. A provider with no
    /// models can still have an unknown provider key.
    pub first_model_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizeV2Outcome {
    pub catalog: NormalizedCatalog,
    pub findings: Vec<Finding>,
}

impl NormalizeV2Outcome {
    /// Counts for `NormalizationFindingsWire.counts`: total occurrences per
    /// finding kind, not the number of distinct diagnostic messages.
    pub fn finding_counts(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for finding in &self.findings {
            *counts.entry(finding.key.kind().to_owned()).or_default() += finding.occurrences;
        }
        counts
    }
}

type Findings = BTreeMap<FindingKey, (usize, Option<String>)>;

trait RecordFinding {
    fn add(&mut self, key: FindingKey, model: Option<&str>);
}

impl RecordFinding for Findings {
    fn add(&mut self, key: FindingKey, model: Option<&str>) {
        self.entry(key)
            .and_modify(|(count, first)| {
                *count += 1;
                if first.is_none() {
                    *first = model.map(str::to_owned);
                }
            })
            .or_insert_with(|| (1, model.map(str::to_owned)));
    }
}

/// Normalize only the v2 shape. The evidence is still required for token-billed
/// audio; the upstream's unit alone does not make an audio rate attributable.
/// No catalog escapes until the complete document has been validated.
pub fn normalize_models_dev_v2_with(
    bytes: &[u8],
    evidence: &ChargeUnitEvidence,
) -> Result<NormalizeV2Outcome, NormalizeError> {
    let document_parse = |error: serde_json::Error| NormalizeError::DocumentParse {
        diagnostic: error.to_string(),
    };
    let first = bytes
        .iter()
        .copied()
        .find(|byte| !matches!(byte, b' ' | b'\n' | b'\r' | b'\t'));
    if first != Some(b'{') {
        serde_json::from_slice::<de::IgnoredAny>(bytes).map_err(document_parse)?;
        return Err(NormalizeError::NotAProviderMap);
    }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let outcome = decoder
        .deserialize_map(ProviderMap { evidence })
        .map_err(document_parse)?;
    decoder.end().map_err(document_parse)?;
    Ok(outcome)
}

struct ProviderMap<'a> {
    evidence: &'a ChargeUnitEvidence,
}

impl<'de> Visitor<'de> for ProviderMap<'_> {
    type Value = NormalizeV2Outcome;

    fn expecting(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.write_str("a v2 provider map")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        use de::Error;

        let mut providers = BTreeMap::new();
        let mut findings = Findings::new();
        let mut model_count = 0;
        // Borrow each provider's JSON directly. Keeping its raw fields out of
        // the rest of the loop bounds temporary allocations to one provider.
        while let Some((id, raw)) = map.next_entry::<String, &RawValue>()? {
            let provider = RawProvider::parse(raw)
                .map_err(|error| M::Error::custom(format_args!("provider {id:?}: {error}")))?;
            let provider = normalize_provider(&id, provider, self.evidence, &mut findings)
                .map_err(M::Error::custom)?;
            model_count += provider.models.len();
            if providers.insert(id, provider).is_some() {
                return Err(M::Error::custom("duplicate provider id"));
            }
        }
        if model_count == 0 {
            return Err(M::Error::custom("no models with the v2 shape"));
        }
        Ok(NormalizeV2Outcome {
            catalog: NormalizedCatalog {
                source: SourceId::ModelsDev,
                providers: providers.into_values().collect(),
            },
            findings: findings
                .into_iter()
                .map(|(key, (occurrences, first_model_id))| Finding {
                    key,
                    occurrences,
                    first_model_id,
                })
                .collect(),
        })
    }
}

fn normalize_provider(
    id: &str,
    provider: RawProvider<'_>,
    evidence: &ChargeUnitEvidence,
    findings: &mut Findings,
) -> Result<NormalizedProvider, String> {
    let name = provider
        .fields
        .read::<String>("name")
        .filter(|name| !name.is_empty());
    Kind::Provider.inventory(
        "provider",
        &provider.fields,
        provider.models.first().map(|(model, _)| model.as_str()),
        findings,
    );
    let mut models = BTreeMap::new();
    for (model_id, raw) in provider.models {
        let raw =
            RawModel::parse(raw).map_err(|error| format!("model {id}/{model_id}: {error}"))?;
        let model = normalize_model(id, &model_id, name.as_deref(), raw, evidence, findings);
        if models.insert(model_id, model).is_some() {
            return Err(format!("duplicate model id in provider {id:?}"));
        }
    }
    Ok(NormalizedProvider {
        provider_id: id.to_owned(),
        // V2 puts resolved endpoints and SDKs on each model, not on providers.
        api_base_quarantined: false,
        npm_quarantined: false,
        models: models.into_values().collect(),
    })
}

fn normalize_model(
    provider: &str,
    model: &str,
    provider_name: Option<&str>,
    raw: RawModel<'_>,
    evidence: &ChargeUnitEvidence,
    findings: &mut Findings,
) -> NormalizedModel {
    let RawModel {
        fields,
        model_type,
        capabilities,
        api,
    } = raw;
    Kind::Model.inventory("model", &fields, Some(model), findings);
    if !matches!(
        model_type.as_str(),
        "chat"
            | "image"
            | "video"
            | "embedding"
            | "reranking"
            | "decision"
            | "transcription"
            | "speech"
            | "realtime"
            | "extraction"
            | "audio"
    ) {
        findings.add(
            FindingKey::UnknownModelType {
                value: model_type.clone(),
            },
            Some(model),
        );
    }
    let mut refused = BTreeSet::new();
    let modes = fields
        .object("experimental")
        .and_then(|experimental| experimental.object("modes"));
    if let Some(modes) = &modes {
        for (name, mode) in &modes.0 {
            if let Ok(mode) = Object::parse(mode) {
                if let Some(cost) = mode.object("cost") {
                    rates::defer_mode(model, name, &cost, findings);
                }
            }
        }
    }
    let modalities = fields.object("modalities");
    let input_modalities = read_modalities(modalities.as_ref(), "input");
    let attachment = input_modalities
        .as_ref()
        .map(|items| items.iter().any(|value| *value != Modality::Text));
    let reasoning = capabilities.object("reasoning");
    let limit = fields.object("limit");
    let key = ModelKey {
        source: SourceId::ModelsDev,
        provider_id: provider.to_owned(),
        model_id: model.to_owned(),
    };
    let rates = fields.object("cost").map_or_else(Vec::new, |cost| {
        rates::base_rates(&key, &cost, evidence, findings)
    });
    NormalizedModel {
        key,
        provider_name: provider_name.map(str::to_owned),
        display_name: fields
            .read::<String>("name")
            .filter(|name| !name.is_empty()),
        family: fields.read("family"),
        canonical_id: fields.read("canonical_id"),
        status: fields.read("status"),
        model_type: Some(model_type),
        release_date: fields.read("release_date"),
        last_updated: fields.read("last_updated"),
        knowledge_cutoff: fields.read("knowledge"),
        open_weights: fields.read("open_weights"),
        capabilities: Capabilities {
            input_modalities,
            output_modalities: read_modalities(modalities.as_ref(), "output"),
            attachment,
            reasoning: reasoning
                .as_ref()
                .and_then(|reasoning| reasoning.read("supported")),
            reasoning_options: reasoning.as_ref().and_then(reasoning_options),
            tool_call: capabilities
                .object("tools")
                .and_then(|tools| tools.read("supported")),
            structured_output: capabilities.read("structured_output"),
            temperature: capabilities.read("temperature"),
        },
        limits: Limits {
            context_tokens: absent_if_zero(limit.as_ref().and_then(|limit| limit.read("context"))),
            output_tokens: absent_if_zero(limit.as_ref().and_then(|limit| limit.read("output"))),
        },
        wire: normalize_api(model, &api, findings, &mut refused),
        rates,
        refused,
        quarantined: Quarantine {
            provider_override: false,
            experimental_modes: modes
                .into_iter()
                .flat_map(|mode| mode.0)
                .map(|(name, _)| name)
                .collect(),
        },
    }
}

fn read_modalities(modalities: Option<&Object<'_>>, direction: &str) -> Option<Vec<Modality>> {
    let published: Vec<String> = modalities?.read(direction)?;
    // MIME subtypes describe the same catalog modality. Output classes that
    // v1 grouped with text are still distinguished by the model's v2 type.
    let mut base = Vec::new();
    for value in published {
        let name = match value.as_str() {
            "embedding" | "decision" | "reranking" => "text",
            "application/pdf" => "pdf",
            other => match other.split_once('/') {
                Some((kind @ ("text" | "image" | "audio" | "video"), _)) => kind,
                _ => other,
            },
        };
        let modality = match name {
            "text" => Modality::Text,
            "image" => Modality::Image,
            "audio" => Modality::Audio,
            "video" => Modality::Video,
            "pdf" => Modality::Pdf,
            _ => Modality::Other(name.to_owned()),
        };
        if !base.contains(&modality) {
            base.push(modality);
        }
    }
    Some(base)
}

fn reasoning_options(reasoning: &Object<'_>) -> Option<Value> {
    if reasoning.read("supported") != Some(true) {
        return None;
    }
    let mut options = Vec::new();
    if reasoning.read("toggle") == Some(true) {
        options.push(json!({"type": "toggle"}));
    }
    if let Some(effort) = reasoning.read::<Vec<Value>>("effort") {
        options.push(json!({"type": "effort", "values": effort}));
    }
    if let Some(budget) = reasoning.object("budget") {
        let mut option = json!({"type": "budget_tokens"});
        for field in ["min", "max"] {
            if let Some(tokens) = budget.read::<u64>(field) {
                option[field] = json!(tokens);
            }
        }
        options.push(option);
    }
    Some(Value::Array(options))
}

fn normalize_api(
    model: &str,
    api: &Object<'_>,
    findings: &mut Findings,
    refused: &mut BTreeSet<RefusedFactScope>,
) -> WireFacts {
    let mut wire = WireFacts::default();
    for (index, (protocol, raw)) in api.0.iter().enumerate() {
        if !known_api_protocol(protocol) {
            findings.add(
                FindingKey::UnknownApiProtocol {
                    value: protocol.clone(),
                },
                Some(model),
            );
            if index == 0 {
                // Do not fall back to the second API: it need not accept the
                // same requests or represent the endpoint previously served.
                refused.extend(
                    ["api", "npm", "shape"].map(|name| RefusedFactScope::Fact(FactKey::wire(name))),
                );
            }
        } else if index == 0 {
            if let Ok(entry) = Object::parse(raw) {
                let npm = entry
                    .object("sdk")
                    .and_then(|sdk| sdk.read::<String>("aisdk"));
                let api = entry.read::<String>("base_url");
                let defaults_to_responses = matches!(
                    npm.as_deref(),
                    Some("@ai-sdk/openai" | "@ai-sdk/azure" | "@ai-sdk/xai")
                );
                let shape = match (protocol.as_str(), defaults_to_responses) {
                    ("chat-completions", true) => Some("completions"),
                    ("responses", false) => Some("responses"),
                    _ => None,
                };
                wire = WireFacts {
                    api: wire_claim(api.as_deref(), None),
                    npm: wire_claim(npm.as_deref(), None),
                    shape: wire_claim(shape, None),
                };
            }
        }
    }
    wire
}

fn known_api_protocol(value: &str) -> bool {
    matches!(
        value,
        "chat-completions"
            | "responses"
            | "messages"
            | "converse"
            | "generate-content"
            | "interactions"
            | "cohere-chat"
            | "systemone"
            | "decisions"
            | "gemini-decisions"
            | "evaluate"
            | "workers-ai-run"
            | "images"
            | "videos"
            | "realtime"
            | "live"
            | "bidi-generate-content"
            | "bidi-generate-music"
            | "rerank"
            | "speech"
            | "transcriptions"
            | "embeddings"
            | "embed-content"
            | "predict"
            | "contextualized-embeddings"
            | "invoke-model"
            | "start-async-invoke"
            | "invoke-model-with-bidirectional-stream"
            | "fim"
            | "moderations"
            | "ocr"
    )
}
