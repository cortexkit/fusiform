//! Normalize the published models.dev v2 provider map without a v1 fallback.

mod inventory;
mod rates;
mod raw;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::{self, DeserializeOwned, MapAccess, Visitor};
use serde::Deserializer;
use serde_json::{json, value::RawValue, Value};

use super::{
    absent_if_zero, wire_claim, Capabilities, ChargeUnitEvidence, Limits, NormalizeError,
    NormalizedCatalog, NormalizedModel, NormalizedProvider, Quarantine, RefusedFactScope,
    WireFacts,
};
use crate::upstream_facts::FactKey;
use crate::{Modality, ModelKey, SourceId, Timestamp};
use inventory::Kind;
use raw::{ModelObject, Object, ProviderObject};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FindingKey {
    UnknownKey { object_kind: String, path: String },
    UnknownModelType { value: String },
    UnknownApiProtocol { value: String },
    UnknownCostUnit { role: String, value: String },
    UnservedCost { role: String, unit: String },
    UnattributableChargeUnit { role: String },
    RefusedPrice { role: String, detail: String },
    RefusedModelMember { path: String },
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
            Self::RefusedModelMember { .. } => "refused_model_member",
        }
    }

    fn unknown_key(kind: Kind, path: &str, name: &str) -> Self {
        Self::UnknownKey {
            object_kind: format!("{kind:?}"),
            path: format!("{path}.{name}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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
    pub next_date_boundary_ms: Option<i64>,
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
    normalize_models_dev_at(bytes, evidence, Timestamp(0))
}

pub(super) fn normalize_models_dev_at(
    bytes: &[u8],
    evidence: &ChargeUnitEvidence,
    observed_at: Timestamp,
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
        .deserialize_map(ProviderMap {
            evidence,
            observed_at,
        })
        .map_err(document_parse)?;
    decoder.end().map_err(document_parse)?;
    Ok(outcome)
}

struct ProviderMap<'a> {
    evidence: &'a ChargeUnitEvidence,
    observed_at: Timestamp,
}

struct DateBoundaries {
    observed_at: Timestamp,
    next_ms: Option<i64>,
}

impl DateBoundaries {
    fn is_active(&mut self, boundary_ms: i64) -> bool {
        if self.observed_at.0 >= boundary_ms {
            return true;
        }
        self.next_ms = Some(
            self.next_ms
                .map_or(boundary_ms, |next| next.min(boundary_ms)),
        );
        false
    }
}

struct Context<'a> {
    evidence: &'a ChargeUnitEvidence,
    findings: Findings,
    dates: DateBoundaries,
}

impl<'de> Visitor<'de> for ProviderMap<'_> {
    type Value = NormalizeV2Outcome;

    fn expecting(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.write_str("a v2 provider map")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        use de::Error;

        let mut providers = BTreeMap::new();
        let mut ctx = Context {
            evidence: self.evidence,
            findings: Findings::new(),
            dates: DateBoundaries {
                observed_at: self.observed_at,
                next_ms: None,
            },
        };
        let mut model_count = 0;
        // Borrow each provider's JSON directly. Keeping its raw fields out of
        // the rest of the loop bounds temporary allocations to one provider.
        while let Some((id, raw)) = map.next_entry::<String, &RawValue>()? {
            let provider = ProviderObject::parse(raw)
                .map_err(|error| M::Error::custom(format_args!("provider {id:?}: {error}")))?;
            let provider = normalize_provider(&id, provider, &mut ctx).map_err(M::Error::custom)?;
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
            next_date_boundary_ms: ctx.dates.next_ms,
            findings: ctx
                .findings
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
    provider: ProviderObject<'_>,
    ctx: &mut Context<'_>,
) -> Result<NormalizedProvider, String> {
    let name = provider.fields.raw("name").map_or(Ok(None), |raw| {
        serde_json::from_str::<String>(raw.get()).map(Some)
    });
    Kind::Provider.inventory(
        "provider",
        &provider.fields,
        provider.models.first().map(|(model, _)| model.as_str()),
        &mut ctx.findings,
    );
    let mut models = BTreeMap::new();
    for (model_id, raw) in provider.models {
        let raw =
            ModelObject::parse(raw).map_err(|error| format!("model {id}/{model_id}: {error}"))?;
        let name = name
            .as_ref()
            .map(|name| name.as_deref().filter(|name| !name.is_empty()));
        let model = normalize_model(id, &model_id, name.map_err(|_| ()), raw, ctx);
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

struct ModelFactRead<'a> {
    model: &'a str,
    findings: &'a mut Findings,
    refused: BTreeSet<RefusedFactScope>,
}

impl ModelFactRead<'_> {
    fn refused(&mut self, path: &str, facts: &[&str]) {
        self.findings.add(
            FindingKey::RefusedModelMember {
                path: path.to_owned(),
            },
            Some(self.model),
        );
        self.refused.extend(
            facts
                .iter()
                .map(|key| RefusedFactScope::Fact(FactKey::from_stored((*key).to_owned()))),
        );
    }

    fn decode<T, E>(&mut self, result: Result<T, E>, path: &str, facts: &[&str]) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(_) => {
                self.refused(path, facts);
                None
            }
        }
    }

    fn value<T: DeserializeOwned>(
        &mut self,
        raw: Option<&RawValue>,
        path: &str,
        facts: &[&str],
    ) -> Option<T> {
        self.decode(serde_json::from_str::<T>(raw?.get()), path, facts)
    }

    fn object<'raw>(
        &mut self,
        raw: Option<&'raw RawValue>,
        path: &str,
        facts: &[&str],
    ) -> Option<Object<'raw>> {
        self.decode(Object::parse(raw?), path, facts)
    }
}

