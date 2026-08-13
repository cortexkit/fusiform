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

use crate::overlay;
use fusiform_core::{FieldId, SourceId, Timestamp};
use fusiform_store::correct::{apply_correction, plan_correction};
use fusiform_store::serve::{CatalogQuery, CatalogSnapshot, Presence};
use fusiform_store::{CatalogError, CatalogStore, FactKey};
use serde::Deserialize;

// The served contract lives in `fusiform-protocol` so a consumer can compile
// against it without taking this module's store, network or runtime. Re-exported
// here because this file is where the contract is USED, and a reader following
// a route should not have to know which crate the type is declared in.
pub use fusiform_protocol::{
    CatalogGetRequest, CatalogGetResponse, CorrectRequest, CorrectResponse, CorrectedFact,
    CorrectionDetail, HistoryEra, HistoryRequest, HistoryResponse, OverriddenFactWire, PollChanges,
    StatusPoll, StatusRequest, StatusResponse, ToolResponse, UncertainFactWire, WithheldFactWire,
    TOOLS, TOOL_CORRECT, TOOL_GET, TOOL_HISTORY, TOOL_STATUS,
};

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
        TOOL_CORRECT => serve_correct(store, &args).map(ToolResponse::Correct),
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
        // Counted from the same snapshot as the model total, so the two cannot
        // disagree about which models exist.
        models_priced: Some(snapshot.priced_model_count()),
        era_count,
        recent_polls: polls
            .into_iter()
            .map(|p| StatusPoll {
                observed_at_ms: p.observed_at.0,
                outcome: p.outcome,
                failure_class: p.failure_class,
                detail: p.detail,
                duration_ms: p.duration_ms,
                // Omitted when the poll wrote nothing — a 304, an unchanged
                // document, or a failure. Reporting zeroes there would put
                // four fields on every ordinary poll to say nothing happened,
                // which the outcome already says.
                changes: (p.eras > 0).then_some(PollChanges {
                    eras: p.eras,
                    models_arrived: p.models_arrived,
                    models_withdrawn: p.models_withdrawn,
                    facts_changed: p.facts_changed,
                }),
            })
            .collect(),
    })
}

