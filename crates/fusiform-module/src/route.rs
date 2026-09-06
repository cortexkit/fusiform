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

    // AN EMPTY HISTORY HAS THREE CAUSES AND THEY NEED DIFFERENT ACTIONS.
    //
    // `catalog.get` already separates them: a wrong provider means the whole
    // id is wrong, a wrong model under a real provider usually means a version
    // suffix, and a real model with nothing recorded for this fact is a
    // genuine answer. History returned the same empty result for all three,
    // and the CLI rendered one sentence — "no eras recorded for this fact" —
    // whose comment named a typo in the KEY as the cause. That is the third
    // possibility stated as the diagnosis, so an operator with a mistyped
    // MODEL was told to check the key.
    //
    // Reached only when the read found nothing, so the ordinary path pays no
    // extra query.
    if rows.is_empty() {
        if !store
            .provider_is_known(source, &request.provider_id)
            .map_err(|e| store_error(&e))?
        {
            return Err(RouteError::bad_request(format!(
                "unknown provider {:?}: fusiform has never recorded a model \
                 under that id",
                request.provider_id
            )));
        }
        if !store
            .model_is_known(source, &request.provider_id, &request.model_id)
            .map_err(|e| store_error(&e))?
        {
            return Err(RouteError::bad_request(format!(
                "unknown model {:?} under provider {:?}: the provider exists, \
                 so check the model id — upstream ids often carry a version \
                 suffix",
                request.model_id, request.provider_id
            )));
        }
        // Known model, nothing recorded for this fact: a real answer, and now
        // the only remaining cause is the fact key. The empty response says
        // which of the three it is by elimination.
    }

    // Annotated exactly as the catalog path annotates, because the reason for
    // the annotation does not weaken with age.
    //
    // The golden fixture caught this: removing the storage write dropped
    // provenance from HISTORY while `catalog.get` kept it, because only the
    // catalog renderer applied the serve-time fill. A historical rate reading
    // `currency: USD` with nothing saying the currency was assumed is the
    // laundering this field exists to prevent — an operator auditing a past
    // charge is exactly who needs to know models.dev published no currency at
    // all.
    let eras: Vec<HistoryEra> = rows
        .into_iter()
        .map(|row| HistoryEra {
            value: {
                let mut v = fact_for_the_wire(&row.value_json);
                name_the_provenance_of_a_pre_0_11_row(&mut v);
                v
            },
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

    // Disclose an override on this fact WITHOUT applying it to the eras.
    //
    // Through the SAME function `catalog.get` uses, because the first version
    // of this did the lookup by hand and got both halves wrong in production:
    // it reported an override on a model whose overlay value AGREES with the
    // catalog, and it printed an empty upstream value. Two rules live in that
    // function and a hand-written second copy had neither.
    //
    // The current value is the LAST era: this query orders oldest-first, and a
    // reader reaching for `first()` would compare against the model's original
    // value forever.
    //
    // The eras are what the upstream published and stay that way; an override
    // is a judgment about the current view. But an operator reading history for
    // an overridden fact was seeing the upstream's number alone, with nothing
    // reconciling it against what catalog.get serves — two surfaces
    // disagreeing, both correct, which reads as one of them being wrong.
    let overridden = eras.last().and_then(|newest| {
        override_for(
            &newest.value,
            &request.provider_id,
            &request.model_id,
            &request.fact_key,
            overlay::corrections().get(&(
                request.provider_id.clone(),
                request.model_id.clone(),
                FactKey::from_stored(request.fact_key.clone()),
            ))?,
        )
    });

    Ok(HistoryResponse {
        source: source.as_str().to_string(),
        provider_id: request.provider_id,
        model_id: request.model_id,
        fact_key: request.fact_key,
        eras,
        overridden,
    })
}

/// The disclosure a correction produces against a value the store actually
/// holds, or `None` when there is nothing to say.
///
/// # Why this is one function and not a rule written down twice
///
/// It encodes two decisions that are easy to state and easy to omit, and a
/// second hand-written copy omitted BOTH in production:
///
/// 1. **A correction that changes nothing is not reported.** The upstream may
///    have fixed the row, leaving the cell redundant rather than wrong. A line
///    reading `1000000 -> 1000000` trains an operator to skim the ones that
///    matter — and worse, a note on every response destroys the very
///    distinction the disclosure exists to draw.
///
/// 2. **The upstream value comes from the ROW, never from the correction.**
///    `Correction::upstream_value` is a slot filled in here, not a value
///    carried by the overlay: the line must report what THIS poll published,
///    not what the overlay's author saw. Reading the unfilled slot yields an
///    empty string, which renders as a sentence naming one value where it
///    promises two.
fn override_for(
    value: &serde_json::Value,
    provider_id: &str,
    model_id: &str,
    fact_key: &str,
    correction: &overlay::Correction,
) -> Option<OverriddenFactWire> {
    let served: serde_json::Value = match correction.served_value.parse::<i64>() {
        Ok(n) => serde_json::Value::from(n),
        Err(_) => serde_json::Value::String(correction.served_value.clone()),
    };
    if *value == served {
        return None;
    }
    Some(OverriddenFactWire {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        fact_key: fact_key.to_string(),
        upstream_value: value.to_string(),
        served_value: correction.served_value.clone(),
        authority: correction.authority.clone(),
    })
}

/// Refuse a point-in-time read that predates everything this store knows.
///
/// # The defect
///
/// A read at an instant before the earliest observation returned an EMPTY
/// CATALOG with a success status — 0 models, well-formed, no signal. Measured
/// on the live store 2026-08-15: the record begins 2026-08-12 10:07:29Z, and a
/// read at 02:00:00Z the same morning answered "0 models" rather than "I was
/// not watching".
///
/// A consumer replaying a past decision or re-pricing a past charge gets
/// "nothing existed then", which is a claim about the world, from a store
/// whose only honest claim is about its own coverage. It is the same polarity
/// error as accepting a source no row carries, and the same one the single
/// model path already avoids for unknown ids — an answer of zero must not be
/// how a query reports that it could not be asked.
///
/// # Why this is not the same as "absent at that instant"
///
/// A model that exists and was not published at T is a REAL answer: fusiform
/// watched at T and did not see it. That case stays an empty result, and the
/// comment at its site says so. The difference is whether fusiform was
/// watching at all, which no per-model check can determine and only the
/// store's coverage bound can.
fn refuse_before_the_record(
    store: &CatalogStore,
    source: SourceId,
    at: Option<Timestamp>,
) -> Result<(), RouteError> {
    let Some(asked) = at else {
        return Ok(());
    };
    let Some(begins) = store
        .record_begins_at(source)
        .map_err(|e| store_error(&e))?
    else {
        // No records at all: an empty store cannot bound anything, and the
        // seed writes one on first boot. Refusing here would fail a fresh
        // install for a coverage claim it has no way to make.
        return Ok(());
    };
    if asked.0 < begins.0 {
        return Err(RouteError::bad_request(format!(
            "no record at {}: fusiform's history begins at {}, so it cannot say              what the catalog held before then. An empty answer would claim the              catalog was empty; it was unobserved.",
            asked.0, begins.0
        )));
    }
    Ok(())
}

/// Fill in the currency provenance of a rate written before the field existed.
///
/// # Why absence cannot simply be left alone
///
/// `unit_provenance` began being stored in fusiform-protocol 0.11.0. The live
/// store holds 19,707 rate eras written before it, and an append-only store
/// never rewrites them — they are what fusiform believed when they were
/// written, and editing them would destroy the record this store exists to
/// keep.
///
/// So absence would have to mean something on the wire, and it CANNOT mean
/// nothing: `UnitProvenance::Unknown` is a real variant meaning "no statement
/// and no policy covers this", which is the state that stops a non-USD
/// provider being silently priced in dollars. An absent field would be
/// indistinguishable from that, and a consumer treating a pre-0.11.0 USD row
/// as unknown-provenance would refuse to price a rate that is perfectly well
/// established.
///
/// # Why filling it in is a fact rather than an invention
///
/// The population is CLOSED and its provenance is known with certainty. Every
/// rate row written before 0.11.0 came through one normalization path, whose
/// only currency branch is `AssumedByPolicy(models-dev-usd-v1)` — models.dev
/// publishes no currency field at all, so there has never been another way for
/// a rate to acquire one. After 0.11.0 every new row carries the field
/// explicitly, so the defaulted population can only shrink.
///
/// This is the one shape where a serve-time default is honest: not "we do not
/// know so here is a guess", but "we know, and the row predates the column".
///
/// The claim's truth rests on there being exactly one policy, which is
/// enforced by `only_one_currency_policy_has_ever_existed` — a second policy
/// makes the default ambiguous and must force a decision rather than silently
/// mislabel rows written under the other one.
/// Parse a stored fact for the wire, and never substitute a MEANINGFUL value
/// when the parse fails.
///
/// Three call sites did this and two of them agreed. The third substituted
/// `Value::Null`, which in this catalog is not a neutral placeholder: the served
/// contract says a null limit means UNKNOWN CAPACITY and must never be defaulted
/// to a number. So a row this producer failed to parse would arrive at a
/// consumer as a statement fusiform makes deliberately about the upstream —
/// absent and unknown collapsed at the exact seam this catalog exists to keep
/// apart, in the producer.
///
/// The raw string is the honest substitute. It is visibly not a fact object, so
/// a consumer decoding it fails on the shape rather than believing a plausible
/// absence, and the bytes that failed to parse travel with the failure instead
/// of being replaced by a verdict.
///
/// Reaching here at all means a stored row is not the JSON this producer wrote,
/// which is corruption or a representation change rather than upstream data —
/// but the point is what it does when it happens, not how often.
fn fact_for_the_wire(value_json: &str) -> serde_json::Value {
    serde_json::from_str(value_json)
        .unwrap_or_else(|_| serde_json::Value::String(value_json.to_string()))
}

fn name_the_provenance_of_a_pre_0_11_row(value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if obj.get("state").and_then(|s| s.as_str()) != Some("priced") {
        return;
    }
    if obj.contains_key("unit_provenance") {
        return;
    }
    obj.insert(
        "unit_provenance".to_string(),
        serde_json::json!({
            "kind": "assumed_by_policy",
            "policy": fusiform_core::PolicyId::models_dev_usd_v1().0,
        }),
    );
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

    // The overrides in effect right now, from the same snapshot the counts come
    // from, so status cannot disagree with itself about which models exist.
    //
    // Rendered into the wire model map first because that is the shape both
    // this and `catalog.get` compare against — building a second traversal here
    // would be two artifacts of one rule with nothing holding them together.
    let mut wire_models: BTreeMap<String, BTreeMap<String, serde_json::Value>> = BTreeMap::new();
    for model in &snapshot.models {
        wire_models.insert(
            format!("{}/{}", model.provider_id, model.model_id),
            model
                .facts
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), fact_for_the_wire(v)))
                .collect(),
        );
    }
    let overridden = overrides_in_effect(&wire_models, overlay::corrections());

    // Failed polls across the whole history, not just the poll window.
    //
    // The window is ten by default and a failure can sit far outside it, so an
    // operator asking what fusiform has been doing sees ten clean rows and
    // nothing suggesting there is more to see.
    //
    // The DISTANCE is derived per response rather than described in a comment,
    // because it moves with every poll. This comment used to say "about forty
    // polls back"; by 2026-08-17 the real answer was 196.
    let (failures_ever, last_failure_at, last_failure_class) =
        store.failure_history(source).map_err(|e| store_error(&e))?;
    let failures = last_failure_at.map(|at| fusiform_protocol::FailureHistoryWire {
        ever: failures_ever,
        last_at_ms: at.0,
        // The class of the NEWEST failure, which decides where an operator
        // looks: `network` sends them to the upstream, `parse` to the payload,
        // `implausible` to the shrink guard. A count and an age without it says
        // something went wrong and not what.
        last_class: last_failure_class.map(|c| fusiform_store::failure_class_word(c).to_string()),
        polls_back: store.observations_since(source, at).ok(),
    });

    Ok(StatusResponse {
        source: source.as_str().to_string(),
        catalog_version,
        overridden,
        failures,
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
            value: fact_for_the_wire(&row.value_json),
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
        // `"seed"` IS NOT ACCEPTED, and refusing it is the point.
        //
        // `SourceId::Seed` exists in the domain, and no row has ever been
        // written under it: the embedded snapshot is models.dev's own data
        // fetched earlier, so `seed.rs` stores it under `ModelsDev` and marks
        // its provenance with `BoundaryKind::Seed`. Seed-ness is a property of
        // HOW a value was learned, not of WHO said it.
        //
        // This arm used to return `Ok(SourceId::Seed)`. Every query then
        // matched zero rows and returned an EMPTY CATALOG with a success
        // status — "fusiform knows of no models" — which is the same defect as
        // a prefix filter that selects nothing, and the same one the whole
        // store exists to prevent: absence must not be indistinguishable from
        // never-published. Verified against the live store, which holds
        // 73,584 eras and not one under `seed`.
        Some(other) => Err(RouteError::bad_request(format!(
            "unknown source {other:?}; fusiform serves \"models.dev\". \
             A bootstrap snapshot is not a separate source: it is stored under \
             \"models.dev\" with a seed boundary, visible in catalog.history."
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

    // Checked ONCE, before the branch, so both read paths inherit it.
    //
    // The single-model and bulk reads have needed the same property three
    // separate times in this crate and diverged every time it was written
    // twice. Placing this above the split makes the divergence unwritable
    // rather than merely absent.
    refuse_before_the_record(store, source, at)?;

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
    // Must agree with the bulk path's resolution, and the reason is a consumer
    // dependency rather than tidiness.
    //
    // `resolved_at_ms` is what tells a consumer a SHRUNKEN identity set apart
    // from a HISTORICAL one — the requested instant for a point-in-time read,
    // now for a current one. `catalog_version` cannot: it is
    // `max(now_ms, current + 1)`, so a restore raises it while the content goes
    // back, and both cases present as "version rose, identities shrank".
    //
    // This path recomputes rather than reusing, so a mutation neutering it
    // survived a test that drove only the bulk read. The consumer consequence:
    // NARROWING A QUERY TO ONE MODEL WOULD SILENTLY LOSE THE FIELD A
    // COMPLETENESS GUARD BRANCHES ON.
    //
    // ASTRO's disposition for it, which is why this comment says more than "keep
    // these in sync": they consume whole-document snapshots and depend on
    // nothing here today — and would acquire the dependency the moment someone
    // narrows a read for efficiency. That is an optimisation a reviewer
    // approves without noticing, and the acquisition has no diff: nothing about
    // adding `provider_id` to a request says a different function now computes
    // the field a guard reads.
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
            // Whether the model had anything READABLE before the plane filter
            // ran. This separates two emptinesses that look identical in the
            // response and mean opposite things.
            let had_readable_facts = !model.facts.is_empty();
            let facts = match &request.fact_prefixes {
                Some(prefixes) => model
                    .facts
                    .into_iter()
                    .filter(|(k, _)| prefixes.iter().any(|p| k.as_str().starts_with(p)))
                    .collect(),
                None => model.facts,
            };
            // The model is returned even when the FILTER leaves it with no
            // facts, and that is the whole point of this branch.
            //
            // The caller named ONE model. Dropping it because a plane filter
            // matched nothing answers "no such model" to a question about a
            // model that exists — the uninformative zero again, and this time
            // in the plane a pricing consumer reads. 419 of 6,583 present
            // models publish no rate at all (measured 2026-08-15), so
            // `--rates` on any of them returned an empty catalog
            // indistinguishable from a typo.
            //
            // For a ledger the difference is the whole question: a model that
            // is UNPRICED and used means a subscription or a billing gap,
            // while a model that is UNKNOWN and used means a bad id. An empty
            // response cannot tell them apart, and the store can.
            //
            // Note the comment three lines up already guards the neighbouring
            // case — a rates-only request must not make a present model look
            // retired — and this one made a present model look absent.
            //
            // The BULK read keeps dropping them, deliberately, because it
            // answers a different question: "give me the rate plane" is a
            // request for models that have rates, not a census. Here the
            // caller named the subject, so the answer is about that subject.
            //
            // BUT NOT WHEN WITHHOLDING EMPTIED IT, and an existing test caught
            // me conflating the two. If every fact is withheld by a
            // correction, the model's identity is already carried by
            // `withheld`, and an empty entry here would claim "this model has
            // no facts" when the truth is "it has facts fusiform refuses to
            // serve". The emptiness would be reported twice and mean something
            // different each time.
            //
            // So: an empty result AFTER filtering is worth reporting; an empty
            // result BEFORE filtering is already reported elsewhere.
            if had_readable_facts {
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
            let mut value = serde_json::from_str(&raw).unwrap_or_else(|_| {
                // A stored value this build cannot parse. Carried as a string
                // rather than dropped, so the row is visible and reportable
                // instead of presenting as an absent fact.
                serde_json::Value::String(raw.clone())
            });
            name_the_provenance_of_a_pre_0_11_row(&mut value);
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
/// Which corrections currently DIVERGE from what the upstream publishes.
///
/// Reporting only — nothing is mutated. Shares its rule with
/// [`apply_corrections`] by construction: both ask whether the served value
/// differs from the stored one, so `catalog.get` and `catalog.status` cannot
/// disagree about what is being overridden.
///
/// A correction whose row the upstream has since fixed is a no-op and is
/// omitted from both. That is not an error: the cell is redundant rather than
/// wrong, and reporting "1000000 -> 1000000" would train an operator to skim
/// the entries that matter.
fn overrides_in_effect(
    models: &BTreeMap<String, BTreeMap<String, serde_json::Value>>,
    corrections: &overlay::Corrections,
) -> Vec<OverriddenFactWire> {
    let mut out = Vec::new();
    for (identity, facts) in models {
        let Some((provider_id, model_id)) = identity.split_once('/') else {
            continue;
        };
        for (fact_key, value) in facts {
            let key = (
                provider_id.to_string(),
                model_id.to_string(),
                FactKey::from_stored(fact_key.clone()),
            );
            let Some(correction) = corrections.get(&key) else {
                continue;
            };
            let Some(wire) = override_for(value, provider_id, model_id, fact_key, correction)
            else {
                continue;
            };
            out.push(wire);
        }
    }
    out
}

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

            let Some(wire) = override_for(value, provider_id, model_id, fact_key, correction)
            else {
                // Nothing to report AND nothing to apply: the two go together,
                // since a value already equal to the correction needs no write.
                continue;
            };
            applied.push(wire);
            *value = served;
        }
    }

    applied
}

#[cfg(test)]
mod wire_value_tests {
    use super::*;

    /// A row this producer cannot parse must not arrive as a MEANING.
    ///
    /// `null` is load-bearing on this wire: the served contract says a null
    /// limit is unknown capacity and must never be defaulted to a number. So
    /// substituting `Value::Null` for a parse failure hands a consumer a
    /// deliberate-looking statement about the upstream when the truth is that
    /// fusiform failed to read its own stored row — absent and unknown
    /// collapsed in the producer, at the seam this catalog exists to keep apart.
    #[test]
    fn an_unparseable_row_never_becomes_null() {
        let got = fact_for_the_wire("{not json");
        assert!(
            !got.is_null(),
            "a parse failure must not be served as null, which this wire reads \
             as 'the upstream published no value': {got:?}"
        );
        assert_eq!(
            got,
            serde_json::Value::String("{not json".to_string()),
            "the bytes that failed to parse must travel with the failure: {got:?}"
        );

        // CONTROL: a well-formed row still parses to its object, so the
        // assertion above cannot pass by everything becoming a string.
        let ok = fact_for_the_wire(r#"{"state":"priced","units":3000000000}"#);
        assert!(
            ok.is_object(),
            "a parseable row must still become an object, else the failure \
             assertion above is vacuous: {ok:?}"
        );
    }
}
