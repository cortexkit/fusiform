#![forbid(unsafe_code)]

//! Fusiform's served wire schema.
//!
//! These are the types a consumer compiles against to call `catalog.get`,
//! `catalog.history` and `catalog.status`. They are the contract; everything
//! else in fusiform is implementation.
//!
//! # Why this crate exists, and why it holds nothing else
//!
//! The fleet's cross-repo payload rule is that one definition is consumed by
//! both sides of a wire. SUBC ruled that a module-owned published crate is the
//! house pattern for that — `subc-protocol` is the precedent, owned by the
//! daemon's repo and compiled against by every client — because a served schema
//! is a DECLARATION authored by the producer. Producer ownership makes drift
//! unauthorable: the schema and the crate move in one commit.
//!
//! It depends on serde and nothing else, and that is a contract rather than a
//! coincidence. A consumer's condition for depending on a module-owned crate
//! was that it must not drag in the module's internals — a client, a runtime, a
//! database driver — because then the location stops mattering and the coupling
//! is real. `deps.rs` asserts the tree stays this shape.
//!
//! # What is NOT here
//!
//! The domain types. `SourceId`, `Timestamp`, `FactKey`, `Amount` and the rest
//! live in `fusiform-core` and `fusiform-store`, and a consumer never needs
//! them: every field below is a `String`, an `i64`, a `BTreeMap` or a
//! `serde_json::Value`. That is not a simplification for the wire — it is what
//! the wire always was, and moving these types here only made it visible.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The tools fusiform serves.
pub const TOOL_GET: &str = "catalog.get";
pub const TOOL_HISTORY: &str = "catalog.history";
pub const TOOL_STATUS: &str = "catalog.status";
/// The only tool that writes.
pub const TOOL_CORRECT: &str = "catalog.correct";

/// Every tool name, so a dispatch and a manifest cannot disagree about which
/// tools exist.
pub const TOOLS: &[&str] = &[TOOL_GET, TOOL_HISTORY, TOOL_STATUS, TOOL_CORRECT];

/// A `catalog.get` request.
///
/// Every field is optional with a stated default, so the empty object `{}` is a
/// valid request for the current catalog. That matters for an operator typing a
/// call by hand and for a consumer that wants everything.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogGetRequest {
    /// Which upstream to read. Defaults to `models.dev`, the only source today.
    ///
    /// Unknown fields are rejected by `deny_unknown_fields` above, and the
    /// source name is matched explicitly rather than parsed leniently. So a
    /// misspelled field or an unsupported source produces an error instead of a
    /// silently different answer.
    #[serde(default)]
    pub source: Option<String>,

    /// Resolve the catalog at this instant, in epoch milliseconds.
    ///
    /// Absent means now — and the response records which instant "now" resolved
    /// to, so the same read is repeatable.
    #[serde(default)]
    pub at_ms: Option<i64>,

    /// Only return facts whose key starts with one of these prefixes.
    ///
    /// Absent means every fact. Measured on a live 6,253-model document: the
    /// full fact set is 3.0 MB of JSON, the capability plane 0.64 MB, rates
    /// 1.55 MB.
    #[serde(default)]
    pub fact_prefixes: Option<Vec<String>>,

    /// Include models the catalog has recorded as retired.
    ///
    /// Defaults to false: a retired model is not part of "what models exist",
    /// and returning it by default would have every consumer write the same
    /// filter. An audit read asks for it.
    #[serde(default)]
    pub include_retired: bool,

    /// Return only this model. `provider_id` must accompany it.
    #[serde(default)]
    pub model_id: Option<String>,

    /// Return only models from this provider.
    #[serde(default)]
    pub provider_id: Option<String>,
}

