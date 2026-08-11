//! The `catalog.get` route: turning a request into a catalog read.
//!
//! Kept separate from the daemon wiring so the request/response contract can be
//! tested without a socket. The daemon's handler is a thin adapter over
//! [`serve_catalog_get`].
//!
//! # What this layer decides, and what it refuses to guess at
//!
//! It parses a request, runs one read, and renders the result. It does not
//! interpret values: a rate that is `Unpriced(MissingRate)` travels as that, and
//! a limit the upstream stopped publishing travels as `null`. Every place this
//! layer could substitute a plausible value for a missing one is a place a
//! consumer would silently spend against a number fusiform invented.
//!
//! It also refuses to guess at a malformed request. An unparseable body, an
//! unknown source, an unreadable instant — each is an error naming what was
//! wrong, never a default read. A consumer that asked for the catalog at an
//! instant and received the current catalog would have no way to notice.

use std::collections::BTreeMap;

use fusiform_core::{SourceId, Timestamp};
use fusiform_store::serve::{CatalogQuery, CatalogSnapshot, Presence};
use fusiform_store::{CatalogError, CatalogStore, FactKey};
use serde::{Deserialize, Serialize};

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

/// A fact a read refused to answer, on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WithheldFactWire {
    /// `provider_id/model_id`, the same identity spelling `models` uses.
    pub model: String,
    pub fact_key: String,
    /// Every correction covering the read instant, oldest first.
    pub corrections: Vec<CorrectionDetail>,
}

impl CatalogGetResponse {
    pub fn model_count(&self) -> usize {
        self.models.len()
    }

    pub fn fact_count(&self) -> usize {
        self.models.values().map(|f| f.len()).sum()
    }
}

/// Why a request could not be served.
///
/// The `code` is what a consumer branches on and is deliberately coarse; the
/// message carries the detail. A rich vocabulary nobody branches on is the same
/// defect as a rich success shape nobody can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteError {
    pub code: &'static str,
    pub message: String,
}

impl RouteError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            code: "bad_request",
            message: message.into(),
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            code: "unavailable",
            message: message.into(),
        }
    }
}