const WIRE_FACTS: &[&str] = &["wire.api", "wire.npm", "wire.shape"];
const MODALITY_FACTS: &[&str] = &[
    "capability.input_modalities",
    "capability.output_modalities",
    "capability.attachment",
];
const CAPABILITY_FACTS: &[&str] = &[
    "capability.reasoning",
    "capability.reasoning_options",
    "capability.tool_call",
    "capability.structured_output",
    "capability.temperature",
];

fn raw_field<'raw>(object: Option<&Object<'raw>>, key: &str) -> Option<&'raw RawValue> {
    object?.raw(key)
}

fn normalize_model(
    provider: &str,
    model: &str,
    provider_name: Result<Option<&str>, ()>,
    raw: ModelObject<'_>,
    ctx: &mut Context<'_>,
) -> NormalizedModel {
    let ModelObject { fields } = raw;
    Kind::Model.inventory("model", &fields, Some(model), &mut ctx.findings);
    let mut read = ModelFactRead {
        model,
        findings: &mut ctx.findings,
        refused: BTreeSet::new(),
    };
    let model_type = read.decode(
        serde_json::from_str::<String>(fields.raw("type").expect("required v2 member").get()),
        "type",
        &["model.type"],
    );
    if let Some(value) = model_type.as_ref().filter(|value| {
        !matches!(
            value.as_str(),
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
        )
    }) {
        read.findings.add(
            FindingKey::UnknownModelType {
                value: value.clone(),
            },
            Some(model),
        );
    }
    let capabilities = normalize_capabilities(&fields, &mut read);
    let limit = read.object(
        fields.raw("limit"),
        "limit",
        &["limit.context", "limit.output"],
    );
    let api = read.object(fields.raw("api"), "api", WIRE_FACTS);
    let experimental_raw = fields.raw("experimental");
    let experimental = read.object(experimental_raw, "experimental", &[]);
    let modes_raw = raw_field(experimental.as_ref(), "modes");
    let modes = read.object(modes_raw, "experimental.modes", &[]);
    let unreadable_modes = (experimental_raw.is_some() && experimental.is_none())
        || (modes_raw.is_some() && modes.is_none());
    let mut row = NormalizedModel {
        key: ModelKey {
            source: SourceId::ModelsDev,
            provider_id: provider.to_owned(),
            model_id: model.to_owned(),
        },
        provider_name: read
            .decode(provider_name, "provider.name", &["provider.name"])
            .flatten()
            .map(str::to_owned),
        display_name: read
            .value::<String>(fields.raw("name"), "name", &["model.name"])
            .filter(|name| !name.is_empty()),
        family: read.value(fields.raw("family"), "family", &["model.family"]),
        canonical_id: read.value(
            fields.raw("canonical_id"),
            "canonical_id",
            &["model.canonical_id"],
        ),
        status: read.value(fields.raw("status"), "status", &["model.status"]),
        model_type,
        release_date: fields.read("release_date"),
        last_updated: fields.read("last_updated"),
        knowledge_cutoff: fields.read("knowledge"),
        open_weights: read.value(
            fields.raw("open_weights"),
            "open_weights",
            &["model.open_weights"],
        ),
        capabilities,
        limits: Limits {
            context_tokens: absent_if_zero(read.value(
                raw_field(limit.as_ref(), "context"),
                "limit.context",
                &["limit.context"],
            )),
            output_tokens: absent_if_zero(read.value(
                raw_field(limit.as_ref(), "output"),
                "limit.output",
                &["limit.output"],
            )),
        },
        wire: api
            .as_ref()
            .map(|api| normalize_api(api, &mut read))
            .unwrap_or_default(),
        rates: Vec::new(),
        refused: read.refused,
        quarantined: Quarantine {
            provider_override: false,
            experimental_modes: modes
                .iter()
                .flat_map(|mode| &mode.0)
                .map(|(name, _)| name.clone())
                .collect(),
        },
    };
    if unreadable_modes {
        row.refused.insert(RefusedFactScope::ModeRates(None));
    }
    row.rates = rates::normalize_rates(
        &row.key,
        fields.raw("cost"),
        modes.as_ref(),
        ctx,
        &mut row.refused,
    );
    row
}

