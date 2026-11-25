//! Facts as written by upstream ingest, before serving adds fusiform annotations.

use crate::{normalize::NormalizedModel, RateValue};

/// What an era is about, in the served contract's vocabulary.
///
/// A newtype rather than a bare string so a caller cannot invent a key at a
/// call site: the vocabulary is closed at the type level even though the column
/// is text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FactKey(String);

impl FactKey {
    /// The fact a [`crate::FieldId`] names.
    ///
    /// A correction declares WHICH fields were wrong and is written on a
    /// specific fact's row. Those are two statements of one belief, and without
    /// a mapping between them nothing stops a row that corrects `rate.input`
    /// from claiming it was the context limit that was wrong. Each claim is
    /// individually valid, so only their agreement can rule that out.
    ///
    /// Exhaustive by construction: adding a `FieldId` variant without a fact
    /// key here fails to compile.
    pub fn for_field(field: crate::FieldId) -> Option<Self> {
        use crate::{CapabilityId, FieldId, LimitId};
        Some(match field {
            FieldId::Rate { class } => Self::rate(class),
            // A name that could not have been served cannot name a served
            // fact, so it maps to nothing rather than to an odd key.
            FieldId::ModeRate { class, mode } => {
                if !crate::normalize::is_servable_mode_name(&mode) {
                    return None;
                }
                Self::rate_in_mode(class, &mode)
            }
            FieldId::Existence => Self::existence(),
            FieldId::ProviderName => Self::provider("name"),
            FieldId::Limit { limit } => Self::limit(match limit {
                LimitId::Context => "context",
                LimitId::Output => "output",
            }),
            FieldId::Capability { capability } => Self::capability(match capability {
                CapabilityId::Reasoning => "reasoning",
                CapabilityId::ReasoningOptions => "reasoning_options",
                CapabilityId::StructuredOutput => "structured_output",
                CapabilityId::Temperature => "temperature",
                CapabilityId::ToolCall => "tool_call",
                CapabilityId::Attachment => "attachment",
                CapabilityId::InputModalities => "input_modalities",
                CapabilityId::OutputModalities => "output_modalities",
            }),
            FieldId::Model { attribute } => FactKey::model(match attribute {
                crate::ModelAttributeId::Name => "name",
                crate::ModelAttributeId::Family => "family",
                crate::ModelAttributeId::OpenWeights => "open_weights",
                crate::ModelAttributeId::CanonicalId => "canonical_id",
                crate::ModelAttributeId::Status => "status",
                crate::ModelAttributeId::Type => "type",
            }),
            FieldId::Wire { attribute } => FactKey::wire(match attribute {
                crate::WireAttributeId::Npm => "npm",
                crate::WireAttributeId::Api => "api",
                crate::WireAttributeId::Shape => "shape",
            }),
            // A tiered rate's fact key carries a threshold that the FieldId
            // does not know, so these two cannot be mapped to a single key.
            // `None` rather than a guess: the alternative is returning the
            // untiered key, which would make a correction about a tier read as
            // a correction about the base rate.
            FieldId::TierThreshold | FieldId::TierRate | FieldId::ChargeUnit => return None,
        })
    }
}

/// The namespace prefixes fact keys are grouped under.
///
/// Named constants rather than string literals at each construction site,
/// because a consumer selecting a fact family does it by prefix — and a prefix that
/// no longer matches any key returns an EMPTY result rather than an error. That
/// is the failure shape this repository keeps finding: a wrong answer that
/// reads as a legitimate one. A caller filtering on `"rate."` after the
/// namespace moved gets zero rates and no indication anything is wrong.
pub mod prefix {
    /// Pricing facts, including tiered rates like `rate.input.above_context.200000`
    /// and mode rates like `rate.input.mode.fast`.
    pub const RATE: &str = "rate.";
    /// Declared capabilities.
    pub const CAPABILITY: &str = "capability.";
    /// Declared limits.
    pub const LIMIT: &str = "limit.";
    /// What the upstream says the model IS, independent of who serves it.
    ///
    /// Distinct from `capability.` on purpose: a capability is something the
    /// model can do, and these are facts about the artefact — which weights
    /// family it belongs to, whether those weights are published. Two providers
    /// serving the same weights agree on these and disagree on rates, which is
    /// exactly what makes them useful for relating rows across providers.
    pub const MODEL: &str = "model.";
    /// Claims about the provider offering the row, not the model's creator.
    pub const PROVIDER: &str = "provider.";
    /// Published connection claims, not a transport implementation.
    pub const WIRE: &str = "wire.";
}