/// Serve `catalog.correct`: record that fusiform's own record was wrong.
///
/// The only route that writes. It plans against the store first and reports
/// every refusal at once, because an operator correcting six facts wants six
/// problems in one reply rather than six round trips.
pub fn serve_correct(store: &CatalogStore, body: &[u8]) -> Result<CorrectResponse, RouteError> {
    let request: CorrectRequest = serde_json::from_slice(body)
        .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?;
    let source = parse_source(request.source.as_deref())?;

    if request.reason.trim().is_empty() {
        // A correction with no reason is unauditable: the design's whole point
        // is that a consumer can trace a partition back to a named defect.
        return Err(RouteError::bad_request(
            "a correction must carry a reason naming the defect record".to_string(),
        ));
    }

    // Field ids arrive as the served vocabulary spells them, and are parsed
    // through the same serde representation a consumer reads. A field this does
    // not recognise is refused rather than skipped: silently dropping one would
    // write a correction with a narrower extent than the operator stated.
    let mut fields = Vec::new();
    for raw in &request.fields {
        let field: FieldId = serde_json::from_value(raw.clone()).map_err(|e| {
            RouteError::bad_request(format!(
                "{raw} is not a field this catalog can correct: {e}"
            ))
        })?;
        fields.push(field);
    }
    if fields.is_empty() {
        return Err(RouteError::bad_request(
            "a correction must name at least one field".to_string(),
        ));
    }

    let now = Timestamp(now_ms());
    let planned = plan_correction(
        store,
        source,
        &request.provider_id,
        &request.model_id,
        &fields,
        Timestamp(request.affected_from_ms),
        Timestamp(request.affected_until_ms),
        &request.reason,
        now,
    )
    .map_err(|e| store_error(&e))?;

    let plan = match planned {
        Ok(plan) => plan,
        Err(refusals) => {
            // Every refusal, joined. The code is coarse on purpose — a consumer
            // branches on "refused", an operator reads the message.
            let detail = refusals
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(RouteError {
                code: "refused",
                message: detail,
            });
        }
    };

    let facts: Vec<CorrectedFact> = plan
        .rows
        .iter()
        .map(|row| CorrectedFact {
            fact_key: row.fact_key.as_str().to_string(),
            value: serde_json::from_str(&row.value_json)
                .unwrap_or(serde_json::Value::String(row.value_json.clone())),
            current_since_ms: row.current_since.0,
        })
        .collect();

    if !request.dry_run {
        apply_correction(store, source, &plan, now).map_err(|e| store_error(&e))?;
    }

    Ok(CorrectResponse {
        written: !request.dry_run,
        facts,
        affected_from_ms: plan.affected_from.0,
        affected_until_ms: plan.affected_until.0,
        reason: plan.reason,
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

            // An unknown provider is a REFUSAL, not an empty result.
            //
            // "0 models" is a true statement about the filter and a misleading
            // one about the catalog: a misspelled provider and a real provider
            // whose models are all retired produce the identical answer, and
            // the first is far more common at a terminal. An operator reading
            // zero concludes the catalog is missing something.
            //
            // Checked only when the filter matched nothing, so the ordinary
            // path pays no query.
            if snapshot.models.is_empty()
                && !store
                    .provider_is_known(source, &provider)
                    .map_err(|e| store_error(&e))?
            {
                return Err(RouteError::bad_request(format!(
                    "unknown provider {provider:?}: fusiform has never recorded \
                     a model under that id. Check the spelling — provider ids \
                     are the upstream's, so \"anthropic\" rather than \"Anthropic\""
                )));
            }
        }
        snapshot
    };

    Ok(render(source, snapshot, at, overlay::corrections()))
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

    // A name fusiform has never recorded is a REFUSAL, not an empty answer.
    //
    // Same reasoning as the provider filter above, and the two failures are
    // separated because the correct action differs: a wrong provider means the
    // whole id is wrong, while a wrong model under a real provider usually
    // means a version suffix. Reporting "0 models" for either leaves an
    // operator unable to tell a typo from a catalog that genuinely lacks the
    // model — and at a terminal the typo is far more likely.
    //
    // Reached only when the read found nothing, so the ordinary path pays no
    // extra query. History counts as known: a withdrawn model is still a name
    // fusiform recognises, and saying otherwise would send someone hunting a
    // typo they did not make.
    if found.is_none() {
        if !store
            .provider_is_known(source, provider_id)
            .map_err(|e| store_error(&e))?
        {
            return Err(RouteError::bad_request(format!(
                "unknown provider {provider_id:?}: fusiform has never recorded \
                 a model under that id"
            )));
        }
        if !store
            .model_is_known(source, provider_id, model_id)
            .map_err(|e| store_error(&e))?
        {
            return Err(RouteError::bad_request(format!(
                "unknown model {model_id:?} under provider {provider_id:?}: the \
                 provider exists, so check the model id — upstream ids often \
                 carry a version suffix"
            )));
        }
        // Known, and absent at this instant: that is a real answer about a real
        // model, so it stays an empty result rather than becoming an error.
    }

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

    // The same uncertainty the bulk read reports, scoped to this model.
    //
    // Resolved from the store rather than carried through `read_model`, so the
    // two surfaces cannot disagree: a consumer asking for one model and a
    // consumer asking for the catalog must get the same qualification on the
    // same fact. The single-model path is the one that silently lacked
    // correction handling for hours, and this is the same seam.
    let uncertain = store
        .uncertain_facts_at(source, resolved_at)
        .map_err(|e| store_error(&e))?
        .into_iter()
        .filter(|((p, m, _), _)| p == provider_id && m == model_id)
        .map(|((provider_id, model_id, fact_key), (prior, boundary))| {
            fusiform_store::serve::UncertainFact {
                provider_id,
                model_id,
                fact_key,
                superseded_after: prior,
                superseded_by: boundary,
            }
        })
        .collect();

    Ok(CatalogSnapshot {
        source,
        resolved_at,
        catalog_version,
        models,
        withheld,
        uncertain,
    })
}