fn normalize_capabilities(fields: &Object<'_>, read: &mut ModelFactRead<'_>) -> Capabilities {
    let modalities = read.object(fields.raw("modalities"), "modalities", MODALITY_FACTS);
    let input_modalities = read_modalities(read.value(
        raw_field(modalities.as_ref(), "input"),
        "modalities.input",
        &["capability.input_modalities", "capability.attachment"],
    ));
    let output_modalities = read_modalities(read.value(
        raw_field(modalities.as_ref(), "output"),
        "modalities.output",
        &["capability.output_modalities"],
    ));
    let attachment = input_modalities
        .as_ref()
        .map(|items| items.iter().any(|value| *value != Modality::Text));
    let capabilities = read.object(fields.raw("capabilities"), "capabilities", CAPABILITY_FACTS);
    let capabilities = capabilities.as_ref();
    let reasoning_facts = &["capability.reasoning", "capability.reasoning_options"];
    let reasoning = read.object(
        raw_field(capabilities, "reasoning"),
        "capabilities.reasoning",
        reasoning_facts,
    );
    let supported = read.value(
        raw_field(reasoning.as_ref(), "supported"),
        "capabilities.reasoning.supported",
        reasoning_facts,
    );
    let tools = read.object(
        raw_field(capabilities, "tools"),
        "capabilities.tools",
        &["capability.tool_call"],
    );
    Capabilities {
        input_modalities,
        output_modalities,
        attachment,
        reasoning: supported,
        reasoning_options: reasoning
            .as_ref()
            .filter(|_| supported == Some(true))
            .map(|options| reasoning_options(options, read)),
        tool_call: read.value(
            raw_field(tools.as_ref(), "supported"),
            "capabilities.tools.supported",
            &["capability.tool_call"],
        ),
        structured_output: read.value(
            raw_field(capabilities, "structured_output"),
            "capabilities.structured_output",
            &["capability.structured_output"],
        ),
        temperature: read.value(
            raw_field(capabilities, "temperature"),
            "capabilities.temperature",
            &["capability.temperature"],
        ),
    }
}

fn read_modalities(published: Option<Vec<String>>) -> Option<Vec<Modality>> {
    let mut base = Vec::new();
    for value in published? {
        let modality = Modality::parse(modality_label(&value));
        if !base.contains(&modality) {
            base.push(modality);
        }
    }
    Some(base)
}

/// The catalog modality for an upstream v2 output or input label.
pub fn modality_label(value: &str) -> &str {
    // MIME subtypes describe the same catalog modality. Output classes that
    // v1 grouped with text are still distinguished by the model's v2 type.
    match value {
        "embedding" | "decision" | "reranking" => "text",
        "application/pdf" => "pdf",
        other => match other.split_once('/') {
            Some((kind @ ("text" | "image" | "audio" | "video"), _)) => kind,
            _ => other,
        },
    }
}