/// The infixes that make a rate key conditional, and the one question every
/// caller asks of them.
///
/// Named here rather than spelled at each site, for the reason [`prefix`]
/// gives: a filter written against a misspelled infix matches nothing and
/// reads as "this model has no such rates".
pub mod rate_key {
    /// A rate that applies above a context threshold:
    /// `rate.<class>.above_context.<tokens>`.
    pub const ABOVE_CONTEXT: &str = ".above_context.";
    /// A rate that applies in the upstream's named mode:
    /// `rate.<class>.mode.<name>`.
    pub const MODE: &str = ".mode.";

    /// Whether a fact key is a mode rate.
    pub fn is_mode_rate(key: &str) -> bool {
        key.starts_with(super::prefix::RATE) && key.contains(MODE)
    }
}

impl FactKey {
    pub fn provider(name: &str) -> Self {
        Self(format!("{}{name}", prefix::PROVIDER))
    }

    pub fn wire(name: &str) -> Self {
        Self(format!("{}{name}", prefix::WIRE))
    }

    /// The key for a rate, per token class — never one key for "rates".
    pub fn rate(class: crate::TokenClass) -> Self {
        Self(format!("{}{}", prefix::RATE, token_class_str(class)))
    }

    /// The key for a rate that applies above a context threshold.
    ///
    /// The threshold is part of the KEY rather than part of the value, so a
    /// provider adding a tier opens an era on the new tier alone instead of
    /// rewriting the base rate's history. It also means a point-in-time read
    /// asks for exactly the rate it needs — no consumer has to fetch every rate
    /// for a model and filter.
    pub fn rate_above_context(class: crate::TokenClass, tokens: u64) -> Self {
        Self(format!(
            "{}{}{}{tokens}",
            prefix::RATE,
            token_class_str(class),
            rate_key::ABOVE_CONTEXT
        ))
    }

    /// The key for a rate that applies only in the upstream's named mode.
    ///
    /// The mode name is part of the KEY, as the threshold is for a tier, so a
    /// provider repricing one mode opens an era on that mode's key alone and a
    /// consumer pricing a mode asks for exactly the rate it needs. The name is
    /// the upstream's label, verbatim; the normalizer admits only names that
    /// are safe inside a dotted key.
    pub fn rate_in_mode(class: crate::TokenClass, mode: &str) -> Self {
        Self(format!(
            "{}{}{}{mode}",
            prefix::RATE,
            token_class_str(class),
            rate_key::MODE
        ))
    }

    /// Rebuild a key from its stored text.
    ///
    /// Stored rows are text. An unrecognised key can indicate a hand-edited
    /// database or a value written by a newer version. Preserving it verbatim keeps that visible instead of turning a
    /// hand-edit into a parse error on an unrelated read.
    pub fn from_stored(raw: String) -> Self {
        Self(raw)
    }

    /// The key for a capability field.
    pub fn capability(name: &str) -> Self {
        Self(format!("{}{name}", prefix::CAPABILITY))
    }

    /// The key for a declared limit.
    pub fn limit(name: &str) -> Self {
        Self(format!("{}{name}", prefix::LIMIT))
    }

    /// A fact about the model artefact rather than about this provider's
    /// offering of it.
    pub fn model(name: &str) -> Self {
        Self(format!("{}{name}", prefix::MODEL))
    }

    /// The key for whether the model exists in the source at all. A retirement
    /// is a tombstone value under this key, never a deleted row.
    pub fn existence() -> Self {
        Self("existence".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn token_class_str(class: crate::TokenClass) -> &'static str {
    use crate::TokenClass::*;
    match class {
        Input => "input",
        Output => "output",
        CacheRead => "cache_read",
        CacheWrite => "cache_write",
        Reasoning => "reasoning",
        InputAudio => "input_audio",
        OutputAudio => "output_audio",
    }
}

/// Whether a model is present in the source.
///
/// A closed two-state value under the existence key, serialized into the era's
/// value column like any other fact. Presence is history, not a row's absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existence {
    Present,
    /// The source stopped describing this model. Not "deleted" and not
    /// "unavailable" — those are claims about the provider, and fusiform only
    /// observed that a row stopped appearing.
    Absent,
}

impl Existence {
    pub const fn as_json(self) -> &'static str {
        match self {
            Existence::Present => "\"present\"",
            Existence::Absent => "\"absent\"",
        }
    }
}