/// A `catalog.get` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogGetResponse {
    pub source: String,
    /// The instant this snapshot resolves to, always concrete.
    pub resolved_at_ms: i64,
    /// The monotonic catalog version at the time of the read.
    ///
    /// Carried on a pull as well as a push, so a consumer can tell whether a
    /// read it just made is newer than the last push it applied without
    /// correlating two different surfaces.
    pub catalog_version: i64,
    /// Model identity to fact map. The identity is `provider_id/model_id`, the
    /// pair that makes a model unique — measured, 6,253 model rows carry only
    /// 2,957 distinct ids.
    ///
    /// The inner key is a FACT KEY, from the closed set in [`SERVED_FACTS`].
    /// Read that before deciding what to do with a value: it records which
    /// facts can change a consumer's request bytes, which are advisory, and
    /// which fusiform deliberately never serves.
    pub models: BTreeMap<String, BTreeMap<String, serde_json::Value>>,

    /// Facts this read withheld because their record is known bad.
    ///
    /// Omitted from the wire when empty, which is the ordinary case, so this
    /// costs nothing until it matters.
    ///
    /// A withheld fact is absent from `models`, and without this list that
    /// absence is indistinguishable from a fact the upstream never published.
    /// A consumer pricing a model would read "no rate" and could not tell it
    /// from "the rate we recorded is wrong" — the second demands a different
    /// action and is the one that costs money.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub withheld: Vec<WithheldFactWire>,
}

/// Every fact fusiform serves, and what a consumer may do with it.
///
/// # Why this is in the wire crate rather than in fusiform's documentation
///
/// The set of served facts is checkable from the payload — a consumer can
/// enumerate the keys. What is NOT checkable from the payload is which of them
/// may change the bytes a consumer sends to a provider, and that distinction
/// decides whether a wrong value is a cosmetic defect or a silent behaviour
/// change.
///
/// It lived in fusiform's charter and in one conversation with the consumer who
/// named the three byte-affecting fields. Neither is reachable from the artifact
/// a consumer compiles against, which makes it a relationship recorded nowhere a
/// check can see — the failure mode ASTRO and I isolated on 2026-08-12, where
/// every available verification confirms the shapes and none can find the error,
/// because what is missing is not a wrong value but an unrecorded meaning.
///
/// # The classes
///
/// **Byte-affecting.** A consumer legitimately renders these into a request, so
/// a wrong value changes what goes on the wire to a provider with nothing
/// failing. BROCA named these from their own source: `limits.context` drives
/// transform pressure, `limits.output` is rendered as a request parameter, and
/// `capability.reasoning` gates their reasoning policy — a false value strips
/// thinking blocks and the model simply stops reasoning, silently.
///
/// **Money.** Rates price real usage. Absent, zero and unknown are three
/// distinct states here and must not be collapsed: an unpriced fact is a
/// refusal to state a rate, not a rate of zero.
///
/// **Advisory.** Descriptive. A consumer may display or filter on these; a
/// wrong value is visible rather than silent.
///
/// # What fusiform never serves, and why it is not an omission
///
/// Renderer-selection fields — `provider.npm`, per-model provider overrides,
/// and `experimental` — are parsed, flagged, and never emitted. Fusiform says
/// WHAT exists; it never says HOW to speak to it. A consumer that found an
/// endpoint or an auth shape here would be taking wire-family resolution from a
/// catalog, which is the one thing this module is chartered never to do.
/// `crates/fusiform-store/tests/served_vocabulary.rs` fails if one appears.
pub const SERVED_FACTS: &[ServedFact] = &[
    ServedFact {
        key: "existence",
        class: FactClass::Advisory,
        note: "present, absent, or retired. Absence is a withdrawal, not a gap.",
    },
    ServedFact {
        key: "limit.context",
        class: FactClass::ByteAffecting,
        note: "context window; a consumer sizes transform pressure on it",
    },
    ServedFact {
        key: "limit.output",
        class: FactClass::ByteAffecting,
        note: "maximum output; rendered as a request parameter",
    },
    ServedFact {
        key: "capability.reasoning",
        class: FactClass::ByteAffecting,
        note: "gates a reasoning policy; a wrong false silently strips thinking",
    },
    ServedFact {
        key: "capability.tool_call",
        class: FactClass::Advisory,
        note: "whether the model accepts tool definitions",
    },
    ServedFact {
        key: "capability.attachment",
        class: FactClass::Advisory,
        note: "whether the model accepts attachments",
    },
    ServedFact {
        key: "capability.input_modalities",
        class: FactClass::Advisory,
        note: "null when unpublished, which is not the same as an empty list",
    },
    ServedFact {
        key: "capability.output_modalities",
        class: FactClass::Advisory,
        note: "null when unpublished, which is not the same as an empty list",
    },
    ServedFact {
        key: "rate.input",
        class: FactClass::Money,
        note: "per million input tokens",
    },
    ServedFact {
        key: "rate.output",
        class: FactClass::Money,
        note: "per million output tokens",
    },
    ServedFact {
        key: "rate.cache_read",
        class: FactClass::Money,
        note: "per million cached-read tokens",
    },
    ServedFact {
        key: "rate.cache_write",
        class: FactClass::Money,
        note: "per million cache-write tokens",
    },
    ServedFact {
        key: "rate.reasoning",
        class: FactClass::Money,
        note: "per million reasoning tokens",
    },
];

