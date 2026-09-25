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
    CorrectionDetail, HistoryEra, HistoryRequest, HistoryResponse, MarkArtifactRequest,
    MarkArtifactResponse, OverriddenFactWire, PlanAmount, PlanPriceWire, PlanPricesRequest,
    PlanPricesResponse, PollChanges, RetractArtifactRequest, RetractArtifactResponse, StatusPoll,
    StatusRequest, StatusResponse, ToolResponse, UncertainFactWire, WithheldFactWire, TOOLS,
    TOOL_CORRECT, TOOL_GET, TOOL_HISTORY, TOOL_MARK_ARTIFACT, TOOL_PLAN_PRICES,
    TOOL_RETRACT_ARTIFACT, TOOL_STATUS,
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
            code: fusiform_protocol::CODE_BAD_REQUEST,
            message: message.into(),
        }
    }

    /// A well-formed request fusiform has no catalog to answer.
    ///
    /// Distinct from [`Self::bad_request`] because a consumer must act on them
    /// differently, and the old shared code could not tell them apart. An
    /// unknown model, an unknown provider, an instant before the record begins
    /// — none is a caller defect. They are ANSWERS, arrived at deliberately, and
    /// reporting them as client errors sends someone to debug a correct
    /// request.
    ///
    /// These are also the refusals that exist so an empty catalog is never
    /// returned in place of "nobody was watching", so collapsing them into a
    /// caller defect undoes the distinction they were built to make.
    fn no_coverage(message: impl Into<String>) -> Self {
        Self {
            code: fusiform_protocol::CODE_NO_COVERAGE,
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
        TOOL_MARK_ARTIFACT => serve_mark_artifact(store, &args).map(ToolResponse::MarkArtifact),
        TOOL_RETRACT_ARTIFACT => {
            serve_retract_artifact(store, &args).map(ToolResponse::RetractArtifact)
        }
        TOOL_PLAN_PRICES => serve_plan_prices(store, &args).map(ToolResponse::PlanPrices),
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
        // An alias id has no eras because fusiform never observed it: its facts
        // are another row's, served under the alias at read time. Name that
        // row instead of refusing the id as unknown, so a consumer knows whose
        // history to ask for.
        if let Some(alias) = alias_in_effect(
            store,
            source,
            crate::aliases::aliases(),
            &request.provider_id,
            &request.model_id,
        )? {
            return Ok(HistoryResponse {
                source: source.as_str().to_string(),
                provider_id: request.provider_id,
                model_id: request.model_id,
                fact_key: request.fact_key,
                eras: Vec::new(),
                last_changed_at_ms: None,
                inherited_from: Some(alias_origin(store, source, alias)),
                overridden: None,
            });
        }
        if !store
            .provider_is_known(source, &request.provider_id)
            .map_err(|e| store_error(&e))?
        {
            return Err(RouteError::no_coverage(format!(
                "unknown provider {:?}: fusiform has never recorded a model \
                 under that id",
                request.provider_id
            )));
        }
        if !store
            .model_is_known(source, &request.provider_id, &request.model_id)
            .map_err(|e| store_error(&e))?
        {
            return Err(unknown_model(&request.provider_id, &request.model_id));
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

    // Served here rather than left to the consumer because the exclusion it
    // applies is not derivable from the era list above: which polls were
    // fusiform's own representation changes is not in those rows, and a
    // consumer taking the newest boundary reads the whole catalog as freshly
    // maintained.
    //
    // A store read failure renders no field rather than a wrong instant. An
    // absent value says "not established"; a wrong one is an assertion about
    // when a price last moved, which is the kind of claim a caller acts on.
    let last_changed_at_ms = store
        .last_changed_at(source, &request.provider_id, &request.model_id, &fact)
        .ok()
        .flatten()
        .map(|t| t.0);

    // Why an empty history can be the RIGHT answer for a fact that is served.
    //
    // A rate inherited from the creator has no eras on this row -- the value is
    // derived at serve time -- so `catalog.get` serves a price and
    // `catalog.history` finds nothing. Two surfaces disagreeing about whether a
    // fact exists, both correct, which reads as one of them being broken.
    //
    // Answered by running THE SAME derivation `catalog.get` runs, rather than
    // by re-deriving the eligibility rule here. A second copy of that rule
    // would be free to drift from the one that decides what is actually
    // served, and then this surface would explain an absence that is not the
    // one the consumer met.
    //
    // Only consulted when the history is empty: a fact with eras of its own is
    // this provider's, and inheritance never overwrites a published rate.
    let inherited_from = if eras.is_empty() {
        inheritance_origin(
            store,
            source,
            &request.provider_id,
            &request.model_id,
            &fact,
        )
    } else {
        None
    };

    Ok(HistoryResponse {
        source: source.as_str().to_string(),
        provider_id: request.provider_id,
        model_id: request.model_id,
        fact_key: request.fact_key,
        eras,
        last_changed_at_ms,
        inherited_from,
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
        return Err(RouteError::no_coverage(format!(
            // Continuations, not a wrapped literal. Without the trailing \
            // the source indentation becomes part of the string, and this
            // message shipped with two runs of fourteen spaces in it — visible
            // only by reading the SERVED output, never by reading the source,
            // where it looks like ordinary wrapping.
            "no record at {}: fusiform's history begins at {}, so it cannot say \
             what the catalog held before then. An empty answer would claim the \
             catalog was empty; it was unobserved.",
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
        // Read from the contract table rather than collected from the snapshot.
        //
        // Collecting the keys models happen to carry answers "what did the
        // upstream publish", which is the question that misled a consumer into
        // pricing on one rate. It would also make the answer depend on which
        // models exist today: a catalog with no cached-pricing rows would report
        // that fusiform cannot serve `rate.cache_read`, which is false.
        served_facts: fusiform_protocol::SERVED_FACTS
            .iter()
            .map(|f| f.key.to_string())
            .collect(),
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

    let aliases = crate::aliases::aliases();
    let named = request.provider_id.is_some() && request.model_id.is_some();
    if let (Some(provider_id), Some(model_id)) =
        (request.provider_id.as_deref(), request.model_id.as_deref())
    {
        if let Some(alias) = alias_in_effect(store, source, aliases, provider_id, model_id)? {
            return serve_named_alias(
                store,
                source,
                provider_id,
                model_id,
                alias,
                at,
                request.fact_prefixes.as_ref(),
            );
        }
    }

    let (snapshot, retired) = if let (Some(provider_id), Some(model_id)) =
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
                // Widened, then narrowed again after inheritance runs. See
                // `widened_for_inheritance`: the gate's own inputs were being
                // filtered away by the caller's own filter.
                Some(prefixes) => {
                    fusiform_store::serve::FactFilter::Prefixes(widened_for_inheritance(prefixes))
                }
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
                // The hint names the VOCABULARY rather than a casing rule.
                //
                // It read "check the spelling, so anthropic rather than
                // Anthropic" — a rule true of one case and handed to every
                // reader of this refusal. The commonest real cause is not a typo
                // at all: a caller using a DIFFERENT NAMING for the same vendor.
                // Insula calls them `codex` and `claude`, and publishes a
                // separate `apiProvider` field carrying models.dev slugs
                // precisely because the two vocabularies differ. Telling that
                // caller to check their spelling sends them hunting a typo they
                // did not make — which I did to myself against that very field.
                return Err(RouteError::no_coverage(format!(
                    "unknown provider {provider:?}: fusiform has never recorded \
                     a model under that id. Provider ids are models.dev's own — \
                     a DIFFERENT NAMING for the same vendor will not resolve \
                     here, and neither will a different case"
                )));
            }
        }
        // Empty on the bulk path by design, not by omission: the reasoning is
        // on `CatalogGetResponse::retired` — 805 retirements against 7,852
        // present models, permanent in an append-only history.
        (snapshot, Vec::new())
    };

    let mut response = render(
        source,
        snapshot,
        at,
        overlay::corrections(),
        Some((store, crate::creators::creators())),
        request.fact_prefixes.as_ref(),
    );
    // Attached after rendering rather than passed through it: `render` is shared
    // with the bulk path, which has no retirements to report, and threading an
    // always-empty argument through it would invite a future caller to fill it.
    response.retired = retired;
    // Aliases on a bulk read, current view only for the same reason as the
    // named read: fusiform never observed an alias id, so it has no past.
    if !named && at.is_none() {
        add_aliases_to_bulk(
            store,
            source,
            aliases,
            &mut response,
            request.provider_id.as_deref(),
            request.fact_prefixes.as_ref(),
        )?;
    }
    Ok(response)
}

fn single_model_snapshot(
    store: &CatalogStore,
    source: SourceId,
    provider_id: &str,
    model_id: &str,
    at: Option<Timestamp>,
    request: &CatalogGetRequest,
) -> Result<(CatalogSnapshot, Vec<fusiform_protocol::RetiredModelWire>), RouteError> {
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
            return Err(RouteError::no_coverage(format!(
                "unknown provider {provider_id:?}: fusiform has never recorded \
                 a model under that id"
            )));
        }
        if !store
            .model_is_known(source, provider_id, model_id)
            .map_err(|e| store_error(&e))?
        {
            return Err(unknown_model(provider_id, model_id));
        }
        // Known, and absent at this instant: that is a real answer about a real
        // model, so it stays an empty result rather than becoming an error.
    }

    let catalog_version = store.catalog_version().map_err(|e| store_error(&e))?;

    let mut models = Vec::new();
    let mut retired: Vec<fusiform_protocol::RetiredModelWire> = Vec::new();
    if let Some(model) = found {
        // Presence is checked before the fact filter, for the same reason the
        // catalog read checks it first: a rates-only request must not make a
        // present model look retired.
        // Captured rather than merely skipped. A named read that returns an
        // empty `models` is indistinguishable from "your fact filter matched
        // nothing", and a consumer polling presence therefore held a withdrawn
        // model on its roster indefinitely — the absence had no reason attached
        // and none could be inferred.
        if !request.include_retired && !model.is_present() {
            retired.push(fusiform_protocol::RetiredModelWire {
                model: format!("{}/{}", model.provider_id, model.model_id),
                // The instant fusiform NOTICED, which is the boundary of the
                // existence era. The upstream removed it somewhere in the
                // half-hour window ending there; `catalog.history` carries the
                // window for a consumer that needs the interval.
                // Read from the existence fact's own genuine-change instant
                // rather than stamped with "now". One indexed query, and only
                // on the path where a model was actually excluded.
                //
                // `last_changed_at` rather than the newest era boundary,
                // because a poll that restated existence without the upstream
                // changing it would otherwise report today.
                retired_at_ms: store
                    .last_changed_at(
                        source,
                        &model.provider_id,
                        &model.model_id,
                        &fusiform_store::FactKey::existence(),
                    )
                    .ok()
                    .flatten()
                    .map(|t| t.0)
                    .unwrap_or(resolved_at.0),
            });
        }
        let keep = request.include_retired || model.is_present();
        if keep {
            // Whether the model had anything READABLE before the plane filter
            // ran. This separates two emptinesses that look identical in the
            // response and mean opposite things.
            let had_readable_facts = !model.facts.is_empty();
            let facts = match &request.fact_prefixes {
                Some(prefixes) => {
                    // Same widening as the bulk path, for the same reason and
                    // in the same shape. These two filters have diverged three
                    // times in this crate; they share the helper so the rule
                    // has one statement.
                    let wide = widened_for_inheritance(prefixes);
                    model
                        .facts
                        .into_iter()
                        .filter(|(k, _)| wide.iter().any(|p| k.as_str().starts_with(p)))
                        .collect()
                }
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

    Ok((
        CatalogSnapshot {
            source,
            resolved_at,
            catalog_version,
            models,
            withheld,
            uncertain,
        },
        retired,
    ))
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
    inherit: Option<(&CatalogStore, &crate::creators::Creators)>,
    requested_prefixes: Option<&Vec<String>>,
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
        // Inheritance is skipped for point-in-time reads. `at` asks what the
        // catalog HELD at an instant, and an inherited rate is what fusiform
        // derives today from a table that did not exist then -- serving it into
        // a historical answer would put a present-tense derivation inside a
        // past-tense claim. Same rule the overlay overrides already follow.
        if let (Some((store, creators)), None) = (inherit, at) {
            inherit_rate_for(store, source, creators, &mut facts, &model.model_id);
        }
        // Drop what was fetched only to gate the rule above.
        narrow_to_requested(&mut facts, requested_prefixes);
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
        // Filled by the NAMED-read caller, which is the only path that knows a
        // specific model was excluded. A bulk read leaves it empty on purpose:
        // 805 retired models against 7,852 present, permanent in an append-only
        // history, so attaching them to every read would ship hundreds of
        // ancient retirements to callers who asked about none of them.
        retired: Vec::new(),
        // Filled by the alias paths in `serve_catalog_get`, which are the only
        // callers that serve one row's facts under another id.
        aliased: Vec::new(),
    }
}

/// One statement of the unknown-model refusal, for the two routes that ask it.
///
/// `catalog.get` and `catalog.history` reach this from the same predicate —
/// `model_is_known` returned false under a provider that exists — and carried
/// byte-identical copies of the sentence. Operator-facing text duplicated
/// across call sites drifts the moment one copy is improved, and the copy left
/// behind is the one nobody is looking at.
///
/// The hint stays, unlike the provider-level refusal's, because it is GROUNDED
/// rather than remembered: this branch runs only after the store has confirmed
/// the provider is real, so "the provider exists" is something the code
/// established rather than a likely cause recalled from one case.
fn unknown_model(provider_id: &str, model_id: &str) -> RouteError {
    RouteError::no_coverage(format!(
        "unknown model {model_id:?} under provider {provider_id:?}: the provider \
         exists, so check the model id — upstream ids often carry a version suffix"
    ))
}

/// Serve the curated subscription prices.
///
/// Reads at NOW rather than taking an instant, deliberately. A point-in-time
/// read of this plane would be a different contract — "what did we believe on
/// day X" rather than "what is in force" — and nobody has asked for it. Adding
/// it later is additive; guessing at its semantics now is not.
fn serve_plan_prices(store: &CatalogStore, args: &[u8]) -> Result<PlanPricesResponse, RouteError> {
    let request: PlanPricesRequest = serde_json::from_slice(args)
        .map_err(|e| RouteError::bad_request(format!("malformed plan.prices request: {e}")))?;

    let now = now_ms();
    let rows = store
        .plan_prices_at(now, request.provider_id.as_deref())
        .map_err(|e| store_error(&e))?;

    // A named provider with no rows is a REFUSAL, not an empty list.
    //
    // Same reasoning as the catalog's unknown-provider arm: an empty answer
    // reads as "this provider has no subscription pricing", which is a claim
    // about the world, when the truth is that nobody has curated one. The
    // states are different and only one of them is someone's job.
    if rows.is_empty() {
        if let Some(provider) = &request.provider_id {
            return Err(RouteError::no_coverage(format!(
                "no curated subscription prices for provider {provider:?}: fusiform \
                 holds plan prices only for providers someone has sourced, which is \
                 a short list — this is an absence of curation rather than a claim \
                 that the provider has no plans"
            )));
        }
    }

    Ok(PlanPricesResponse {
        prices: rows
            .into_iter()
            .map(|r| PlanPriceWire {
                provider_id: r.provider_id,
                tier: r.tier,
                price: match (r.minor_units, r.exponent, r.currency, r.period) {
                    (Some(minor_units), Some(exponent), Some(currency), Some(period)) => {
                        Some(PlanAmount {
                            minor_units,
                            exponent,
                            currency,
                            period,
                        })
                    }
                    // Any other shape is a refusal row. The store's CHECK makes
                    // partial amounts unrepresentable, so this arm is reached
                    // only by rows that genuinely carry no price.
                    _ => None,
                },
                refusal_reason: r.refusal_reason,
                boundary_at_ms: r.boundary_at_ms,
                established_by: r.established_by,
                established_at_ms: r.established_at_ms,
                review_by_ms: r.review_by_ms,
                source_ref: r.source_ref,
            })
            .collect(),
        resolved_at_ms: now,
        tier_vocabulary: crate::plan_prices::TIER_VOCABULARY.to_string(),
        unit_policy: crate::plan_prices::UNIT_POLICY.to_string(),
    })
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

/// Mark one poll as fusiform changing its own representation.
///
/// # Why this route exists at all
///
/// The store has carried `mark_observation_artifact` since the mechanism
/// landed, and until now nothing in the shipped binary called it — three
/// callers, all tests. So an operator could not mark anything, the exclusion
/// could never fire in production, and `last_changed_at` would report the
/// 2026-08-16 instant for every priced fact forever. That is precisely the
/// defect the field exists to prevent, shipped without the means to prevent it.
///
/// # Why it previews by default
///
/// A mark deletes real history from every derivation that honours it, and it is
/// unfalsifiable afterwards: nothing in the store records what the excluded
/// eras would have said. So it is bounded exactly like `catalog.correct` — one
/// observation named explicitly, never a pattern, and a write only on an
/// explicit `dry_run: false`.
///
/// The preview answers the question an operator actually has, which is whether
/// this id names the poll they mean: the observed instant, how many eras it
/// wrote, and how many of those restate the claim already in force.
pub fn serve_mark_artifact(
    store: &CatalogStore,
    body: &[u8],
) -> Result<MarkArtifactResponse, RouteError> {
    let request: MarkArtifactRequest = serde_json::from_slice(body)
        .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?;

    if request.reason.trim().is_empty() {
        // Same rule as a correction: a mark with no reason is unauditable, and
        // this one is worse — a correction leaves the corrected era in place to
        // argue with, while a mark leaves nothing behind at all.
        return Err(RouteError::bad_request(
            "a mark must carry a reason naming the evidence record".to_string(),
        ));
    }

    // The observation must exist. An id that names nothing is a coverage answer
    // rather than a malformed request: the operator asked a well-formed
    // question about a poll this store does not have.
    let observed_at = store
        .observation_observed_at(request.observation_id)
        .map_err(|e| store_error(&e))?
        .ok_or_else(|| {
            RouteError::no_coverage(format!(
                "no observation {} in this store",
                request.observation_id
            ))
        })?;

    let (restating, total) = store
        .same_claim_era_fraction(request.observation_id)
        .map_err(|e| store_error(&e))?;

    let committed = if request.dry_run {
        false
    } else {
        store
            .mark_observation_artifact(
                request.observation_id,
                request.reason.trim(),
                Timestamp(now_ms()),
            )
            .map_err(|e| store_error(&e))?;
        true
    };

    Ok(MarkArtifactResponse {
        observation_id: request.observation_id,
        committed,
        restating_eras: restating,
        total_eras: total,
        observed_at_ms: observed_at.0,
    })
}

/// Take back a mark on one poll.
///
/// # Why a separate tool rather than a flag
///
/// A retraction carries its own reason and is its own claim. A boolean on the
/// mark call would let the two be confused in a call log, and the question an
/// operator asks afterwards — "who withdrew this and why" — needs an answer
/// that is not the same field holding a different value.
///
/// Previews by default, like every write here. The preview is worth more than
/// on a mark, because it answers whether there is anything live to take back:
/// a retraction against an unmarked poll is a successful call that changes
/// nothing, and reporting that as done would leave an operator believing they
/// had fixed something.
pub fn serve_retract_artifact(
    store: &CatalogStore,
    body: &[u8],
) -> Result<RetractArtifactResponse, RouteError> {
    let request: RetractArtifactRequest = serde_json::from_slice(body)
        .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?;

    if request.reason.trim().is_empty() {
        return Err(RouteError::bad_request(
            "a retraction must carry a reason: it changes what every consumer of \
             last_changed_at reads, and an unexplained change is unauditable"
                .to_string(),
        ));
    }

    if store
        .observation_observed_at(request.observation_id)
        .map_err(|e| store_error(&e))?
        .is_none()
    {
        return Err(RouteError::no_coverage(format!(
            "no observation {} in this store",
            request.observation_id
        )));
    }

    let live = store
        .observation_artifact_reason(request.observation_id)
        .map_err(|e| store_error(&e))?
        .is_some();

    let committed = if request.dry_run || !live {
        // Not written when there is nothing live: a retraction event against an
        // unmarked poll would record a withdrawal of a claim nobody made.
        false
    } else {
        store
            .retract_observation_artifact(
                request.observation_id,
                request.reason.trim(),
                Timestamp(now_ms()),
            )
            .map_err(|e| store_error(&e))?
    };

    Ok(RetractArtifactResponse {
        observation_id: request.observation_id,
        committed,
        had_live_mark: live,
    })
}

#[cfg(test)]
mod inheritance_tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
    use fusiform_core::BoundaryKind;
    use fusiform_store::NewEra;

    fn store() -> (CatalogStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = CatalogStore::open(&StorageDescriptor {
            module_id: "fusiform".to_string(),
            storage_namespace: "default".to_string(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.path().join("store.db").to_string_lossy().to_string(),
            },
        })
        .unwrap();
        (store, dir)
    }

    fn era(provider: &str, model: &str, key: &str, value: &str) -> NewEra {
        NewEra {
            source: SourceId::ModelsDev,
            provider_id: provider.to_string(),
            model_id: model.to_string(),
            fact_key: FactKey::from_stored(key.to_string()),
            value_json: value.to_string(),
            boundary_at: Timestamp(1_000),
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        }
    }

    fn seed(store: &CatalogStore, provider: &str, model: &str, rate: Option<i64>, open: bool) {
        let mut eras = vec![
            era(provider, model, "existence", r#""present""#),
            era(provider, model, "model.family", r#""glm""#),
            era(
                provider,
                model,
                "model.open_weights",
                if open { "true" } else { "false" },
            ),
        ];
        if let Some(u) = rate {
            eras.push(era(
                provider,
                model,
                "rate.input",
                &format!(r#"{{"state":"priced","units":{u},"exponent":9,"currency":"USD"}}"#),
            ));
        }
        store.append_eras(&eras).unwrap();
    }

    /// Add one rate dimension to an existing row.
    fn add_rate(store: &CatalogStore, provider: &str, model: &str, key: &str, value: &str) {
        store
            .append_eras(&[era(provider, model, key, value)])
            .unwrap();
    }

    fn facts_of(
        store: &CatalogStore,
        provider: &str,
        model: &str,
    ) -> BTreeMap<String, serde_json::Value> {
        let (m, _) = store
            .read_model(SourceId::ModelsDev, provider, model, None)
            .unwrap();
        let m = m.expect("the row must exist");
        m.facts
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    serde_json::from_str(v).unwrap_or(serde_json::Value::Null),
                )
            })
            .collect()
    }

    fn creators() -> crate::creators::Creators {
        [("glm".to_string(), "zai".to_string())]
            .into_iter()
            .collect()
    }

    /// An unpriced OPEN-weight row inherits the creator's rate, disclosed.
    #[test]
    fn an_unpriced_open_weight_row_inherits_and_says_whose_price_it_is() {
        let (store, _d) = store();
        seed(&store, "zai", "glm-x", Some(75_000_000), true);
        seed(&store, "ollama-cloud", "glm-x", None, true);

        let mut facts = facts_of(&store, "ollama-cloud", "glm-x");
        // Control: the reseller genuinely publishes nothing, so what follows is
        // about inheritance rather than about the fixture.
        assert!(
            !facts.contains_key("rate.input"),
            "control: the reseller must be unpriced"
        );

        inherit_rate_for(
            &store,
            SourceId::ModelsDev,
            &creators(),
            &mut facts,
            "glm-x",
        );

        let rate = facts.get("rate.input").expect("must inherit");
        assert_eq!(rate["units"], 75_000_000);
        assert_eq!(
            rate["inherited_from"]["provider_id"], "zai",
            "it must say whose price this is, or a consumer cannot tell it \
             from a published one"
        );
    }

    /// Closed weights do not inherit: two providers serving one closed model
    /// are two offerings that share a name, not one artefact.
    #[test]
    fn a_closed_weight_row_does_not_inherit() {
        let (store, _d) = store();
        seed(&store, "zai", "closed-y", Some(50_000_000), false);
        seed(&store, "ollama-cloud", "closed-y", None, false);

        let mut facts = facts_of(&store, "ollama-cloud", "closed-y");
        inherit_rate_for(
            &store,
            SourceId::ModelsDev,
            &creators(),
            &mut facts,
            "closed-y",
        );
        assert!(
            !facts.contains_key("rate.input"),
            "a closed model must stay unpriced even though a creator price exists"
        );
    }

    /// Inheritance carries the creator's base rates and never its mode rates.
    ///
    /// A mode is a way of calling the creator's own endpoint; the reseller may
    /// not offer it at all, so the creator's price for it would read as the
    /// reseller's price for a mode nobody established it has.
    #[test]
    fn inheritance_never_supplies_a_mode_rate() {
        let (store, _d) = store();
        seed(&store, "zai", "glm-m", Some(75_000_000), true);
        add_rate(
            &store,
            "zai",
            "glm-m",
            "rate.input.mode.fast",
            r#"{"state":"priced","units":150000000,"exponent":9,"currency":"USD"}"#,
        );
        seed(&store, "ollama-cloud", "glm-m", None, true);

        let mut facts = facts_of(&store, "ollama-cloud", "glm-m");
        inherit_rate_for(
            &store,
            SourceId::ModelsDev,
            &creators(),
            &mut facts,
            "glm-m",
        );

        // Control: inheritance did fire, so the absence below is the filter.
        assert_eq!(facts["rate.input"]["units"], 75_000_000);
        assert!(
            !facts.contains_key("rate.input.mode.fast"),
            "a mode rate must never be inherited: {facts:?}"
        );
    }

    /// A row that publishes only mode rates has a price card, so it does not
    /// inherit base rates from the creator.
    #[test]
    fn a_row_with_only_mode_rates_does_not_inherit() {
        let (store, _d) = store();
        seed(&store, "zai", "glm-n", Some(75_000_000), true);
        seed(&store, "ollama-cloud", "glm-n", None, true);
        add_rate(
            &store,
            "ollama-cloud",
            "glm-n",
            "rate.input.mode.fast",
            r#"{"state":"priced","units":90000000,"exponent":9,"currency":"USD"}"#,
        );

        let mut facts = facts_of(&store, "ollama-cloud", "glm-n");
        inherit_rate_for(
            &store,
            SourceId::ModelsDev,
            &creators(),
            &mut facts,
            "glm-n",
        );

        assert!(
            !facts.contains_key("rate.input"),
            "a row publishing a mode rate publishes a price card and must not \
             have its base filled from the creator: {facts:?}"
        );
        assert_eq!(
            facts["rate.input.mode.fast"]["units"], 90_000_000,
            "its own mode rate stays exactly as published"
        );
    }

    /// A published rate is never replaced. This fills a hole; it does not
    /// correct anyone's price.
    #[test]
    fn a_published_rate_is_left_exactly_as_published() {
        let (store, _d) = store();
        seed(&store, "zai", "glm-x", Some(75_000_000), true);
        seed(&store, "ollama-cloud", "glm-x", Some(9_000_000), true);

        let mut facts = facts_of(&store, "ollama-cloud", "glm-x");
        inherit_rate_for(
            &store,
            SourceId::ModelsDev,
            &creators(),
            &mut facts,
            "glm-x",
        );
        assert_eq!(
            facts["rate.input"]["units"], 9_000_000,
            "the reseller's own price must survive"
        );
        assert!(
            facts["rate.input"].get("inherited_from").is_none(),
            "and must not be marked as inherited"
        );
    }

    /// Every dimension the creator publishes is inherited, not a chosen two.
    ///
    /// Measured before writing this: all 11 rows that inherit today have a
    /// creator publishing more than input and output — a cache read price,
    /// usually. Carrying two of four served a partial picture with nothing
    /// marking it partial, and in this catalog an absent rate means "nobody
    /// published this", which was false for exactly those dimensions.
    #[test]
    fn every_rate_dimension_the_creator_publishes_is_inherited() {
        let (store, _d) = store();
        seed(&store, "zai", "glm-x", Some(75_000_000), true);
        add_rate(
            &store,
            "zai",
            "glm-x",
            "rate.output",
            r#"{"state":"priced","units":250000000,"exponent":9,"currency":"USD"}"#,
        );
        add_rate(
            &store,
            "zai",
            "glm-x",
            "rate.cache_read",
            r#"{"state":"priced","units":15000000,"exponent":9,"currency":"USD"}"#,
        );
        // A published ZERO is a published price, and must travel like one.
        add_rate(
            &store,
            "zai",
            "glm-x",
            "rate.cache_write",
            r#"{"state":"stated_zero"}"#,
        );
        seed(&store, "ollama-cloud", "glm-x", None, true);

        let mut facts = facts_of(&store, "ollama-cloud", "glm-x");
        inherit_rate_for(
            &store,
            SourceId::ModelsDev,
            &creators(),
            &mut facts,
            "glm-x",
        );

        for key in [
            "rate.input",
            "rate.output",
            "rate.cache_read",
            "rate.cache_write",
        ] {
            let v = facts.get(key).unwrap_or_else(|| {
                panic!(
                    "{key} must be inherited: the creator publishes it, and serving it \
                     absent would tell a consumer nobody published a price that \
                     somebody did. Got: {facts:?}"
                )
            });
            assert_eq!(
                v["inherited_from"]["provider_id"], "zai",
                "{key} must say whose price it is"
            );
        }
        assert_eq!(
            facts["rate.cache_write"]["state"], "stated_zero",
            "a published zero must arrive as stated_zero rather than being dropped \
             for not being `priced`"
        );
    }

    /// A provider that published ANY rate keeps its own card, untouched.
    ///
    /// # The production defect this pins
    ///
    /// The gate asked "is rate.input priced?" as a proxy for "does this
    /// provider publish a price card?". `stated_zero` is a PUBLISHED price --
    /// the provider saying a dimension is free -- so a subscription provider
    /// stating every dimension zero read as unpriced, and inheritance filled
    /// the dimensions around it from someone else's card:
    ///
    ///     alibaba-token-plan/deepseek-v4-flash
    ///       input/output/cache_read/cache_write  stated_zero
    ///       reasoning                            priced 600000000  <- INHERITED
    ///
    /// A router ranks that model as costing money for reasoning; a ledger
    /// records a charge that does not exist. Worse than the gap inheritance was
    /// built to fill, because an absent rate is visibly absent and a fabricated
    /// one is indistinguishable from a real price.
    ///
    /// A partial card is still a card: what is missing from it is the
    /// provider's statement about what they do not charge for, and it stays
    /// VISIBLY missing rather than being filled from elsewhere.
    #[test]
    fn a_provider_that_published_any_rate_keeps_its_own_card() {
        let (store, _d) = store();
        seed(&store, "zai", "glm-x", Some(75_000_000), true);
        add_rate(
            &store,
            "zai",
            "glm-x",
            "rate.reasoning",
            r#"{"state":"priced","units":600000000,"exponent":9,"currency":"USD"}"#,
        );

        // The subscription shape: every dimension published, all free.
        seed(&store, "ollama-cloud", "glm-x", None, true);
        for key in ["rate.input", "rate.output"] {
            add_rate(
                &store,
                "ollama-cloud",
                "glm-x",
                key,
                r#"{"state":"stated_zero"}"#,
            );
        }

        let mut facts = facts_of(&store, "ollama-cloud", "glm-x");
        // Control: the fixture really does state zero rather than omitting.
        assert_eq!(
            facts["rate.input"]["state"], "stated_zero",
            "control: a stated zero must be present, or this tests nothing"
        );

        inherit_rate_for(
            &store,
            SourceId::ModelsDev,
            &creators(),
            &mut facts,
            "glm-x",
        );

        assert_eq!(
            facts["rate.input"]["state"], "stated_zero",
            "a published zero must survive"
        );
        assert!(
            !facts.contains_key("rate.reasoning"),
            "and NO dimension may be imported beside it: the provider published \
             a card, and a dimension absent from it is their statement rather \
             than a hole to fill. Got: {facts:?}"
        );
    }

    /// A partial card is still a card.
    ///
    /// Separated from the case above because the shapes differ and only one of
    /// them existed in the catalog: a provider publishing output and nothing
    /// else. Under the old gate its missing input was filled from the creator,
    /// producing a blended card belonging to nobody.
    #[test]
    fn a_partial_card_is_not_topped_up_from_the_creator() {
        let (store, _d) = store();
        seed(&store, "zai", "glm-x", Some(75_000_000), true);

        seed(&store, "ollama-cloud", "glm-x", None, true);
        add_rate(
            &store,
            "ollama-cloud",
            "glm-x",
            "rate.output",
            r#"{"state":"priced","units":4000000,"exponent":9,"currency":"USD"}"#,
        );

        let mut facts = facts_of(&store, "ollama-cloud", "glm-x");
        inherit_rate_for(
            &store,
            SourceId::ModelsDev,
            &creators(),
            &mut facts,
            "glm-x",
        );

        assert_eq!(
            facts["rate.output"]["units"], 4_000_000,
            "the provider's own price survives"
        );
        assert!(
            !facts.contains_key("rate.input"),
            "and the dimension they did not publish stays VISIBLY absent rather \
             than being filled from another provider's card"
        );
    }

    /// History explains an inherited rate instead of denying it.
    ///
    /// # The disagreement this pins
    ///
    /// `catalog.get` serves a price on this row; `catalog.history` for the
    /// same fact found no eras and rendered "check the fact key" — sending an
    /// operator to hunt a typo in a key that had just been answered. Two
    /// surfaces disagreeing about whether a fact exists, both correct.
    ///
    /// Found by driving the shipped CLI against production, not by a test.
    #[test]
    fn history_names_where_an_inherited_rate_comes_from() {
        let (store, _d) = store();
        seed(&store, "zai", "glm-x", Some(75_000_000), true);
        seed(&store, "ollama-cloud", "glm-x", None, true);

        let origin = inheritance_origin(
            &store,
            SourceId::ModelsDev,
            "ollama-cloud",
            "glm-x",
            &FactKey::from_stored("rate.input".to_string()),
        )
        .expect("an inherited rate must name its origin");
        assert_eq!(origin.provider_id, "zai");

        // The creator's OWN row publishes the rate, so its history is real and
        // must not be explained away as inherited.
        assert!(
            inheritance_origin(
                &store,
                SourceId::ModelsDev,
                "zai",
                "glm-x",
                &FactKey::from_stored("rate.input".to_string()),
            )
            .is_none(),
            "a provider's own published rate must not report an origin"
        );

        // A non-rate fact is never inherited, so it must not acquire an
        // explanation either — without this the helper could answer for every
        // empty history in the catalog.
        assert!(
            inheritance_origin(
                &store,
                SourceId::ModelsDev,
                "ollama-cloud",
                "glm-x",
                &FactKey::from_stored("limit.context".to_string()),
            )
            .is_none(),
            "only rates are inherited"
        );
    }

    /// The ROUTE must carry the explanation, not just the helper.
    ///
    /// Testing `inheritance_origin` directly cannot see a route that never
    /// calls it — a mutation replacing the route's whole branch with `None`
    /// SURVIVED the helper test. That is the same gap that produced the
    /// `--rates` defect: the function was right and the route feeding it was
    /// wrong, and a function tested through its own front door cannot see its
    /// caller.
    #[test]
    fn the_history_route_serves_the_origin() {
        let (store, _d) = store();
        seed(&store, "zai", "glm-x", Some(75_000_000), true);
        seed(&store, "ollama-cloud", "glm-x", None, true);

        let response = serve_history(
            &store,
            br#"{"provider_id":"ollama-cloud","model_id":"glm-x","fact_key":"rate.input"}"#,
        )
        .expect("the request must be served");

        assert!(
            response.eras.is_empty(),
            "control: this row genuinely has no eras, which is why the \
             explanation is needed"
        );
        let origin = response.inherited_from.expect(
            "the route must explain an empty history that is served by \
             inheritance, or an operator is told to check a fact key that \
             catalog.get just answered",
        );
        assert_eq!(origin.provider_id, "zai");

        // Control: a row with real history must NOT carry the explanation, or
        // this passes against a route that attaches it unconditionally.
        let creator = serve_history(
            &store,
            br#"{"provider_id":"zai","model_id":"glm-x","fact_key":"rate.input"}"#,
        )
        .expect("the request must be served");
        assert!(!creator.eras.is_empty(), "control: the creator has history");
        assert!(
            creator.inherited_from.is_none(),
            "a published rate's history must not be explained as inherited"
        );
    }

    /// The BULK path must inherit under a rate filter too.
    ///
    /// The test below drives the single-model path (provider AND model named).
    /// A pricing consumer reading a whole provider takes the bulk path, and a
    /// mutation removing the widening from THAT call site survived — so the two
    /// paths were never compared under a filter.
    #[test]
    fn the_bulk_path_inherits_under_a_rate_filter() {
        let (store, _d) = store();
        seed(&store, "zai", "glm-x", Some(75_000_000), true);
        seed(&store, "ollama-cloud", "glm-x", None, true);

        let bulk = serve_catalog_get(
            &store,
            br#"{"provider_id":"ollama-cloud","fact_prefixes":["rate."]}"#,
        )
        .expect("the request must be served");

        let facts = bulk
            .models
            .get("ollama-cloud/glm-x")
            .expect("the model must be present in a bulk read");
        assert!(
            facts.contains_key("rate.input"),
            "a bulk read with a rate filter must inherit exactly as the \
             single-model read does: a pricing consumer reading a whole \
             provider is the ordinary case. Got: {facts:?}"
        );

        // Control: the single-model path on the same store, so a failure here
        // names WHICH path is wrong rather than saying inheritance is broken.
        let single = serve_catalog_get(
            &store,
            br#"{"provider_id":"ollama-cloud","model_id":"glm-x","fact_prefixes":["rate."]}"#,
        )
        .expect("the request must be served");
        assert!(
            single.models["ollama-cloud/glm-x"].contains_key("rate.input"),
            "control: the single-model path must still inherit"
        );
    }

    /// A rate filter must not switch inheritance off.
    ///
    /// # This is the test that was missing, and production proved it
    ///
    /// The gates read `model.open_weights` and `model.family` from the fact map
    /// the caller receives. `--rates` is exactly what a PRICING consumer
    /// passes, and it filtered the gate's own inputs away, so the rule silently
    /// did not fire. Verified against the running binary two days after
    /// placement: the same model inherited with no filter and came back bare
    /// with `--rates`.
    ///
    /// Every unit test above passed throughout, because they call
    /// `inherit_rate_for` with a map nobody filtered. The defect lived in the
    /// seam between the function and the route that feeds it — so this test
    /// drives `serve_catalog_get`, which is the only surface a consumer has.
    ///
    /// The failure direction is the bad one: a gate with missing inputs reads
    /// as "not eligible", which is indistinguishable from a model that
    /// correctly does not inherit. The feature looked ABSENT rather than
    /// broken, and absent is what it looked like before it existed.
    #[test]
    fn a_rate_filter_does_not_switch_inheritance_off() {
        let (store, _d) = store();
        // `glm` -> `zai` is in the SHIPPED table, so this drives the real
        // mapping rather than a fixture that agrees with itself.
        seed(&store, "zai", "glm-x", Some(75_000_000), true);
        seed(&store, "ollama-cloud", "glm-x", None, true);

        let filtered = serve_catalog_get(
            &store,
            br#"{"provider_id":"ollama-cloud","model_id":"glm-x","fact_prefixes":["rate."]}"#,
        )
        .expect("the request must be served");

        let facts = filtered
            .models
            .get("ollama-cloud/glm-x")
            .expect("the model must be present");

        let rate = facts.get("rate.input").unwrap_or_else(|| {
            panic!(
                "a rate filter must not switch inheritance off: the gates read \
                 model.* facts, and filtering them away made the rule silently \
                 not fire in production. Got: {facts:?}"
            )
        });
        assert_eq!(rate["units"], 75_000_000);
        assert_eq!(rate["inherited_from"]["provider_id"], "zai");

        // And the widening must not leak: the caller asked for rates, so the
        // facts fetched to gate the rule are dropped again before serving.
        assert!(
            !facts.contains_key("model.family"),
            "a --rates response must carry only rate facts, or fusiform is \
             answering a different question from the one asked: {facts:?}"
        );

        // Control: the same request WITHOUT a filter must also inherit, or this
        // test would pass against a build that inherits only under a filter.
        let unfiltered = serve_catalog_get(
            &store,
            br#"{"provider_id":"ollama-cloud","model_id":"glm-x"}"#,
        )
        .expect("the request must be served");
        assert!(
            unfiltered.models["ollama-cloud/glm-x"].contains_key("rate.input"),
            "control: inheritance must work unfiltered too"
        );
    }

    /// An uncurated family inherits nothing: staying unpriced is the honest
    /// answer when nobody has established who published the weights.
    #[test]
    fn an_uncurated_family_inherits_nothing() {
        let (store, _d) = store();
        seed(&store, "zai", "glm-x", Some(75_000_000), true);
        seed(&store, "ollama-cloud", "glm-x", None, true);

        let mut facts = facts_of(&store, "ollama-cloud", "glm-x");
        inherit_rate_for(
            &store,
            SourceId::ModelsDev,
            &crate::creators::Creators::new(),
            &mut facts,
            "glm-x",
        );
        assert!(
            !facts.contains_key("rate.input"),
            "no curated creator means no inheritance"
        );
    }
}

/// Curated aliases, driven through `serve_tool_call` with the SHIPPED table.
///
/// Through the route rather than the helpers, so a route that stops calling
/// the alias logic reddens a test: a helper test passes against a correct
/// helper nobody calls, which is how the `--rates` inheritance defect above
/// shipped.
#[cfg(test)]
mod alias_tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
    use fusiform_core::BoundaryKind;
    use fusiform_store::NewEra;

    const ALIAS: &str = "google/antigravity-gemini-3.8-flash";
    const TARGET: &str = "google/gemini-3.8-flash";

    fn store() -> (CatalogStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = CatalogStore::open(&StorageDescriptor {
            module_id: "fusiform".to_string(),
            storage_namespace: "default".to_string(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.path().join("store.db").to_string_lossy().to_string(),
            },
        })
        .unwrap();
        (store, dir)
    }

    fn era_at(model: &str, key: &str, value: &str, at: i64) -> NewEra {
        NewEra {
            source: SourceId::ModelsDev,
            provider_id: "google".to_string(),
            model_id: model.to_string(),
            fact_key: FactKey::from_stored(key.to_string()),
            value_json: value.to_string(),
            boundary_at: Timestamp(at),
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        }
    }

    /// A google row with a family, a limit, reasoning options and a price.
    fn seed_model(store: &CatalogStore, model: &str, units: i64) {
        store
            .append_eras(&[
                era_at(model, "existence", r#""present""#, 1_000),
                era_at(model, "model.family", r#""gemini-flash""#, 1_000),
                era_at(model, "limit.context", "1048576", 1_000),
                era_at(
                    model,
                    "capability.reasoning_options",
                    r#"["low","medium","high"]"#,
                    1_000,
                ),
                era_at(
                    model,
                    "rate.input",
                    &format!(
                        r#"{{"state":"priced","units":{units},"exponent":9,"currency":"USD"}}"#
                    ),
                    1_000,
                ),
            ])
            .unwrap();
    }

    fn get(store: &CatalogStore, args: &str) -> Result<CatalogGetResponse, RouteError> {
        let body = format!(r#"{{"name":"catalog.get","arguments":{args}}}"#);
        match serve_tool_call(store, body.as_bytes())? {
            ToolResponse::Catalog(c) => Ok(c),
            other => panic!("catalog.get must answer with a catalog: {other:?}"),
        }
    }

    fn named(prefixes: &str) -> String {
        format!(r#"{{"provider_id":"google","model_id":"antigravity-gemini-3.8-flash"{prefixes}}}"#)
    }

    #[test]
    fn a_named_alias_read_serves_the_target_under_the_alias_key() {
        let (store, _d) = store();
        seed_model(&store, "gemini-3.8-flash", 300_000_000);

        let got = get(&store, &named("")).expect("an alias id must be served");
        let facts = got
            .models
            .get(ALIAS)
            .unwrap_or_else(|| panic!("the entry must sit under the alias key: {got:?}"));
        assert!(
            !got.models.contains_key(TARGET),
            "the target must not be served under its own key on a read that \
             named the alias: {got:?}"
        );
        assert_eq!(facts["limit.context"], 1_048_576);
        assert_eq!(
            facts["capability.reasoning_options"],
            serde_json::json!(["low", "medium", "high"])
        );
        assert_eq!(facts["model.family"], "gemini-flash");

        let rate = &facts["rate.input"];
        assert_eq!(rate["units"], 300_000_000);
        assert_eq!(
            rate["inherited_from"],
            serde_json::json!({
                "provider_id": "google",
                "model_id": "gemini-3.8-flash",
                "family": "gemini-flash",
                "basis": "alias",
            }),
            "a price through an alias is the target's list price and must say so"
        );

        assert_eq!(
            got.aliased,
            vec![fusiform_protocol::AliasedModelWire {
                model: ALIAS.to_string(),
                target_provider_id: "google".to_string(),
                target_model_id: "gemini-3.8-flash".to_string(),
                source_ref: crate::aliases::aliases()[&(
                    "google".to_string(),
                    "antigravity-gemini-3.8-flash".to_string()
                )]
                    .source_ref
                    .clone(),
            }]
        );

        // CONTROL: the target read directly carries no alias marker, so the
        // marker above is the alias path's doing rather than the store's.
        let direct = get(
            &store,
            r#"{"provider_id":"google","model_id":"gemini-3.8-flash"}"#,
        )
        .unwrap();
        assert!(direct.aliased.is_empty());
        assert!(direct.models[TARGET]["rate.input"]
            .get("inherited_from")
            .is_none());
    }

    #[test]
    fn a_real_row_for_the_alias_id_wins() {
        let (store, _d) = store();
        seed_model(&store, "gemini-3.8-flash", 300_000_000);
        seed_model(&store, "antigravity-gemini-3.8-flash", 7);

        let got = get(&store, &named("")).unwrap();
        assert!(
            got.aliased.is_empty(),
            "a real row must not be disclosed as aliased: {got:?}"
        );
        let rate = &got.models[ALIAS]["rate.input"];
        assert_eq!(rate["units"], 7, "the real row's own price must be served");
        assert!(rate.get("inherited_from").is_none());

        // The bulk read must not add a second entry for it either.
        let bulk = get(&store, r#"{"provider_id":"google"}"#).unwrap();
        assert_eq!(bulk.models[ALIAS]["rate.input"]["units"], 7);
        assert!(
            !bulk.aliased.iter().any(|a| a.model == ALIAS),
            "{:?}",
            bulk.aliased
        );
    }

    #[test]
    fn an_absent_target_serves_nothing_and_names_it() {
        let (store, _d) = store();
        // The provider exists, so this cannot pass by refusing the provider.
        seed_model(&store, "gemini-3.7-flash", 1);

        let err = get(&store, &named("")).expect_err("an alias of nothing must refuse");
        assert_eq!(err.code, fusiform_protocol::CODE_NO_COVERAGE);
        assert!(err.message.contains(TARGET), "{}", err.message);

        let bulk = get(&store, r#"{"provider_id":"google"}"#).unwrap();
        assert!(!bulk.models.contains_key(ALIAS), "{bulk:?}");
        assert!(!bulk.aliased.iter().any(|a| a.model == ALIAS));
    }

    #[test]
    fn a_retired_target_serves_nothing() {
        let (store, _d) = store();
        seed_model(&store, "gemini-3.8-flash", 1);
        seed_model(&store, "gemini-3.7-flash", 1);
        store
            .append_eras(&[era_at(
                "gemini-3.8-flash",
                "existence",
                r#""absent""#,
                2_000,
            )])
            .unwrap();

        let err = get(&store, &named("")).expect_err("an alias of a retired row must refuse");
        assert_eq!(err.code, fusiform_protocol::CODE_NO_COVERAGE);
        assert!(err.message.contains(TARGET), "{}", err.message);

        let bulk = get(&store, r#"{"provider_id":"google"}"#).unwrap();
        assert!(!bulk.models.contains_key(ALIAS), "{bulk:?}");
    }

    #[test]
    fn a_point_in_time_read_never_aliases() {
        let (store, _d) = store();
        seed_model(&store, "gemini-3.8-flash", 1);

        let err = get(&store, &named(r#","at_ms":1500"#))
            .expect_err("an at read of an alias id must refuse");
        assert_eq!(err.code, fusiform_protocol::CODE_NO_COVERAGE);
        assert!(
            err.message.contains("current reads only"),
            "{}",
            err.message
        );

        let bulk = get(&store, r#"{"provider_id":"google","at_ms":1500}"#).unwrap();
        assert!(bulk.aliased.is_empty(), "{bulk:?}");
        assert!(!bulk.models.contains_key(ALIAS));
    }

    #[test]
    fn a_bulk_read_includes_the_alias() {
        let (store, _d) = store();
        seed_model(&store, "gemini-3.8-flash", 300_000_000);

        for args in [r#"{}"#, r#"{"provider_id":"google"}"#] {
            let bulk = get(&store, args).unwrap();
            let facts = bulk
                .models
                .get(ALIAS)
                .unwrap_or_else(|| panic!("{args}: the alias must be in a bulk read: {bulk:?}"));
            assert_eq!(facts["rate.input"]["inherited_from"]["basis"], "alias");
            assert!(
                bulk.models.contains_key(TARGET),
                "the target keeps its own entry"
            );
            assert_eq!(
                bulk.aliased
                    .iter()
                    .map(|a| a.model.as_str())
                    .collect::<Vec<_>>(),
                vec![ALIAS],
                "{args}: only aliases whose target is present are disclosed"
            );
        }

        // A provider filter that is not the alias's provider excludes it.
        seed_model(&store, "gemini-3.7-flash", 1);
        store
            .append_eras(&[NewEra {
                provider_id: "anthropic".to_string(),
                ..era_at("claude-x", "existence", r#""present""#, 1_000)
            }])
            .unwrap();
        let other = get(&store, r#"{"provider_id":"anthropic"}"#).unwrap();
        assert!(other.aliased.is_empty(), "{other:?}");
    }

    #[test]
    fn a_rate_filter_still_gets_rates_through_the_alias() {
        let (store, _d) = store();
        seed_model(&store, "gemini-3.8-flash", 300_000_000);

        let got = get(&store, &named(r#","fact_prefixes":["rate."]"#)).unwrap();
        let facts = &got.models[ALIAS];
        assert_eq!(facts["rate.input"]["units"], 300_000_000);
        assert_eq!(
            facts["rate.input"]["inherited_from"]["family"], "gemini-flash",
            "the family is read from model.* facts, which the filter must not \
             take away from the alias path"
        );
        assert!(
            facts.keys().all(|k| k.starts_with("rate.")),
            "the widening must not leak into a --rates answer: {facts:?}"
        );

        let bulk = get(
            &store,
            r#"{"provider_id":"google","fact_prefixes":["rate."]}"#,
        )
        .unwrap();
        assert_eq!(
            bulk.models[ALIAS]["rate.input"]["inherited_from"]["family"],
            "gemini-flash"
        );
    }

    #[test]
    fn history_of_an_alias_names_the_target() {
        let (store, _d) = store();
        seed_model(&store, "gemini-3.8-flash", 1);

        for fact in ["rate.input", "limit.context"] {
            let body = format!(
                r#"{{"name":"catalog.history","arguments":{{"provider_id":"google","model_id":"antigravity-gemini-3.8-flash","fact_key":"{fact}"}}}}"#
            );
            let ToolResponse::History(h) = serve_tool_call(&store, body.as_bytes())
                .unwrap_or_else(|e| panic!("{fact}: history of an alias id must answer: {e:?}"))
            else {
                panic!("history must answer with a history");
            };
            assert!(h.eras.is_empty(), "fusiform never observed the alias id");
            let origin = h.inherited_from.expect("the target must be named");
            assert_eq!(origin.provider_id, "google");
            assert_eq!(origin.model_id.as_deref(), Some("gemini-3.8-flash"));
            assert_eq!(origin.basis, "alias");
            assert_eq!(origin.family, "gemini-flash");
        }
    }

    /// A rate the target itself inherited keeps the marker naming the real
    /// origin, rather than being restamped as the target's own price.
    #[test]
    fn a_target_rate_that_was_already_inherited_keeps_its_marker() {
        let alias = crate::aliases::Alias {
            target_provider_id: "ollama-cloud".to_string(),
            target_model_id: "glm-x".to_string(),
            basis: "b".to_string(),
            source_ref: "s".to_string(),
            established_at_ms: 1,
            review_by_ms: 2,
        };
        let original =
            serde_json::json!({"provider_id":"zai","family":"glm","basis":"open_weights"});
        let mut facts = BTreeMap::from([
            ("model.family".to_string(), serde_json::json!("glm")),
            (
                "rate.input".to_string(),
                serde_json::json!({"state":"priced","units":1,"inherited_from": original.clone()}),
            ),
            (
                "rate.output".to_string(),
                serde_json::json!({"state":"priced","units":2}),
            ),
        ]);
        mark_rates_borrowed_through_alias(&mut facts, &alias);
        assert_eq!(facts["rate.input"]["inherited_from"], original);
        assert_eq!(facts["rate.output"]["inherited_from"]["basis"], "alias");
        assert!(
            facts["model.family"].get("inherited_from").is_none(),
            "only rates are marked"
        );
    }
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

/// Does this prefix filter admit rate facts?
///
/// Both directions, because a caller may name a prefix BROADER than `rate.`
/// (`""` for everything) or NARROWER (`rate.input`), and either admits rates.
fn admits_rates(prefixes: &[String]) -> bool {
    prefixes.iter().any(|p| {
        p.starts_with(fusiform_store::prefix::RATE)
            || fusiform_store::prefix::RATE.starts_with(p.as_str())
    })
}

/// Widen a caller's prefix filter to carry the facts inheritance is gated on.
///
/// # The defect this closes, found in production
///
/// Inheritance is gated on `model.open_weights` and `model.family`. Those gates
/// read the fact map the caller receives — so `--rates`, which is precisely
/// what a PRICING consumer passes, filtered the gate's own inputs away and the
/// rule silently did not fire. Verified against the running binary: the same
/// model inherited correctly with no filter and returned bare with `--rates`.
///
/// The failure is silent in the worst direction. A gate whose inputs are
/// missing reads as "not eligible", which is indistinguishable from a model
/// that genuinely should not inherit, so the feature looked absent rather than
/// broken — and absent is exactly what it looked like before it was built.
///
/// Widening the QUERY rather than re-reading per model keeps this at zero extra
/// round-trips: the facts are fetched with the rest and dropped again in
/// [`narrow_to_requested`] if the caller did not ask for them.
fn widened_for_inheritance(prefixes: &[String]) -> Vec<String> {
    let mut out = prefixes.to_vec();
    if admits_rates(prefixes) && !out.iter().any(|p| p == fusiform_store::prefix::MODEL) {
        out.push(fusiform_store::prefix::MODEL.to_string());
    }
    out
}

/// Drop the facts that were fetched only to gate inheritance.
///
/// The caller's filter is a statement about what they want to READ, and
/// widening it was fusiform's implementation detail. Serving the extra keys
/// would make a `--rates` response carry non-rate facts, which is a different
/// answer to the question that was asked.
fn narrow_to_requested(
    facts: &mut BTreeMap<String, serde_json::Value>,
    requested: Option<&Vec<String>>,
) {
    let Some(prefixes) = requested else {
        return;
    };
    facts.retain(|k, _| prefixes.iter().any(|p| k.starts_with(p)));
}

/// Whose row this fact is served from, when it is served by inheritance.
///
/// Runs the real derivation over the model's real facts and reads the marker
/// off the result, so this cannot disagree with what `catalog.get` serves. The
/// alternative -- re-testing `open_weights`, the family, the creator table and
/// the creator's published rates here -- is a second statement of the rule,
/// and the two would answer differently the first time either changed.
fn inheritance_origin(
    store: &CatalogStore,
    source: SourceId,
    provider_id: &str,
    model_id: &str,
    fact: &FactKey,
) -> Option<fusiform_protocol::money::InheritedFrom> {
    if !fact.as_str().starts_with(fusiform_store::prefix::RATE) {
        return None;
    }
    let (found, _withheld) = store.read_model(source, provider_id, model_id, None).ok()?;
    let model = found?;

    let mut facts: BTreeMap<String, serde_json::Value> = model
        .facts
        .into_iter()
        .map(|(k, raw)| {
            let v = fact_for_the_wire(&raw);
            (k.as_str().to_string(), v)
        })
        .collect();

    inherit_rate_for(
        store,
        source,
        crate::creators::creators(),
        &mut facts,
        model_id,
    );

    serde_json::from_value(facts.get(fact.as_str())?.get("inherited_from")?.clone()).ok()
}

/// Attach an inherited rate to a model whose serving provider publishes none.
///
/// # The defect this closes
///
/// models.dev keeps an open-weight model's list price on the ORIGINATOR's
/// provider entry, not on the reseller's. A consumer reading an absent rate as
/// maximum cost ranks every such model last — which is how a router lost every
/// ollama-cloud selection to a paid model even though a subscription made those
/// calls free.
///
/// # Why the value is disclosed rather than blended
///
/// The inherited number is one provider's list price standing in for another's
/// row, and the two genuinely differ: across open-weight model ids priced under
/// more than one provider, the median spread is 1.39x and the worst measured is
/// 29.2x. A consumer ranking relatively can use it; one that billed from it
/// would be wrong. So it arrives under `inherited_from`, with `candidates` and
/// the observed min/max, and a consumer that ignores the marker still gets a
/// usable number rather than a decode error.
///
/// # Why `open_weights` gates it
///
/// Two providers serving the same OPEN weights are serving the same artefact,
/// so one's price says something about the other's. Two providers serving a
/// closed model are two distinct commercial offerings that happen to share a
/// name, and inheriting between them would assert a relationship that does not
/// exist.
fn inherit_rate_for(
    store: &CatalogStore,
    source: SourceId,
    creators: &crate::creators::Creators,
    facts: &mut BTreeMap<String, serde_json::Value>,
    model_id: &str,
) {
    // Only rows that publish NO RATE AT ALL.
    //
    // This asked "is rate.input priced?" as a proxy for "does this provider
    // publish a price card?", and the proxy is wrong in the direction that
    // fabricates charges. `stated_zero` IS a published price -- it is the
    // provider saying this dimension is free -- and treating it as absence let
    // inheritance fill the dimensions around it from someone else's card.
    //
    // Found in production, one poll after placement:
    //
    //     alibaba-token-plan/deepseek-v4-flash
    //       rate.input        stated_zero
    //       rate.output       stated_zero
    //       rate.cache_read   stated_zero
    //       rate.cache_write  stated_zero
    //       rate.reasoning    priced 600000000   <- INHERITED from deepseek
    //
    // A subscription provider states every dimension free, and fusiform
    // attached a foreign $0.60/Mtok reasoning charge to it. A router ranks that
    // model as costing money; a ledger records a charge that does not exist. It
    // is worse than the gap it was built to fill, because an absent rate is
    // visibly absent while a fabricated one is indistinguishable from a real
    // price.
    //
    // A provider that has published ANY rate fact has a price card, and the
    // dimensions missing from it are their statement about what they do not
    // charge for -- not an invitation to import a number from elsewhere. So the
    // gate is now the honest form of what the rule claims: inheritance is for
    // rows where the upstream published nothing at all.
    //
    // This keeps the case it was built for: ollama-cloud/glm-5.3-flash has zero
    // rate facts in the store (measured), which is why its rates went missing
    // rather than arriving as zeros.
    //
    // A mode rate counts as publishing a rate. A row that prices only a mode
    // (five live rows do) has a price card that simply states no base price,
    // and filling its base dimensions from another provider would be the same
    // top-up the stated_zero case above describes.
    let publishes_a_rate = facts
        .keys()
        .any(|k| k.starts_with(fusiform_store::prefix::RATE));
    if publishes_a_rate {
        return;
    }

    if facts.get("model.open_weights").and_then(|v| v.as_bool()) != Some(true) {
        return;
    }
    let Some(family) = facts
        .get("model.family")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return;
    };
    let Some(creator) = creators.get(&family) else {
        // No curated row for this family. Staying unpriced is the honest
        // answer: nobody has established who published these weights, and
        // guessing would relate this row to a provider chosen by nothing.
        return;
    };

    let Ok((Some(origin), _withheld)) = store.read_model(source, creator, model_id, None) else {
        return;
    };

    // EVERY rate dimension the creator publishes, not a chosen two.
    //
    // This began as `["rate.input", "rate.output"]` because those are what a
    // router ranks on. Measured against the live store: all 11 rows that
    // inherit have a creator publishing MORE than those two — a cache read
    // price, usually — so every inheriting row was served a partial picture
    // with nothing saying it was partial.
    //
    // A consumer pricing a cached request then sees an inherited input rate and
    // an absent cache rate, and absent means "nobody published this" in this
    // catalog's vocabulary. That reading is false: somebody did publish it, and
    // fusiform declined to carry it for no reason a consumer could see.
    //
    // If the creator's price is a defensible stand-in for one dimension it is a
    // stand-in for all of them, and the rule is easier to state with no
    // exceptions: WHERE THE RESELLER PUBLISHED NOTHING, THE CREATOR'S PRICE
    // STANDS IN, DISCLOSED. Tiered keys ride along under the same rule; none
    // exist among the inheriting rows today, so this is the rule applying
    // uniformly rather than a case anyone has exercised.
    //
    // MODE RATES DO NOT RIDE ALONG. A mode is a way of calling one provider's
    // endpoint (a service tier, a speed flag), and whether a reseller offers
    // that mode at all is something only the reseller's own row can say. The
    // creator's price for a mode the reseller may not have would read as the
    // reseller's price for it.
    let inheritable: Vec<(String, String)> = origin
        .facts
        .iter()
        .filter(|(k, _)| k.as_str().starts_with(fusiform_store::prefix::RATE))
        .filter(|(k, _)| !fusiform_store::rate_key::is_mode_rate(k.as_str()))
        .map(|(k, v)| (k.as_str().to_string(), v.clone()))
        .collect();

    for (key, raw) in inheritable {
        // A key absent from `facts` means the upstream published nothing —
        // and that reading depends on an invariant held in another crate.
        //
        // A fact WITHHELD by a correction is also absent here: the store drops
        // it from the map and reports it in `withheld` instead. If that could
        // happen on this path, inheritance would fill the slot fusiform is
        // actively refusing to answer, and the response would carry both a
        // price and a notice saying that price was withheld — contradictory,
        // with the consumer having no reason to read the notice.
        //
        // It cannot, for a structural reason rather than a lucky one:
        // `affected_until` can never exceed the instant a correction is
        // recorded, so corrected intervals are bounded in the past and never
        // intersect a read at now — and inheritance runs ONLY on current-view
        // reads (see the `at.is_none()` gate in `render`). Withholding appears
        // exactly where inheritance does not run.
        //
        // Fenced in fusiform-store/tests/correction_never_denies_now.rs. If
        // that bound is ever relaxed, this loop needs the withheld key set
        // passed in; nothing here would fail loudly on its own.
        //
        // PER KEY, never once for the whole set.
        //
        // The gate above reads `rate.input` alone, and this loop used to write
        // two keys on the strength of it — so a reseller that published an
        // output price and no input price would have had its OWN price
        // overwritten by the creator's. No row in the catalog has that shape
        // today (measured: 0), which is exactly why it would have gone
        // unnoticed. Overwriting a published price is the one thing this rule
        // promises never to do.
        if facts.contains_key(&key) {
            continue;
        }

        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        // Whatever state the creator published, including `stated_zero`.
        // "The originator states this is free" is a published price and carries
        // the same standing as a number; skipping it would reintroduce the
        // partial picture one level down.
        name_the_provenance_of_a_pre_0_11_row(&mut value);
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                "inherited_from".to_string(),
                serde_json::json!({
                    "provider_id": creator,
                    "family": family,
                    "basis": "open_weights",
                }),
            );
        }
        facts.insert(key, value);
    }
}

/// The curated alias for a named id, when it applies.
///
/// A REAL ROW WINS: if the store has ever recorded the id itself — present or
/// retired — the upstream now publishes it, and serving another row's facts
/// under it would overwrite what the upstream says with what fusiform assumed.
/// The check is on the store rather than on the table, so an alias goes quiet
/// the moment the real row lands, with no edit needed here.
fn alias_in_effect<'a>(
    store: &CatalogStore,
    source: SourceId,
    aliases: &'a crate::aliases::Aliases,
    provider_id: &str,
    model_id: &str,
) -> Result<Option<&'a crate::aliases::Alias>, RouteError> {
    let Some(alias) = aliases.get(&(provider_id.to_string(), model_id.to_string())) else {
        return Ok(None);
    };
    if store
        .model_is_known(source, provider_id, model_id)
        .map_err(|e| store_error(&e))?
    {
        return Ok(None);
    }
    Ok(Some(alias))
}

/// A named `catalog.get` of an alias id.
///
/// Refuses rather than answering empty in both cases it cannot serve, and says
/// which target it would have served, so the caller can ask for that row.
fn serve_named_alias(
    store: &CatalogStore,
    source: SourceId,
    provider_id: &str,
    model_id: &str,
    alias: &crate::aliases::Alias,
    at: Option<Timestamp>,
    prefixes: Option<&Vec<String>>,
) -> Result<CatalogGetResponse, RouteError> {
    let target = format!("{}/{}", alias.target_provider_id, alias.target_model_id);
    if at.is_some() {
        // An alias is today's reading of a route's code, and fusiform never
        // observed the alias id, so there is no past to report under it.
        // Serving the target's past under the alias would claim the route
        // existed and routed there at that instant, which nobody established.
        return Err(RouteError::no_coverage(format!(
            "{provider_id}/{model_id} is a curated alias of {target}, and aliases \
             apply to current reads only: fusiform never observed the alias id, \
             so it has no past. Read {target} at that instant instead"
        )));
    }
    aliased_response(store, source, provider_id, model_id, alias, prefixes)?.ok_or_else(|| {
        RouteError::no_coverage(format!(
            "{provider_id}/{model_id} is a curated alias of {target}, which is \
             absent or retired in the current catalog, so the alias serves \
             nothing"
        ))
    })
}

/// The target's current served facts under the alias key, or `None` when the
/// target is absent or retired.
///
/// Runs the SAME named read the target would get — inheritance, overrides and
/// all — so an alias can never serve something different from what a caller
/// asking for the target directly receives, apart from the rate markers.
///
/// The fact filter is widened exactly as `widened_for_inheritance` widens it:
/// the rate markers name the target's `model.family`, and a `--rates` read
/// would otherwise filter that input away and mark every rate with an empty
/// family. The widening is dropped again before the entry is returned.
fn aliased_response(
    store: &CatalogStore,
    source: SourceId,
    provider_id: &str,
    model_id: &str,
    alias: &crate::aliases::Alias,
    prefixes: Option<&Vec<String>>,
) -> Result<Option<CatalogGetResponse>, RouteError> {
    let target_request = CatalogGetRequest {
        provider_id: Some(alias.target_provider_id.clone()),
        model_id: Some(alias.target_model_id.clone()),
        fact_prefixes: prefixes.cloned(),
        ..CatalogGetRequest::default()
    };
    let (snapshot, retired) = match single_model_snapshot(
        store,
        source,
        &alias.target_provider_id,
        &alias.target_model_id,
        None,
        &target_request,
    ) {
        Ok(read) => read,
        // The target is not in the store at all: the alias has nothing to
        // stand for. Any other failure is a real error and propagates.
        Err(e) if e.code == fusiform_protocol::CODE_NO_COVERAGE => return Ok(None),
        Err(e) => return Err(e),
    };
    if !retired.is_empty() {
        return Ok(None);
    }

    let wide = prefixes.map(|p| widened_for_inheritance(p));
    let mut response = render(
        source,
        snapshot,
        None,
        overlay::corrections(),
        Some((store, crate::creators::creators())),
        wide.as_ref(),
    );
    let target_key = format!("{}/{}", alias.target_provider_id, alias.target_model_id);
    let Some(mut facts) = response.models.remove(&target_key) else {
        return Ok(None);
    };
    mark_rates_borrowed_through_alias(&mut facts, alias);
    narrow_to_requested(&mut facts, prefixes);

    let alias_key = format!("{provider_id}/{model_id}");
    response.models.insert(alias_key.clone(), facts);
    response.aliased.push(fusiform_protocol::AliasedModelWire {
        model: alias_key,
        target_provider_id: alias.target_provider_id.clone(),
        target_model_id: alias.target_model_id.clone(),
        source_ref: alias.source_ref.clone(),
    });
    Ok(Some(response))
}

/// Mark every rate on an aliased entry as the target's, not the route's.
///
/// The price is the target's API list price, which a consumer uses to relate
/// a subscription route to API cost; read as the route's own published price
/// it would bill calls that may cost nothing at the margin.
///
/// A rate the target itself inherited keeps its ORIGINAL marker: that marker
/// names the provider whose price it really is, and restamping it would hide
/// the first hop.
fn mark_rates_borrowed_through_alias(
    facts: &mut BTreeMap<String, serde_json::Value>,
    alias: &crate::aliases::Alias,
) {
    let family = facts
        .get("model.family")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let marker = serde_json::to_value(fusiform_protocol::money::InheritedFrom {
        provider_id: alias.target_provider_id.clone(),
        family,
        basis: "alias".to_string(),
        model_id: Some(alias.target_model_id.clone()),
    })
    .expect("a struct of strings serialises");
    for (key, value) in facts.iter_mut() {
        if !key.starts_with(fusiform_store::prefix::RATE) {
            continue;
        }
        let Some(obj) = value.as_object_mut() else {
            continue;
        };
        if obj.contains_key("inherited_from") {
            continue;
        }
        obj.insert("inherited_from".to_string(), marker.clone());
    }
}

/// Add alias entries to a current bulk read.
///
/// An alias is included when its provider passes the caller's provider filter
/// and its target is present — even when the target's provider is filtered
/// out, because the caller asked about the ALIAS's provider. Like the bulk
/// read itself, an entry the fact filter leaves empty is dropped.
///
/// Disclosures about the target (`overridden`, `withheld`, `uncertain`) are
/// merged once each, since the bulk read may already carry them for the
/// target's own entry.
fn add_aliases_to_bulk(
    store: &CatalogStore,
    source: SourceId,
    aliases: &crate::aliases::Aliases,
    response: &mut CatalogGetResponse,
    provider_filter: Option<&str>,
    prefixes: Option<&Vec<String>>,
) -> Result<(), RouteError> {
    for (provider_id, model_id) in aliases.keys() {
        if provider_filter.is_some_and(|p| p != provider_id) {
            continue;
        }
        let Some(alias) = alias_in_effect(store, source, aliases, provider_id, model_id)? else {
            continue;
        };
        let Some(entry) = aliased_response(store, source, provider_id, model_id, alias, prefixes)?
        else {
            continue;
        };
        let CatalogGetResponse {
            models,
            aliased,
            overridden,
            withheld,
            uncertain,
            ..
        } = entry;
        if models.values().all(|facts| facts.is_empty()) {
            continue;
        }
        response.models.extend(models);
        response.aliased.extend(aliased);
        for o in overridden {
            if !response.overridden.contains(&o) {
                response.overridden.push(o);
            }
        }
        for w in withheld {
            if !response.withheld.contains(&w) {
                response.withheld.push(w);
            }
        }
        for u in uncertain {
            if !response.uncertain.contains(&u) {
                response.uncertain.push(u);
            }
        }
    }
    Ok(())
}

/// The origin `catalog.history` names for an alias id: the target row.
///
/// The family is read from the target's current row so the marker matches
/// the one `catalog.get` puts on the alias's rates.
fn alias_origin(
    store: &CatalogStore,
    source: SourceId,
    alias: &crate::aliases::Alias,
) -> fusiform_protocol::money::InheritedFrom {
    let family = store
        .read_model(
            source,
            &alias.target_provider_id,
            &alias.target_model_id,
            None,
        )
        .ok()
        .and_then(|(found, _)| found)
        .and_then(|model| {
            model
                .facts
                .iter()
                .find(|(k, _)| k.as_str() == "model.family")
                .and_then(|(_, raw)| fact_for_the_wire(raw).as_str().map(str::to_string))
        })
        .unwrap_or_default();
    fusiform_protocol::money::InheritedFrom {
        provider_id: alias.target_provider_id.clone(),
        family,
        basis: "alias".to_string(),
        model_id: Some(alias.target_model_id.clone()),
    }
}