/// The exact fact keys and JSON values a normalized model contributes to an observation.
/// Presence is tracked separately because a store records withdrawals as tombstones.
pub fn facts_of(model: &NormalizedModel) -> ModelFacts {
    let mut facts = vec![
        // What the model IS, as the upstream states it, independent of who
        // serves it. Stored rather than derived because these are upstream's
        // own claims — the same test every other fact here passes.
        //
        // They exist as facts so a RELATION between providers can be computed
        // at serve time: `family` is published identically by every provider
        // serving the same weights, and `open_weights` marks the population
        // where one provider's list price says something about another's row.
        // The relation itself is fusiform's derivation and stays out of
        // storage; these two are not.
        (
            FactKey::model("family"),
            json_opt_str(model.family.as_deref()),
        ),
        (
            FactKey::model("open_weights"),
            json_opt_bool(model.open_weights),
        ),
        (
            FactKey::model("canonical_id"),
            json_opt_str(model.canonical_id.as_deref()),
        ),
        (
            FactKey::model("status"),
            json_opt_str(model.status.as_deref()),
        ),
        // Limits. `null` is a real value here: a limit that stops being
        // published is a change, and encoding it as absence would make the era
        // disappear rather than record that the upstream stopped saying.
        (
            FactKey::limit("context"),
            json_opt_u64(model.limits.context_tokens),
        ),
        (
            FactKey::limit("output"),
            json_opt_u64(model.limits.output_tokens),
        ),
        // Capability facts. An absent structured_output is not a false claim.
        (
            FactKey::capability("reasoning"),
            json_opt_bool(model.capabilities.reasoning),
        ),
        // The reasoning settings the model accepts, stored as the upstream
        // published them. Unlike the modality lists below this is NOT sorted:
        // effort levels are listed in order and a consumer relies on that
        // order, so a reordering upstream is a real change and must open an
        // era. `null` when the key was absent, `[]` when the upstream stated
        // the model takes no options.
        (
            FactKey::capability("reasoning_options"),
            json_opt_value(model.capabilities.reasoning_options.as_ref()),
        ),
        (
            FactKey::capability("tool_call"),
            json_opt_bool(model.capabilities.tool_call),
        ),
        (
            FactKey::capability("attachment"),
            json_opt_bool(model.capabilities.attachment),
        ),
        (
            FactKey::capability("input_modalities"),
            json_modalities(&model.capabilities.input_modalities),
        ),
        (
            FactKey::capability("output_modalities"),
            json_modalities(&model.capabilities.output_modalities),
        ),
    ];

    // Missing or empty labels make no display claim. A later withdrawal is
    // stored as a null era, so point-in-time reads still return prior names.
    for (key, name) in [
        (FactKey::model("name"), model.display_name.as_deref()),
        (FactKey::provider("name"), model.provider_name.as_deref()),
    ] {
        if let Some(name) = name {
            facts.push((key, json_opt_str(Some(name))));
        }
    }

    // The upstream's sparse label makes no claim when absent, not even chat.
    if let Some(model_type) = model.model_type.as_deref() {
        facts.push((FactKey::model("type"), json_opt_str(Some(model_type))));
    }

    // Whether models.dev says the model supports structured output.
    // No fact is written when the source omits the key; silence is unknown.
    if let Some(supported) = model.capabilities.structured_output {
        facts.push((
            FactKey::capability("structured_output"),
            json_opt_bool(Some(supported)),
        ));
    }

    // An unpublished acceptance flag is unknown, not a negative claim.
    if let Some(supported) = model.capabilities.temperature {
        facts.push((
            FactKey::capability("temperature"),
            json_opt_bool(Some(supported)),
        ));
    }

    // Total destructuring keeps a new domain connection field from silently
    // disappearing at the storage boundary. Unpublished values create no fact.
    let crate::normalize::WireFacts { npm, api, shape } = &model.wire;
    for (name, claim) in [("npm", npm), ("api", api), ("shape", shape)] {
        if let Some(claim) = claim {
            facts.push((
                FactKey::wire(name),
                serde_json::to_string(claim).expect("a wire claim is JSON strings"),
            ));
        }
    }

    // Rates, keyed per token class and condition. A rate row's key carries
    // every discriminator needed to select it, so a consumer never joins
    // capability rows to price rows.
    for rate in &model.rates {
        if let crate::ChargeBasis::PerMillionTokens { class } = rate.basis {
            let key = match rate.condition {
                crate::RateCondition::Always => FactKey::rate(class),
                crate::RateCondition::MinContextTokens { tokens } => {
                    FactKey::rate_above_context(class, tokens)
                }
                // A mode's price, under the upstream's mode name. Only the
                // rate schedule reaches here; the request bytes that select
                // the mode were dropped when the document was parsed.
                crate::RateCondition::Mode { ref name } => FactKey::rate_in_mode(class, name),
            };
            facts.push((key, json_rate(&rate.value)));
        }
    }

    if !model.refused.is_empty() {
        facts.retain(|(key, _)| !model.refused.iter().any(|scope| scope.covers(key)));
    }
    facts.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
    if !facts.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return ModelFacts {
            values: facts,
            conflicts: Vec::new(),
        };
    }

    // Repeated context tiers can describe the same rate. Equal prices are one
    // claim; different prices leave no price we can attribute to the source.
    let mut values = Vec::with_capacity(facts.len());
    let mut conflicts = Vec::new();
    for group in facts.chunk_by(|left, right| left.0 == right.0) {
        let (fact, value) = &group[0];
        if group.iter().all(|(_, candidate)| candidate == value) {
            values.push((fact.clone(), value.clone()));
        } else {
            let mut values: Vec<_> = group.iter().map(|(_, value)| value.clone()).collect();
            values.sort_unstable();
            values.dedup();
            conflicts.push(FactConflict {
                provider: model.key.provider_id.clone(),
                model: model.key.model_id.clone(),
                fact: fact.clone(),
                values,
            });
        }
    }
    ModelFacts { values, conflicts }
}