/// One served fact and what a consumer may do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServedFact {
    /// The exact key as it appears in [`CatalogGetResponse::models`].
    ///
    /// A tiered rate appends `.above_context.<threshold>` — the threshold comes
    /// from the upstream and is not fusiform's to enumerate, so tiered keys are
    /// matched by prefix rather than listed.
    pub key: &'static str,
    pub class: FactClass,
    /// What the value means, in one line, for a consumer deciding what to do
    /// with it.
    pub note: &'static str,
}

/// What a wrong value in this fact would do to a consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactClass {
    /// A consumer may render this into a provider request. A wrong value
    /// changes the bytes sent, with nothing failing.
    ByteAffecting,
    /// A rate. Prices real usage; absent, zero and unknown are distinct.
    Money,
    /// Descriptive. A wrong value is visible rather than silent.
    Advisory,
}

/// A fact a read refused to answer, on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WithheldFactWire {
    /// `provider_id/model_id`, the same identity spelling `models` uses.
    pub model: String,
    pub fact_key: String,
    /// Every correction covering the read instant, oldest first.
    pub corrections: Vec<CorrectionDetail>,
}

/// What a served tool call produced.
///
/// One enum rather than three entry points, so the daemon handler dispatches
/// once and every tool shares the same envelope unwrap and the same error
/// mapping. A second handler path is a second place for those to drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ToolResponse {
    Catalog(CatalogGetResponse),
    History(HistoryResponse),
    Status(StatusResponse),
    Correct(CorrectResponse),
}

/// A `catalog.history` request: every era for one fact.
///
/// The fact is named explicitly rather than defaulting to "all facts for this
/// model", because a model's full history is thousands of rows and an operator
/// asking a question has one fact in mind.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRequest {
    #[serde(default)]
    pub source: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    /// A key in the served vocabulary: `rate.input`, `limit.context`,
    /// `capability.reasoning`, `existence`.
    pub fact_key: String,
}

/// What a correction says, as an operator reads it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorrectionDetail {
    /// The interval whose recorded values are known bad. Closed at both ends.
    pub affected_from_ms: i64,
    pub affected_until_ms: i64,
    /// Why the record was wrong, in fusiform's own words.
    pub reason: String,
    /// The `FieldId` list the correction names, as stored.
    pub fields: serde_json::Value,
}

/// One era, as an operator reads it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEra {
    pub value: serde_json::Value,
    pub boundary_at_ms: i64,
    /// observed / asserted / seed / corrected.
    pub boundary_kind: String,
    /// The extent and reason, present only on a `corrected` boundary.
    ///
    /// Without this an operator sees `kind: "corrected"` and cannot tell which
    /// interval was affected or why — the cause recorded in the store with no
    /// path to the surface anyone reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correction: Option<CorrectionDetail>,
    /// The other edge of the observation window.
    ///
    /// Present only on an observed boundary. Its absence is the honest answer
    /// for a seed or a correction: neither is an observation of an upstream
    /// change, so neither has a window to report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_from_ms: Option<i64>,
}