/// The tool-call envelope a `ToolProvider` receives on the wire.
///
/// A tool call arrives as `{"name": "<tool>", "arguments": {...}}` and the
/// module gets it intact — subc does not unwrap it. Verified against a live
/// document rather than assumed: passing the envelope straight to the request
/// parser is refused with `unknown field "name"`, because the request type
/// denies unknown fields.
///
/// The library's own echo example parses the body directly as its request,
/// which works only because its match falls through to a default arm. A type
/// that refuses unknown fields — which is the behaviour worth having — makes
/// the envelope mandatory.
#[derive(Debug, Deserialize)]
struct ToolCall {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

/// The tools this module serves.
pub const TOOL_GET: &str = "catalog.get";
pub const TOOL_HISTORY: &str = "catalog.history";
pub const TOOL_STATUS: &str = "catalog.status";

/// Every tool name, so the dispatch and the manifest cannot disagree about
/// which tools exist.
pub const TOOLS: &[&str] = &[TOOL_GET, TOOL_HISTORY, TOOL_STATUS];

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

/// Serve one tool call, unwrapping the wire envelope and dispatching by name.
///
/// This is what the daemon handler calls. The envelope is unwrapped here rather
/// than in the handler so the wire contract is covered by a test that needs no
/// socket.
pub fn serve_tool_call(store: &CatalogStore, body: &[u8]) -> Result<ToolResponse, RouteError> {
    // An empty body is a bare `catalog.get` with no arguments: the read an
    // operator means when they call the module with nothing.
    if body.is_empty() {
        return serve_catalog_get(store, b"").map(ToolResponse::Catalog);
    }

    let call: ToolCall = serde_json::from_slice(body).map_err(|e| {
        RouteError::bad_request(format!(
            "a tool call must be {{\"name\": \"<tool>\", \"arguments\": {{...}}}}: {e}"
        ))
    })?;

    // `null` arguments and an absent `arguments` key both mean "no arguments".
    let args = if call.arguments.is_null() {
        Vec::new()
    } else {
        serde_json::to_vec(&call.arguments).map_err(|e| {
            RouteError::bad_request(format!("tool arguments did not re-serialize: {e}"))
        })?
    };

    match call.name.as_str() {
        TOOL_GET => serve_catalog_get(store, &args).map(ToolResponse::Catalog),
        TOOL_HISTORY => serve_history(store, &args).map(ToolResponse::History),
        TOOL_STATUS => serve_status(store, &args).map(ToolResponse::Status),
        // Named explicitly rather than served anyway. A module that answers to
        // any tool name keeps answering after a consumer's typo, and the
        // consumer believes it called something else.
        other => Err(RouteError::bad_request(format!(
            "unknown tool {other:?}; fusiform serves {}",
            TOOLS.join(", ")
        ))),
    }
}

/// Serve `catalog.history`: every era for one fact.
pub fn serve_history(store: &CatalogStore, body: &[u8]) -> Result<HistoryResponse, RouteError> {
    let request: HistoryRequest = serde_json::from_slice(body)
        .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?;
    let source = parse_source(request.source.as_deref())?;

    let fact = FactKey::from_stored(request.fact_key.clone());
    let rows = store
        .fact_history(source, &request.provider_id, &request.model_id, &fact)
        .map_err(|e| store_error(&e))?;

    let eras = rows
        .into_iter()
        .map(|row| HistoryEra {
            value: serde_json::from_str(&row.value_json)
                .unwrap_or(serde_json::Value::String(row.value_json.clone())),
            boundary_at_ms: row.boundary_at.0,
            boundary_kind: row.boundary_kind,
            correction: row.correction.map(|c| CorrectionDetail {
                affected_from_ms: c.affected_from.0,
                affected_until_ms: c.affected_until.0,
                reason: c.reason,
                fields: serde_json::from_str(&c.fields_json)
                    .unwrap_or(serde_json::Value::String(c.fields_json)),
            }),
            window_from_ms: row.prior_observation_at.map(|t| t.0),
        })
        .collect();

    Ok(HistoryResponse {
        source: source.as_str().to_string(),
        provider_id: request.provider_id,
        model_id: request.model_id,
        fact_key: request.fact_key,
        eras,
    })
}

/// Serve `catalog.status`: what fusiform has been doing.
pub fn serve_status(store: &CatalogStore, body: &[u8]) -> Result<StatusResponse, RouteError> {
    let request: StatusRequest = if body.is_empty() {
        StatusRequest::default()
    } else {
        serde_json::from_slice(body)
            .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?
    };
    let source = parse_source(request.source.as_deref())?;

    let polls = store
        .recent_observations(source, request.polls.unwrap_or(10))
        .map_err(|e| store_error(&e))?;
    let era_count = store.era_count(source).map_err(|e| store_error(&e))?;
    let catalog_version = store.catalog_version().map_err(|e| store_error(&e))?;

    // The model count comes from the same read a consumer would get, so status
    // and catalog.get cannot disagree about how many models exist.
    let snapshot = store
        .read_catalog(&CatalogQuery::current(source))
        .map_err(|e| store_error(&e))?;

    Ok(StatusResponse {
        source: source.as_str().to_string(),
        catalog_version,
        model_count: snapshot.model_count(),
        era_count,
        recent_polls: polls
            .into_iter()
            .map(|p| StatusPoll {
                observed_at_ms: p.observed_at.0,
                outcome: p.outcome,
                failure_class: p.failure_class,
                detail: p.detail,
                duration_ms: p.duration_ms,
            })
            .collect(),
    })
}

/// Resolve a source name, refusing one fusiform does not have.
fn parse_source(name: Option<&str>) -> Result<SourceId, RouteError> {
    match name {
        None | Some("models.dev") => Ok(SourceId::ModelsDev),
        Some("seed") => Ok(SourceId::Seed),
        Some(other) => Err(RouteError::bad_request(format!(
            "unknown source {other:?}; fusiform serves \"models.dev\""
        ))),
    }
}

/// Serve one `catalog.get` request against the store.
///
/// Takes the ARGUMENTS object, not the tool-call envelope. Callers on the wire
/// path want [`serve_tool_call`].
pub fn serve_catalog_get(
    store: &CatalogStore,
    body: &[u8],
) -> Result<CatalogGetResponse, RouteError> {
    // An empty body is a request for everything. A consumer calling a read with
    // no arguments should not have to know that `{}` is the incantation.
    let request: CatalogGetRequest = if body.is_empty() {
        CatalogGetRequest::default()
    } else {
        serde_json::from_slice(body)
            .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?
    };

    let source = parse_source(request.source.as_deref())?;

    if request.model_id.is_some() && request.provider_id.is_none() {
        // A bare model id is ambiguous by construction, and answering with the
        // first match would be arbitrary: one id appears under as many as 28
        // providers at different prices.
        return Err(RouteError::bad_request(
            "model_id requires provider_id: a model id alone is not unique, and \
             one id appears under as many as 28 providers at different prices",
        ));
    }

    let at = request.at_ms.map(Timestamp);

    let snapshot = if let (Some(provider_id), Some(model_id)) =
        (request.provider_id.as_deref(), request.model_id.as_deref())
    {
        single_model_snapshot(store, source, provider_id, model_id, at, &request)?
    } else {
        let mut query = CatalogQuery {
            source,
            at,
            presence: if request.include_retired {
                Presence::IncludingRetired
            } else {
                Presence::PresentOnly
            },
            facts: match &request.fact_prefixes {
                Some(prefixes) => fusiform_store::serve::FactFilter::Prefixes(prefixes.clone()),
                None => fusiform_store::serve::FactFilter::All,
            },
        };
        // A provider filter with no model id: narrow after the read rather than
        // in SQL, because the read is index-backed on the full key and a
        // provider-only predicate would not use it.
        let provider_filter = request.provider_id.clone();
        query.at = at;
        let mut snapshot = store.read_catalog(&query).map_err(|e| store_error(&e))?;
        if let Some(provider) = provider_filter {
            snapshot.models.retain(|m| m.provider_id == provider);
        }
        snapshot
    };

    Ok(render(source, snapshot))
}

fn single_model_snapshot(
    store: &CatalogStore,
    source: SourceId,
    provider_id: &str,
    model_id: &str,
    at: Option<Timestamp>,
    request: &CatalogGetRequest,
) -> Result<CatalogSnapshot, RouteError> {
    let resolved_at = at.unwrap_or_else(|| Timestamp(now_ms()));
    let (found, withheld) = store
        .read_model(source, provider_id, model_id, Some(resolved_at))
        .map_err(|e| store_error(&e))?;

    let catalog_version = store.catalog_version().map_err(|e| store_error(&e))?;

    let mut models = Vec::new();
    if let Some(model) = found {
        // Presence is checked before the fact filter, for the same reason the
        // catalog read checks it first: a rates-only request must not make a
        // present model look retired.
        let keep = request.include_retired || model.is_present();
        if keep {
            let facts = match &request.fact_prefixes {
                Some(prefixes) => model
                    .facts
                    .into_iter()
                    .filter(|(k, _)| prefixes.iter().any(|p| k.as_str().starts_with(p)))
                    .collect(),
                None => model.facts,
            };
            if !facts.is_empty() {
                models.push(fusiform_store::serve::ModelFacts {
                    provider_id: provider_id.to_string(),
                    model_id: model_id.to_string(),
                    facts,
                });
            }
        }
    }

    Ok(CatalogSnapshot {
        source,
        resolved_at,
        catalog_version,
        models,
        withheld,
    })
}

/// Render a snapshot as the wire response.
///
/// Fact values are parsed from their stored JSON text back into JSON values, so
/// a consumer receives `{"units": 3000000000}` rather than a string containing
/// that. A value that does not parse is a stored row this build cannot read,
/// which is loud rather than silently dropped: dropping it would present a
/// model as having no rate.
fn render(source: SourceId, snapshot: CatalogSnapshot) -> CatalogGetResponse {
    let mut models = BTreeMap::new();
    for model in snapshot.models {
        let identity = format!("{}/{}", model.provider_id, model.model_id);
        let mut facts = BTreeMap::new();
        for (key, raw) in model.facts {
            let value = serde_json::from_str(&raw).unwrap_or_else(|_| {
                // A stored value this build cannot parse. Carried as a string
                // rather than dropped, so the row is visible and reportable
                // instead of presenting as an absent fact.
                serde_json::Value::String(raw.clone())
            });
            facts.insert(key.as_str().to_string(), value);
        }
        models.insert(identity, facts);
    }

    let withheld = snapshot
        .withheld
        .into_iter()
        .map(|w| WithheldFactWire {
            model: format!("{}/{}", w.provider_id, w.model_id),
            fact_key: w.fact_key.as_str().to_string(),
            corrections: w
                .corrections
                .into_iter()
                .map(|c| CorrectionDetail {
                    affected_from_ms: c.affected_from.0,
                    affected_until_ms: c.affected_until.0,
                    reason: c.reason,
                    fields: serde_json::from_str(&c.fields_json)
                        .unwrap_or(serde_json::Value::String(c.fields_json)),
                })
                .collect(),
        })
        .collect();

    CatalogGetResponse {
        source: source.as_str().to_string(),
        resolved_at_ms: snapshot.resolved_at.0,
        catalog_version: snapshot.catalog_version,
        models,
        withheld,
    }
}

fn store_error(e: &CatalogError) -> RouteError {
    RouteError::unavailable(format!("catalog read failed: {e}"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