#[derive(Debug)]
pub struct ModelFacts {
    /// Unique upstream facts, ordered by fact key.
    pub values: Vec<(FactKey, String)>,
    pub conflicts: Vec<FactConflict>,
}

/// An omitted fact and its distinct conflicting values in storage JSON form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactConflict {
    pub provider: String,
    pub model: String,
    pub fact: FactKey,
    pub values: Vec<String>,
}

fn json_opt_u64(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

/// A string fact, or `null` when the upstream published none.
///
/// Serialized through serde rather than quoted by hand: a family name is
/// upstream text and could contain a quote or a backslash, and a hand-built
/// literal would produce a value that fails to parse on the way out. The
/// numeric helpers beside this one are safe to format directly; text is not.
fn json_opt_str(v: Option<&str>) -> String {
    match v {
        Some(s) => serde_json::Value::String(s.to_string()).to_string(),
        None => "null".to_string(),
    }
}

/// An upstream JSON value stored as-is, or `null` when the upstream published
/// none. Serialized through serde, so array order and every element (including
/// `null`s) survive exactly as parsed.
fn json_opt_value(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(value) => value.to_string(),
        None => "null".to_string(),
    }
}

fn json_opt_bool(v: Option<bool>) -> String {
    match v {
        Some(b) => b.to_string(),
        None => "null".to_string(),
    }
}

fn json_modalities(mods: &Option<Vec<crate::Modality>>) -> String {
    // `null` when the upstream published no modality block, distinct from `[]`
    // when it published an empty one. The same absent-vs-zero split the rest of
    // this file uses: an empty list claims the model accepts nothing, which is
    // a statement about the model rather than about the document.
    let Some(mods) = mods else {
        return "null".to_string();
    };

    // Sorted so a reordering upstream does not read as a capability change. The
    // upstream's array order is not information — it carries no ranking — and
    // treating it as information would manufacture change events.
    let mut names: Vec<String> = mods.iter().map(modality_name).collect();
    names.sort();
    serde_json::to_string(&names).expect("a list of strings always serializes")
}

fn modality_name(m: &crate::Modality) -> String {
    use crate::Modality::*;
    match m {
        Text => "text".to_string(),
        Image => "image".to_string(),
        Audio => "audio".to_string(),
        Video => "video".to_string(),
        Pdf => "pdf".to_string(),
        Other(s) => s.clone(),
    }
}