/// A `catalog.history` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryResponse {
    pub source: String,
    pub provider_id: String,
    pub model_id: String,
    pub fact_key: String,
    /// Oldest first: a history is read forwards.
    pub eras: Vec<HistoryEra>,
}

/// A `catalog.status` request.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {
    #[serde(default)]
    pub source: Option<String>,
    /// How many recent polls to include. Defaults to 10.
    #[serde(default)]
    pub polls: Option<u32>,
}

/// One recorded poll.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusPoll {
    pub observed_at_ms: i64,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
}

/// A `catalog.status` response: what fusiform has been doing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusResponse {
    pub source: String,
    pub catalog_version: i64,
    pub model_count: usize,
    pub era_count: i64,
    /// The most recent polls, newest first.
    ///
    /// Failures and 304s included. An operator asking what fusiform has been
    /// doing needs the polls that changed nothing most of all: a source
    /// returning 304 for a week and a source failing for a week are
    /// indistinguishable from the catalog alone.
    pub recent_polls: Vec<StatusPoll>,
}

impl CatalogGetResponse {
    pub fn model_count(&self) -> usize {
        self.models.len()
    }

    pub fn fact_count(&self) -> usize {
        self.models.values().map(|f| f.len()).sum()
    }
}

/// A `catalog.correct` request: record that fusiform's own record was wrong.
///
/// # Why this carries no value, and no wildcard
///
/// The operator supplies a DIAGNOSIS — which model, which facts, what window,
/// and why. They supply no value: `affected_until` is when the fix deployed and
/// fusiform stopped recording the bad value, so a later poll has already
/// written the right one. A correction marks a window; it never edits a value.
///
/// And it names exactly one model. There is no "all models" or "every fact"
/// form, deliberately: a correction makes reads inside its window refuse, so a
/// wildcard correction is a catalog kill switch. Fusiform cannot tell who is
/// calling — `RequestCtx` carries no consumer identity — so the blast radius is
/// bounded by what the request can express rather than by who may send it.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CorrectRequest {
    #[serde(default)]
    pub source: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    /// The `FieldId` values this correction names, as the served vocabulary
    /// spells them: `{"field":"rate","class":"input"}`, `{"field":"limit",
    /// "limit":"context"}`, `{"field":"existence"}`.
    pub fields: Vec<serde_json::Value>,
    /// Start of the bad window. A LOWER BOUND: when the true start is not known
    /// precisely it goes to the earliest plausible instant, never the best
    /// guess. An over-inclusive partition costs review time; an under-inclusive
    /// one leaves bad facts outside a partition asserting they are fine.
    pub affected_from_ms: i64,
    /// End of the bad window: when the fix deployed and fusiform stopped
    /// recording the bad value.
    pub affected_until_ms: i64,
    /// Why the record was wrong. Names a finding document, so an audit is
    /// repeatable rather than dependent on prose.
    pub reason: String,
    /// Resolve and report without writing.
    ///
    /// Defaults to true. A write that happens because a flag was forgotten is
    /// the wrong default for the only command in this module that changes what
    /// the catalog says about the past.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

fn default_true() -> bool {
    true
}

/// What a correction did, or would do.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorrectResponse {
    /// False when this was a dry run.
    pub written: bool,
    /// One entry per fact, each carrying the value that stays in force.
    pub facts: Vec<CorrectedFact>,
    pub affected_from_ms: i64,
    pub affected_until_ms: i64,
    pub reason: String,
}

/// One fact a correction marked, or would mark.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorrectedFact {
    pub fact_key: String,
    /// The value that remains in force, read from the store rather than
    /// supplied. Shown so an operator can see what the catalog will still say.
    pub value: serde_json::Value,
    /// When the era carrying that value was established.
    pub current_since_ms: i64,
}