fn reasoning_options(reasoning: &Object<'_>, read: &mut ModelFactRead<'_>) -> Value {
    let facts = &["capability.reasoning_options"];
    let mut options = Vec::new();
    if read.value(
        reasoning.raw("toggle"),
        "capabilities.reasoning.toggle",
        facts,
    ) == Some(true)
    {
        options.push(json!({"type": "toggle"}));
    }
    if let Some(effort) = read.value::<Vec<String>>(
        reasoning.raw("effort"),
        "capabilities.reasoning.effort",
        facts,
    ) {
        options.push(json!({"type": "effort", "values": effort}));
    }
    if let Some(budget) = read.object(
        reasoning.raw("budget"),
        "capabilities.reasoning.budget",
        facts,
    ) {
        let mut option = json!({"type": "budget_tokens"});
        for (key, path) in [
            ("min", "capabilities.reasoning.budget.min"),
            ("max", "capabilities.reasoning.budget.max"),
        ] {
            if let Some(value) = read.value::<Value>(budget.raw(key), path, facts) {
                if value.is_u64() || (key == "min" && value.as_i64() == Some(-1)) {
                    // Some providers publish -1 as a reasoning-budget sentinel.
                    option[key] = value;
                } else {
                    read.refused(path, facts);
                }
            }
        }
        options.push(option);
    }
    Value::Array(options)
}

fn normalize_api(api: &Object<'_>, read: &mut ModelFactRead<'_>) -> WireFacts {
    let mut wire = WireFacts::default();
    for (index, (protocol, raw)) in api.0.iter().enumerate() {
        if !known_api_protocol(protocol) {
            read.findings.add(
                FindingKey::UnknownApiProtocol {
                    value: protocol.clone(),
                },
                Some(read.model),
            );
            if index == 0 {
                // Do not fall back to the second API: it need not accept the
                // same requests or represent the endpoint previously served.
                read.refused.extend(
                    ["api", "npm", "shape"].map(|name| RefusedFactScope::Fact(FactKey::wire(name))),
                );
            }
        } else if index == 0 {
            if let Some(entry) = read.object(Some(raw), &format!("api.{protocol}"), WIRE_FACTS) {
                let sdk_facts: &[&str] = if matches!(
                    protocol.as_str(),
                    "chat-completions" | "responses" | "speech" | "videos"
                ) {
                    &["wire.npm", "wire.shape"]
                } else {
                    &["wire.npm"]
                };
                let sdk = read.object(entry.raw("sdk"), &format!("api.{protocol}.sdk"), sdk_facts);
                let npm: Option<String> = read.value(
                    raw_field(sdk.as_ref(), "aisdk"),
                    &format!("api.{protocol}.sdk.aisdk"),
                    sdk_facts,
                );
                let base_facts: &[&str] = if protocol == "videos" {
                    &["wire.api", "wire.shape"]
                } else {
                    &["wire.api"]
                };
                let base_url: Option<String> = read.value(
                    entry.raw("base_url"),
                    &format!("api.{protocol}.base_url"),
                    base_facts,
                );
                let defaults_to_responses = matches!(
                    npm.as_deref(),
                    Some("@ai-sdk/openai" | "@ai-sdk/azure" | "@ai-sdk/xai")
                );
                let shape = match (protocol.as_str(), defaults_to_responses) {
                    ("images", _) => Some("images"),
                    ("interactions", _) => Some("interactions"),
                    ("speech", _) if npm.as_deref() == Some("@ai-sdk/elevenlabs") => Some("speech"),
                    ("videos", _)
                        if api_has_matching_interactions(
                            base_url.as_deref(),
                            npm.as_deref(),
                            api,
                            read,
                        ) =>
                    {
                        Some("interactions")
                    }
                    ("chat-completions", true) => Some("completions"),
                    ("responses", false) => Some("responses"),
                    _ => None,
                };
                wire = WireFacts {
                    api: wire_claim(base_url.as_deref(), None),
                    npm: wire_claim(npm.as_deref(), None),
                    shape: wire_claim(shape, None),
                };
            }
        }
    }
    wire
}

fn api_has_matching_interactions(
    url: Option<&str>,
    sdk: Option<&str>,
    apis: &Object<'_>,
    read: &mut ModelFactRead<'_>,
) -> bool {
    let (Some(url), Some(sdk)) = (url, sdk) else {
        return false;
    };
    let facts = &["wire.shape"];
    let Some(entry) = read.object(apis.raw("interactions"), "api.interactions", facts) else {
        return false;
    };
    let endpoint: Option<String> =
        read.value(entry.raw("base_url"), "api.interactions.base_url", facts);
    let clients = read.object(entry.raw("sdk"), "api.interactions.sdk", facts);
    let aisdk: Option<String> = read.value(
        raw_field(clients.as_ref(), "aisdk"),
        "api.interactions.sdk.aisdk",
        facts,
    );
    endpoint.as_deref() == Some(url) && aisdk.as_deref() == Some(sdk)
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