/// Render a rate for storage, keeping the three value states distinct.
///
/// A priced amount, a stated zero, and an unpriced reason must render
/// differently, because the whole point of the three-state design is that a
/// consumer can tell them apart. Rendering a stated zero as `0` would make it
/// indistinguishable from a priced zero, which the money boundary refuses to
/// produce precisely so this distinction survives.
///
/// # The currency carries HOW IT WAS ESTABLISHED, not just what it is
///
/// `models.dev` states no currency anywhere. Fusiform serves USD by a named
/// policy, and `UnitProvenance` exists so that "why does the catalog say USD"
/// resolves to an auditable rule rather than a habit — its own doc says
/// serving USD without saying why would launder a convention into a fact.
///
/// This function used to do exactly that. It rendered `"currency":"USD"` and
/// dropped the provenance, so the domain distinguished stated-by-source from
/// assumed-by-policy, `fusiform-core` tested the distinction, and it died one
/// layer down: a consumer received a currency indistinguishable from one the
/// upstream published.
///
/// That matters at a seam more than inside a process. A producer's inference
/// becomes a consumer's fact unless the marker crosses with the value, and the
/// consumer has no way to discover the inference was made — they price against
/// it and are correct until a non-USD provider arrives, at which point every
/// historical row is silently ambiguous about whether USD was read or assumed.
pub fn json_rate(value: &RateValue) -> String {
    match value {
        // NO `unit_provenance` HERE, and its absence is the fix rather than an
        // omission.
        //
        // Storage holds what the UPSTREAM said. Provenance is fusiform's own
        // annotation — models.dev publishes no currency at all, so "USD by
        // policy models-dev-usd-v1" is a statement about this code — and the
        // serve path already attaches it to every priced rate that lacks it.
        //
        // It was written here too for one day. That second path bought nothing
        // and cost 17,455 false eras: the diff compared serialized strings, so
        // every stored rate differed from every newly normalized one and the
        // first poll after placement recorded a price change for every priced
        // model. A second path to the same outcome is not redundancy, it is a
        // second thing that can be wrong, and this one failed in a way the
        // serve path could not observe.
        // `floor` is deliberately NOT stored, by the same rule that keeps
        // `unit_provenance` out of this string.
        //
        // No source publishes minimum-charge data, so the floor is `Unknown` on
        // every row — which is fusiform stating what nobody has established,
        // not a claim the upstream made. Storage holds the upstream's claim.
        //
        // Writing a constant field into every stored rate is also precisely the
        // reserialization described above: it would differ from every existing
        // row and record a price change for every priced model on the first
        // poll after placement. The dimension stays serve-side until a SOURCE
        // publishes a floor, at which point it is an upstream claim and belongs
        // here.
        // `inherited_from` is dropped here for the same reason as `floor`, and
        // the reason is stronger rather than merely analogous.
        //
        // A floor is a dimension nobody has established. An inherited marker is
        // a claim about a DIFFERENT PROVIDER'S ROW — zai's price attached to a
        // reseller's model — and storage records what THIS source said about
        // THIS row. Writing it would put one provider's price into another
        // provider's history, where a point-in-time read would later serve it
        // back as something the upstream published.
        //
        // The destructuring is what forced this decision rather than letting
        // the field arrive by default: `RateValue` gaining a variant field
        // fails to compile here until someone says whether it is an upstream
        // claim. That fence exists because a serve-time annotation reached
        // storage once and the ingest diff read every priced row as changed,
        // writing 17,455 false price eras in a single poll.
        RateValue::Priced {
            amount,
            floor: _,
            // Unit-billed claims belong in curated model history, not in
            // models.dev's upstream observation history.
            per: _,
            increment: _,
            minimum_quantity: _,
            inherited_from: _,
        } => format!(
            r#"{{"state":"priced","units":{},"exponent":{},"currency":"{}"}}"#,
            amount.units,
            amount.exponent,
            amount.currency.as_str()
        ),
        // The marker is dropped on every state, for the reason given above.
        RateValue::StatedZero { inherited_from: _ } => r#"{"state":"stated_zero"}"#.to_string(),
        RateValue::Unpriced {
            reason,
            inherited_from: _,
        } => format!(
            r#"{{"state":"unpriced","reason":"{}"}}"#,
            unpriced_reason_str(*reason)
        ),
        RateValue::BilledAs { .. } => unreachable!(
            "billed_as is serve-only: normalization never constructs billing rules, \
             and plan_ingest receives only normalized values"
        ),
    }
}

fn unpriced_reason_str(r: crate::UnpricedReason) -> &'static str {
    use crate::UnpricedReason::*;
    match r {
        MissingRate => "missing_rate",
        NoCatalogCoverage => "no_catalog_coverage",
        UnknownChargeBasis => "unknown_charge_basis",
        NotEstablished => unreachable!(
            "not_established is serve-only: normalization never constructs billing rules, \
             and plan_ingest receives only normalized values"
        ),
        NoApiCard => unreachable!(
            "no_api_card is serve-only: normalization never constructs quota proxies, \
             and plan_ingest receives only normalized values"
        ),
    }
}