/// Render a snapshot as the wire response.
///
/// Fact values are parsed from their stored JSON text back into JSON values, so
/// a consumer receives `{"units": 3000000000}` rather than a string containing
/// that. A value that does not parse is a stored row this build cannot read,
/// which is loud rather than silently dropped: dropping it would present a
/// model as having no rate.
/// Render a snapshot onto the wire.
///
/// Takes `at` so it can tell a current-view read from a point-in-time one:
/// corrections apply only to the former, and the distinction is not derivable
/// from the snapshot, which always carries a concrete resolved instant.
fn render(
    source: SourceId,
    snapshot: CatalogSnapshot,
    at: Option<Timestamp>,
    corrections: &overlay::Corrections,
) -> CatalogGetResponse {
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

    let uncertain = snapshot
        .uncertain
        .into_iter()
        .map(|u| UncertainFactWire {
            provider_id: u.provider_id,
            model_id: u.model_id,
            fact_key: u.fact_key.as_str().to_string(),
            superseded_after_ms: u.superseded_after.0,
            superseded_by_ms: u.superseded_by.0,
        })
        .collect();

    // Corrections apply to the CURRENT VIEW only.
    //
    // `at` is Some for a point-in-time read, and an overlay cell carries
    // observed_at with no validity interval — so applying today's reading to a
    // past instant would state something about a window nobody observed. The
    // record keeps saying what models.dev said.
    let overridden = if at.is_none() {
        apply_corrections(&mut models, corrections)
    } else {
        Vec::new()
    };

    CatalogGetResponse {
        source: source.as_str().to_string(),
        resolved_at_ms: snapshot.resolved_at.0,
        catalog_version: snapshot.catalog_version,
        models,
        withheld,
        uncertain,
        overridden,
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

/// Replace upstream values with fusiform's corrections, for a CURRENT-VIEW read.
///
/// # The point-in-time exclusion is the whole reason the caller passes a flag
///
/// An overlay cell carries `observed_at` and no validity interval. "Anthropic's
/// documentation, read today, says 200k" does not establish what was true in
/// July — they may have reduced it, and the catalog may have been right at the
/// time. Applying a present-day reading to a past instant manufactures a claim
/// about a window nobody observed: not a forged observation, a forged
/// inference.
///
/// So a point-in-time read never reaches this function, and history keeps
/// saying what models.dev said — the true statement about the record.
fn apply_corrections(
    models: &mut BTreeMap<String, BTreeMap<String, serde_json::Value>>,
    corrections: &overlay::Corrections,
) -> Vec<OverriddenFactWire> {
    let mut applied = Vec::new();

    for (identity, facts) in models.iter_mut() {
        // The identity is `provider/model`, and a model id may itself contain a
        // slash (`openai/gpt-oss-120b` under 28 providers). Split ONCE from the
        // left: the provider id never contains one.
        let Some((provider_id, model_id)) = identity.split_once('/') else {
            continue;
        };

        for (fact_key, value) in facts.iter_mut() {
            let key = (
                provider_id.to_string(),
                model_id.to_string(),
                FactKey::from_stored(fact_key.clone()),
            );
            let Some(correction) = corrections.get(&key) else {
                continue;
            };

            let served: serde_json::Value = match correction.served_value.parse::<i64>() {
                Ok(n) => serde_json::Value::from(n),
                Err(_) => serde_json::Value::String(correction.served_value.clone()),
            };

            // A correction that changes nothing is not reported. The upstream
            // may have fixed the row, leaving the cell redundant rather than
            // wrong — and a line reading "1000000 -> 1000000" trains an
            // operator to skim the ones that matter.
            if *value == served {
                continue;
            }

            applied.push(OverriddenFactWire {
                provider_id: provider_id.to_string(),
                model_id: model_id.to_string(),
                fact_key: fact_key.clone(),
                upstream_value: value.to_string(),
                served_value: correction.served_value.clone(),
                authority: correction.authority.clone(),
            });
            *value = served;
        }
    }

    applied
}
