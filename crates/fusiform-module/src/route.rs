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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use crate::billing_planes::{self, Plane, PlaneKind};
use fusiform_core::{FieldId, SourceId, Timestamp};
use fusiform_curate::curated_disclosure;
use fusiform_curate::{
    self as curate, fact_for_the_wire, name_the_provenance_of_a_pre_0_11_row, narrow_to_requested,
    retain_quality_disclosure,
};
use fusiform_store::correct::{apply_correction, plan_correction};
use fusiform_store::serve::{CatalogQuery, CatalogSnapshot, Presence};
use fusiform_store::{CatalogError, CatalogStore, FactKey};
use serde::Deserialize;

#[cfg(test)]
mod paging_tests;
#[cfg(test)]
mod publisher_tests;
#[cfg(test)]
mod release_import_tests;
#[cfg(test)]
mod serving_regression_tests;

mod dated_model;
mod history;
mod local_overrides;
mod page_plan;
mod quality;
mod quality_efforts;
use page_plan::{serve_planned_page, PageRender, ProviderRange};

#[cfg(test)]
#[path = "route/canonical_link_tests.rs"]
mod canonical_link_tests;

#[cfg(test)]
#[path = "route/model_lineage_tests.rs"]
mod model_lineage_tests;
#[cfg(test)]
#[path = "route/quality_score_tests.rs"]
mod quality_score_tests;

#[cfg(test)]
#[path = "route/curated_withdrawn_tests.rs"]
mod curated_withdrawn_tests;

#[cfg(test)]
#[path = "route/curated_held_tests.rs"]
mod curated_held_tests;

#[cfg(test)]
#[path = "route/curated_history_tests.rs"]
mod curated_history_tests;

// The served contract lives in `fusiform-protocol` so a consumer can compile
// against it without taking this module's store, network or runtime. Re-exported
// here because this file is where the contract is USED, and a reader following
// a route should not have to know which crate the type is declared in.
pub use fusiform_protocol::{
    BillingRuleWire, CatalogGetRequest, CatalogGetResponse, CorrectRequest, CorrectResponse,
    CorrectedFact, CorrectionDetail, HistoryEra, HistoryRequest, HistoryResponse,
    MarkArtifactRequest, MarkArtifactResponse, PlanAmount, PlanPriceWire, PlanPricesRequest,
    PlanPricesResponse, PlaneWire, PollChanges, RetractArtifactRequest, RetractArtifactResponse,
    StatusPoll, StatusRequest, StatusResponse, ToolResponse, UncertainFactWire, WithheldFactWire,
    TOOLS, TOOL_CORRECT, TOOL_GET, TOOL_HISTORY, TOOL_MARK_ARTIFACT, TOOL_PLAN_PRICES,
    TOOL_QUALITY_GRADE, TOOL_QUALITY_GRADES, TOOL_QUALITY_WITHDRAW, TOOL_RETRACT_ARTIFACT,
    TOOL_STATUS,
};
pub use fusiform_protocol::{TOOL_QUALITY_DECOMMISSION, TOOL_QUALITY_MARKS, TOOL_QUALITY_RESTORE};

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
    /// The session's tool preset, forwarded by a carrier outside the
    /// arguments. fusiform serves one surface and no presets, so an absent
    /// preset gets that surface and any named preset is refused (see
    /// `serve_tool_call`). Read explicitly rather than left to serde's
    /// unknown-field tolerance, which would accept the call and silently
    /// ignore the preset: the caller would believe a restriction applied.
    #[serde(default)]
    preset: Option<String>,
}

pub(crate) fn needs_catalog_permit(body: &[u8]) -> bool {
    if body.is_empty() {
        return true;
    }
    serde_json::from_slice::<ToolCall>(body).is_ok_and(|call| {
        match call.name.as_str() {
            TOOL_GET => call
                .arguments
                .get("model_id")
                .and_then(serde_json::Value::as_str)
                .is_none(),
            // These tools count or validate the affected rows against a whole
            // served catalog, including for a dry run.
            TOOL_QUALITY_DECOMMISSION
            | TOOL_QUALITY_RESTORE
            | TOOL_QUALITY_GRADE
            | TOOL_QUALITY_WITHDRAW
            | fusiform_protocol::TOOL_QUALITY_WORKLOAD => true,
            _ => false,
        }
    })
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

    // A provider refuses a preset it does not serve, by name, instead of
    // falling back to a default. fusiform serves none, so every named preset
    // is refused; a call without one gets the only surface there is.
    if let Some(preset) = &call.preset {
        return Err(RouteError::bad_request(format!(
            "fusiform serves no tool presets, so preset {preset:?} is refused; \
             call without a preset to use its one surface"
        )));
    }

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
        TOOL_QUALITY_GRADE => quality::grade(store, &args).map(ToolResponse::QualityWrite),
        fusiform_protocol::TOOL_QUALITY_WORKLOAD => {
            quality_efforts::workload(store, &args).map(ToolResponse::WorkloadEffortWrite)
        }
        TOOL_QUALITY_WITHDRAW => quality::withdraw(store, &args).map(ToolResponse::QualityWrite),
        TOOL_QUALITY_GRADES => quality::grades(store, &args).map(ToolResponse::QualityGrades),
        TOOL_QUALITY_DECOMMISSION => {
            quality::mark(store, &args, true).map(ToolResponse::QualityMarkWrite)
        }
        TOOL_QUALITY_RESTORE => {
            quality::mark(store, &args, false).map(ToolResponse::QualityMarkWrite)
        }
        TOOL_QUALITY_MARKS => quality::marks(store, &args).map(ToolResponse::QualityMarks),
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
    store
        .with_read_snapshot(|| history::serve(store, body))
        .map_err(|e| store_error(&e))?
}

fn upstream_history(
    store: &CatalogStore,
    request: HistoryRequest,
) -> Result<HistoryResponse, RouteError> {
    let source = parse_source(request.source.as_deref())?;
    let fact_key = request
        .fact_key
        .clone()
        .expect("validated history fact key");
    let fact = FactKey::from_stored(fact_key.clone());

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
    let inherited_from = if rows.is_empty() {
        let published = crate::curation::PublishedCuration::load(store, None)
            .map_err(RouteError::unavailable)?;
        let tables = published
            .as_ref()
            .map(|data| data.tables())
            .unwrap_or_else(|| crate::curation::embedded());
        // Same rule as `catalog.get`: a curated row is in effect when upstream
        // does not currently publish the model, including after upstream
        // withdrew it. A fact the withdrawn upstream row recorded still has
        // eras and is answered with them below, unchanged; this branch is
        // reached only for a fact with no eras, such as `model.kind`, which
        // `catalog.get` serves from the curated row.
        if let Some(row) = curated_in_effect(
            store,
            source,
            &request.provider_id,
            &request.model_id,
            tables.curated_models,
        )? {
            if row.facts.contains_key(&fact_key) {
                return Ok(HistoryResponse {
                    source: source.as_str().to_string(),
                    provider_id: request.provider_id,
                    model_id: request.model_id,
                    fact_key: request.fact_key,
                    eras: Vec::new(),
                    curated: Some(curated_disclosure(row)),
                    last_changed_at_ms: None,
                    inherited_from: None,
                    ..HistoryResponse::default()
                });
            }
        }
        // An alias id has no eras because fusiform never observed it: its facts
        // are another row's, served under the alias at read time. Name that
        // row instead of refusing the id as unknown, so a consumer knows whose
        // history to ask for.
        if let Some(alias) = alias_in_effect(
            store,
            source,
            tables.aliases,
            &request.provider_id,
            &request.model_id,
        )? {
            return Ok(HistoryResponse {
                source: source.as_str().to_string(),
                provider_id: request.provider_id,
                model_id: request.model_id,
                fact_key: request.fact_key,
                eras: Vec::new(),
                curated: None,
                last_changed_at_ms: None,
                inherited_from: Some(alias_origin(store, source, alias)),
                ..HistoryResponse::default()
            });
        }
        if !provider_is_served(store, source, &request.provider_id)? {
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
        // which of the three it is by elimination. Inherited rates have no
        // era on this provider, so use the same creator table as catalog.get
        // to explain that case.
        inheritance_origin(
            store,
            source,
            tables.creators,
            &request.provider_id,
            &request.model_id,
            &fact,
        )
    } else {
        None
    };

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

    // Served here rather than left to the consumer because the exclusion it
    // applies is not derivable from the era list above: which polls were
    // fusiform's own representation changes is not in those rows, and a
    // consumer taking the newest boundary reads the whole catalog as freshly
    // maintained.
    //
    // Missing history and a failed lookup are different answers: only the
    // former can omit the field without hiding a store problem.
    let last_changed_at_ms = store
        .last_changed_at(source, &request.provider_id, &request.model_id, &fact)
        .map_err(|e| store_error(&e))?
        .map(|t| t.0);

    Ok(HistoryResponse {
        source: source.as_str().to_string(),
        provider_id: request.provider_id,
        model_id: request.model_id,
        fact_key: request.fact_key,
        eras,
        curated: None,
        last_changed_at_ms,
        inherited_from,
        ..HistoryResponse::default()
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
    refuse_before_begin(
        store
            .record_begins_at(source)
            .map_err(|e| store_error(&e))?,
        Some(asked),
    )
}

fn refuse_before_begin(begins: Option<Timestamp>, at: Option<Timestamp>) -> Result<(), RouteError> {
    let Some(asked) = at else { return Ok(()) };
    let Some(begins) = begins else {
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
    let rates_version = store.rates_version();
    let catalog_version = store.catalog_version().map_err(|e| store_error(&e))?;

    // Count visible existence and rate facts without materializing their values.
    let (model_count, models_priced) = store
        .catalog_counts(source, None)
        .map_err(|e| store_error(&e))?;

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
        store: None,
        bus_published_version: None,
        bus_publish_failures: 0,
        bus_last_error: None,
        catalog_version,
        rates_version,
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
        model_count,
        models_priced: Some(models_priced),
        // The same read `refuse_before_the_record` uses, so the bound a
        // consumer is told and the bound it is refused at are one value.
        history_begins_at_ms: store
            .record_begins_at(source)
            .map_err(|e| store_error(&e))?
            .map(|t| t.0),
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
    serve_catalog_get_with_limits(store, body, CATALOG_PAGE_BYTES, REPLY_FRAME_LIMIT_BYTES)
}

// The supervisor refuses any module reply frame larger than 4 MiB. A 3 MiB
// compact page leaves headroom for the reply's envelope and framing.
const CATALOG_PAGE_BYTES: usize = 3 * 1024 * 1024;
const REPLY_FRAME_LIMIT_BYTES: usize = 4 * 1024 * 1024;

// Only refusals are remembered, never served responses. Curated models, aliases,
// creators, canonical links, wire corrections and billing rules are include_str!
// inputs fixed for this process.
// Not every era write advances `catalog_version`, yet any new era can change
// what a dated read serves, so the key also holds the newest era id: a new era
// makes a remembered refusal stale. The current timestamp's serialized
// width can change without a write, so that width is part of the key too.
static OVERSIZED_CATALOG: Mutex<Option<OversizedCatalog>> = Mutex::new(None);

#[cfg(test)]
thread_local! {
    static CATALOG_RENDER_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, PartialEq, Eq)]
struct WholeCatalogKey {
    store_instance: u64,
    catalog_version: i64,
    latest_era_id: i64,
    latest_model_mark_id: i64,
    source: SourceId,
    at_ms: Option<i64>,
    fact_prefixes: Option<Vec<String>>,
    include_retired: bool,
    frame_limit: usize,
    timestamp_width: usize,
}

impl WholeCatalogKey {
    fn new(
        store: &CatalogStore,
        request: &CatalogGetRequest,
        source: SourceId,
        frame_limit: usize,
    ) -> Result<Option<Self>, RouteError> {
        // Exhaustive: a new request field must be considered for cache identity.
        let CatalogGetRequest {
            source: _,
            at_ms,
            fact_prefixes,
            include_retired,
            model_id,
            provider_id,
            paged,
            page_token: _,
            auth_method,
        } = request;
        if *paged || provider_id.is_some() || model_id.is_some() || auth_method.is_some() {
            return Ok(None);
        }
        let mut fact_prefixes = fact_prefixes.clone();
        if let Some(prefixes) = &mut fact_prefixes {
            prefixes.sort();
            prefixes.dedup();
        }
        Ok(Some(Self {
            store_instance: store.instance_id(),
            catalog_version: store.catalog_version().map_err(|e| store_error(&e))?,
            latest_era_id: store.latest_era_id().map_err(|e| store_error(&e))?,
            latest_model_mark_id: store.latest_model_mark_id().map_err(|e| store_error(&e))?,
            source,
            at_ms: *at_ms,
            fact_prefixes,
            include_retired: *include_retired,
            frame_limit,
            timestamp_width: at_ms.unwrap_or_else(now_ms).to_string().len(),
        }))
    }
}

struct OversizedCatalog {
    key: WholeCatalogKey,
    size: usize,
}

fn oversized_catalog_error(size: usize, frame_limit: usize) -> RouteError {
    RouteError::bad_request(format!(
        "the whole catalog is {size} bytes, over the {frame_limit}-byte reply \
         limit; read it in pages with \"paged\": true and follow \
         next_page_token, or name a provider_id"
    ))
}

#[cfg(test)]
fn serve_catalog_get_with_budget(
    store: &CatalogStore,
    body: &[u8],
    page_budget: usize,
) -> Result<CatalogGetResponse, RouteError> {
    serve_catalog_get_with_limits(store, body, page_budget, REPLY_FRAME_LIMIT_BYTES)
}

fn serve_catalog_get_with_limits(
    store: &CatalogStore,
    body: &[u8],
    page_budget: usize,
    frame_limit: usize,
) -> Result<CatalogGetResponse, RouteError> {
    serve_catalog_get_with_cache(store, body, page_budget, frame_limit, &OVERSIZED_CATALOG)
}

fn serve_catalog_get_with_cache(
    store: &CatalogStore,
    body: &[u8],
    page_budget: usize,
    frame_limit: usize,
    oversized: &Mutex<Option<OversizedCatalog>>,
) -> Result<CatalogGetResponse, RouteError> {
    // An empty body is a request for everything. A consumer calling a read with
    // no arguments should not have to know that `{}` is the incantation.
    let request: CatalogGetRequest = if body.is_empty() {
        CatalogGetRequest::default()
    } else {
        serde_json::from_slice(body)
            .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?
    };

    if !store.is_store_mode() {
        return serve_catalog_get_request(
            store,
            request,
            page_budget,
            frame_limit,
            oversized,
            None,
        );
    }
    // Store releases update upstream and curation together, but rendering uses
    // several connections. Never return a view that straddled a commit.
    for _ in 0..2 {
        let before = store.catalog_version().map_err(|e| store_error(&e))?;
        let result = serve_catalog_get_request(
            store,
            request.clone(),
            page_budget,
            frame_limit,
            oversized,
            None,
        );
        if store.catalog_version().map_err(|e| store_error(&e))? == before {
            return result;
        }
    }
    Err(RouteError::unavailable(
        "published catalog changed during the read; retry",
    ))
}

fn serve_catalog_get_request(
    store: &CatalogStore,
    request: CatalogGetRequest,
    page_budget: usize,
    frame_limit: usize,
    oversized: &Mutex<Option<OversizedCatalog>>,
    page: Option<PageRender<'_>>,
) -> Result<CatalogGetResponse, RouteError> {
    serve_catalog_get_prepared(
        store,
        request,
        page_budget,
        frame_limit,
        oversized,
        page,
        None,
    )
}

fn serve_catalog_get_prepared(
    store: &CatalogStore,
    request: CatalogGetRequest,
    page_budget: usize,
    frame_limit: usize,
    oversized: &Mutex<Option<OversizedCatalog>>,
    page: Option<PageRender<'_>>,
    prepared: Option<&dated_model::DatedModel>,
) -> Result<CatalogGetResponse, RouteError> {
    store
        .with_read_snapshot(|| {
            serve_catalog_get_snapshot(
                store,
                request,
                page_budget,
                frame_limit,
                oversized,
                page,
                prepared,
            )
        })
        .map_err(|e| store_error(&e))?
}

fn serve_catalog_get_snapshot(
    store: &CatalogStore,
    mut request: CatalogGetRequest,
    page_budget: usize,
    frame_limit: usize,
    oversized: &Mutex<Option<OversizedCatalog>>,
    page: Option<PageRender<'_>>,
    prepared: Option<&dated_model::DatedModel>,
) -> Result<CatalogGetResponse, RouteError> {
    let source = parse_source(request.source.as_deref())?;
    if request.page_token.is_some() && !request.paged {
        return Err(RouteError::bad_request("page_token requires paged: true"));
    }
    if request.paged
        && (request.provider_id.is_some()
            || request.model_id.is_some()
            || request.auth_method.is_some())
    {
        return Err(RouteError::bad_request(
            "paged cannot be combined with provider_id, model_id, or auth_method",
        ));
    }
    let cursor = request
        .page_token
        .as_deref()
        .map(parse_page_token)
        .transpose()?;
    if let Some(cursor) = &cursor {
        if request.at_ms != cursor.dated.then_some(cursor.resolved_at_ms) {
            return Err(RouteError::bad_request(
                "repeat page one's at_ms, or omit it if page one omitted it",
            ));
        }
        check_page_version(
            store.catalog_version().map_err(|e| store_error(&e))?,
            cursor,
        )?;
    }

    if request.paged && page.is_none() {
        return serve_planned_page(
            store,
            request,
            source,
            cursor.as_ref(),
            page_budget,
            frame_limit,
            oversized,
        );
    }

    // Select the plane before reading rows. A subscription with no declared
    // billing claim must never receive the API card, even if its provider or
    // model would otherwise produce a different no-coverage refusal.
    let history_sample = prepared.is_some();
    let scoped;
    let published;
    let owned_tables;
    let tables = if let Some(page) = page {
        page.inputs.tables
    } else if let Some(prepared) = prepared {
        let alias_candidate = (!prepared.model_known()).then(|| {
            request
                .provider_id
                .clone()
                .zip(request.model_id.clone())
                .expect("history names a model")
        });
        scoped = crate::curation::PublishedCuration::for_history(
            store,
            request.at_ms.expect("history samples a dated read"),
            alias_candidate,
        )
        .map_err(RouteError::unavailable)?;
        owned_tables = scoped.tables();
        &owned_tables
    } else {
        published = crate::curation::PublishedCuration::load(store, request.at_ms)
            .map_err(RouteError::unavailable)?;
        owned_tables = published
            .as_ref()
            .map(|data| data.tables())
            .unwrap_or_else(|| crate::curation::embedded());
        &owned_tables
    };
    let range = page.map(|page| page.range);
    let plane = select_billing_plane(&request, tables.billing_planes)?;
    // Refusal size describes the served projection, not the support facts fetched below.
    let whole_key = WholeCatalogKey::new(store, &request, source, frame_limit)?;
    let requested_prefixes = request.fact_prefixes.clone();
    request.fact_prefixes = request.fact_prefixes.as_ref().map(|prefixes| {
        let mut wide = prefixes.clone();
        if prefixes.iter().any(|prefix| {
            "capability.cache_lifetimes".starts_with(prefix)
                || "rate.cache_write".starts_with(prefix)
                || prefix.starts_with("rate.cache_write")
        }) {
            // Current and dated TTL prices need all input and cache-write tiers,
            // even when the caller requested only one derived key.
            wide.push("rate.".to_string());
            wide.push("existence".to_string());
        }
        let selects_effort = prefixes.iter().any(|p| curate::selects_effort_facts(p));
        let selects_chat_score = prefixes.iter().any(|p| selects_chat_score(p));
        // All judgments join on the served canonical key. Keep the supporting
        // facts until the join, including when replaying a dated judgment.
        if selects_judgments(prefixes) {
            wide.extend(["model.canonical_id", "existence"].map(str::to_owned));
        }
        if selects_effort {
            wide.push("capability.reasoning_options".to_string());
        }
        if selects_chat_score {
            wide.push("model.type".to_string());
        }
        // A grade-only projection still needs the row facts that make it eligible.
        for fact in curate::TaskGradeRequirement::required_facts_for_prefixes(prefixes) {
            if !wide.iter().any(|prefix| fact.starts_with(prefix)) {
                wide.push(fact.to_owned());
            }
        }
        wide
    });
    if request.at_ms.is_none() {
        request.fact_prefixes = request.fact_prefixes.as_ref().map(|prefixes| {
            let mut wide = prefixes.clone();
            if admits_rates(prefixes)
                || crate::speed_twins::KEYS
                    .iter()
                    .any(|key| prefixes.iter().any(|prefix| key.starts_with(prefix)))
            {
                // A filtered base-mode read still needs the twin's complete card.
                wide.push("rate.".to_string());
                wide.push("existence".to_string());
            }
            if crate::temperature_ranges::KEYS
                .iter()
                .any(|key| prefixes.iter().any(|prefix| key.starts_with(prefix)))
            {
                // The served acceptance flag, including replacements, gates ranges.
                wide.push("capability.temperature".to_string());
                wide.push("existence".to_string());
            }
            if prefixes.iter().any(|p| "model.lineage".starts_with(p)) {
                // Lineage needs classification inputs even when the caller
                // requests only the derived fact, not the upstream fields.
                wide.push("model.family".to_string());
                wide.push("model.canonical_id".to_string());
                wide.push("existence".to_string());
            }
            if prefixes
                .iter()
                .any(|p| "limit.audio_tokens_per_second".starts_with(p))
            {
                // The conversion is served only alongside an established audio rate.
                wide.push("rate.input_audio".to_string());
                wide.push("existence".to_string());
            }
            if crate::attachment_limits::KEYS
                .iter()
                .any(|key| prefixes.iter().any(|prefix| key.starts_with(prefix)))
                || prefixes
                    .iter()
                    .any(|prefix| prefix.starts_with("limit.image_long_edge_px.above_images."))
            {
                // Attachment bounds need modalities and sometimes context even
                // when the caller selected only a derived bound.
                wide.push("capability.input_modalities".to_string());
                wide.push("limit.context".to_string());
                wide.push("existence".to_string());
            }
            if prefixes.iter().any(|p| "wire.family".starts_with(p)) {
                // Protocol verification needs the package and shape, and endpoint
                // corrections must run before a route can advertise its protocol.
                wide.push("wire.".to_string());
                wide.push("existence".to_string());
            }
            wide
        });
    } else if let Some(prefixes) = &mut request.fact_prefixes {
        if admits_rates(prefixes) {
            // A base can match a mode-rate filter without any matching rate
            // of its own. Keep it through the bulk renderer for the twin join.
            prefixes.push("existence".to_string());
        }
    }
    let curated_prefixes = request.fact_prefixes.clone();
    if plane.is_some() {
        // Carry every rate and the real family through the existing named,
        // bulk and alias paths. Their intermediate filters now retain these
        // inputs; only the final plane view narrows to the caller's filter.
        request.fact_prefixes = request.fact_prefixes.as_ref().map(|prefixes| {
            let mut wide = widened_for_inheritance(prefixes);
            for prefix in [fusiform_store::prefix::RATE, fusiform_store::prefix::MODEL] {
                if !wide.iter().any(|p| p == prefix) {
                    wide.push(prefix.to_string());
                }
            }
            wide
        });
    }

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

    // A one-instant named read loads the same request-owned inputs that a
    // served history reuses at each of its boundaries; the overrides, curated
    // rows, billing rules and refusals below apply to both unchanged.
    let owned = match (prepared, at, &request.provider_id, &request.model_id) {
        (None, Some(_), Some(provider), Some(model)) => Some(dated_model::DatedModel::load(
            store, source, provider, model,
        )?),
        _ => None,
    };
    let prepared = prepared.or(owned.as_ref());

    // Checked ONCE, before the branch, so both read paths inherit it.
    // Keep this ahead of cache lookup: the first observation can establish a
    // coverage bound without changing either the version or the era watermark.
    //
    // The single-model and bulk reads have needed the same property three
    // separate times in this crate and diverged every time it was written
    // twice. Placing this above the split makes the divergence unwritable
    // rather than merely absent.
    if let Some(prepared) = prepared {
        refuse_before_begin(prepared.begins, at)?;
    } else {
        refuse_before_the_record(store, source, at)?;
    }

    if let Some(key) = &whole_key {
        // A short lock around one entry; no store read or render under the lock.
        let mut cached = oversized.lock().unwrap_or_else(|e| e.into_inner());
        if cached.as_ref().is_some_and(|entry| {
            entry.key.store_instance != key.store_instance
                || entry.key.catalog_version != key.catalog_version
                || entry.key.latest_era_id != key.latest_era_id
        }) {
            *cached = None;
        }
        if let Some(entry) = cached.as_ref().filter(|entry| entry.key == *key) {
            return Err(oversized_catalog_error(entry.size, frame_limit));
        }
    }

    let aliases = tables.aliases;
    let named = request.provider_id.is_some() && request.model_id.is_some();
    // Borrowed from the compiled file on a current read; owned when read from
    // the store's history for a point-in-time one.
    let mut curated: Vec<std::borrow::Cow<'_, crate::curated_models::CuratedModel>> = Vec::new();
    if let Some(at) = at {
        // The claim in force at the instant, from the store's history of the
        // curated file. The upstream-wins rule is asked at the same instant:
        // a curated row stands aside wherever models.dev had the model present
        // then, exactly as a current read stands aside while it is present now.
        let eras = match prepared {
            Some(prepared) => prepared.curated_era_at(at).cloned().into_iter().collect(),
            None => store
                .curated_models_at(at.0, request.provider_id.as_deref())
                .map_err(|e| store_error(&e))?,
        };
        for era in eras {
            if request
                .model_id
                .as_deref()
                .is_some_and(|m| m != era.model_id)
            {
                continue;
            }
            let Some(claim) = era.claim else {
                continue;
            };
            let publishes = match prepared {
                Some(prepared) => prepared
                    .read_model(&era.provider_id, &era.model_id, at)
                    .0
                    .is_some_and(|model| model.is_present()),
                None => {
                    upstream_publishes_at(store, source, &era.provider_id, &era.model_id, Some(at))?
                }
            };
            if !curate::curated_applies(publishes, false) {
                continue;
            }
            curated.push(std::borrow::Cow::Owned(crate::curated_models::from_store(
                claim,
            )));
        }
    } else {
        for ((provider, model), row) in &tables.curated_models.models {
            if range.is_some_and(|r| !r.contains(provider)) {
                continue;
            }
            if request
                .provider_id
                .as_deref()
                .is_some_and(|p| p != provider)
                || request.model_id.as_deref().is_some_and(|m| m != model)
            {
                continue;
            }
            // Upstream wins only while it currently publishes the model; a
            // withdrawn upstream row gives way. Check the store, not the
            // filtered response.
            if curated_in_effect(store, source, provider, model, tables.curated_models)?.is_some() {
                curated.push(std::borrow::Cow::Borrowed(row));
            }
        }
    }
    // Ids fusiform served and no longer does, reported as retired on current
    // reads: retired alias ids, and curated rows the file holds. See
    // `aliases::retired_aliases` and `curated_models::HeldModel` for why such
    // an id cannot simply vanish. A row the store knows for the id serves
    // instead (a withdrawn upstream row is then reported by the store's own
    // retirement), and a plane read never lists them, since a plane describes
    // how a login is billed.
    let mut retired_ids = Vec::new();
    if at.is_none() && plane.is_none() {
        let retired_alias_ids = tables.retired_aliases.values().map(|row| {
            (
                &row.provider_id,
                &row.model_id,
                row.retired_at_ms,
                row.successor.as_ref(),
            )
        });
        let held_ids = tables
            .curated_models
            .held
            .values()
            .map(|row| (&row.provider_id, &row.model_id, row.held_since_ms, None));
        for (provider, model, retired_at_ms, successor) in retired_alias_ids.chain(held_ids) {
            if range.is_some_and(|r| !r.contains(provider)) {
                continue;
            }
            if request
                .provider_id
                .as_deref()
                .is_some_and(|p| p != provider)
                || request.model_id.as_deref().is_some_and(|m| m != model)
            {
                continue;
            }
            if !curate::retirement_applies(
                store
                    .model_is_known(source, provider, model)
                    .map_err(|e| store_error(&e))?,
            ) {
                continue;
            }
            retired_ids.push((provider, model, retired_at_ms, successor));
        }
    }
    if let Some(range) = range {
        curated.retain(|row| range.contains(&row.provider_id));
        retired_ids.retain(|(provider, _, _, _)| range.contains(provider));
    }
    if let (Some(provider_id), Some(model_id)) =
        (request.provider_id.as_deref(), request.model_id.as_deref())
    {
        if let Some(alias) = alias_in_effect(store, source, aliases, provider_id, model_id)? {
            let response = serve_named_alias(
                store,
                source,
                provider_id,
                model_id,
                alias,
                at,
                request.fact_prefixes.as_ref(),
                tables,
            )?;
            return finish_plane_view(
                store,
                source,
                response,
                plane,
                requested_prefixes.as_ref(),
                at,
                prepared,
                tables,
                None,
                history_sample,
            );
        }
    }

    let (snapshot, retired) = if named && (!curated.is_empty() || !retired_ids.is_empty()) {
        (
            CatalogSnapshot {
                source,
                // The requested instant on a point-in-time read, as on every
                // other path, so the response says which instant it describes.
                resolved_at: at.unwrap_or_else(|| Timestamp(now_ms())),
                catalog_version: match prepared {
                    Some(prepared) => prepared.version,
                    None => store.catalog_version().map_err(|e| store_error(&e))?,
                },
                models: Vec::new(),
                withheld: Vec::new(),
                uncertain: Vec::new(),
            },
            Vec::new(),
        )
    } else if let (Some(provider_id), Some(model_id)) =
        (request.provider_id.as_deref(), request.model_id.as_deref())
    {
        single_model_snapshot(store, source, provider_id, model_id, at, &request, prepared)?
    } else {
        let query = CatalogQuery {
            source,
            #[cfg(test)]
            at: at.or_else(release_import_tests::current_at),
            #[cfg(not(test))]
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
        let snapshot = if let Some(range) = range {
            store.read_catalog_range(&query, range.after.as_deref(), range.through.as_deref())
        } else {
            store.read_catalog_for_provider(&query, request.provider_id.as_deref())
        }
        .map_err(|e| store_error(&e))?;
        if let Some(provider) = &request.provider_id {
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
                && curated.is_empty()
                && !provider_is_served(store, source, provider)?
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

    // Count the route's main render, not nested renders used to derive aliases.
    #[cfg(test)]
    CATALOG_RENDER_COUNT.with(|count| count.set(count.get() + 1));
    let historical_overrides = at
        .map(|at| match prepared {
            Some(prepared) => Ok(prepared.overrides_at(at)),
            None => crate::fact_overrides::at(store, at.0, request.provider_id.as_deref()),
        })
        .transpose()
        .map_err(|e| store_error(&e))?;
    let mut response = render(
        source,
        snapshot,
        at,
        Some((store, tables.creators)),
        request.fact_prefixes.as_ref(),
        historical_overrides.as_ref(),
        at.is_none().then_some(tables),
    );
    if !named {
        // Widening for an absent identity can fetch only existence. Preserve the
        // bulk-read contract: a model with no requested facts is not a result.
        response.models.retain(|_, facts| !facts.is_empty());
    }
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
            range,
            tables,
        )?;
    }
    if let Some(range) = range {
        response
            .models
            .retain(|model, _| range.contains(model_provider(model)));
        response
            .aliased
            .retain(|row| range.contains(model_provider(&row.model)));
    }
    for row in curated {
        let method = request.auth_method.as_deref();
        let name = if row.facts.contains_key("provider.name")
            || method.is_some_and(|method| !row.auth_methods.iter().any(|m| m == method))
        {
            None
        } else {
            read_provider_name(store, source, &row.provider_id, at)?
        };
        curate::apply_curated_row(
            &mut response,
            &row,
            name.as_deref(),
            method,
            curated_prefixes.as_ref(),
            named,
        );
    }
    if at.is_none() {
        curate::apply_login_methods(
            tables.login_methods,
            &mut response,
            request.auth_method.as_deref(),
        );
    }
    let mut response = finish_plane_view(
        store,
        source,
        response,
        plane,
        requested_prefixes.as_ref(),
        at,
        prepared,
        tables,
        page.map(|page| page.inputs.judgments),
        history_sample,
    )?;
    // Observed plane rows keep their envelope when billing rules remove all
    // selected rates. Curated rows and plain bulk rows require a selected fact.
    if !named {
        let curated = &response.curated;
        response.models.retain(|identity, facts| {
            !facts.is_empty()
                || (request.auth_method.is_some()
                    && !curated.iter().any(|row| &row.model == identity))
        });
        response
            .curated
            .retain(|row| response.models.contains_key(&row.model));
        response
            .aliased
            .retain(|row| response.models.contains_key(&row.model));
        response
            .login_methods
            .retain(|row| response.models.contains_key(&row.model));
    }
    for (provider, model, retired_at_ms, successor) in retired_ids {
        curate::apply_retired_id(
            &mut response,
            (provider, model),
            retired_at_ms,
            successor,
            request.include_retired,
            named,
            requested_prefixes.as_ref(),
        );
    }
    if !request.paged && request.provider_id.is_none() {
        // A whole-catalog reply too large for one frame is refused here, naming
        // the paged form, rather than left for the supervisor to replace with a
        // bare `frame_too_large` that does not say how to read the catalog.
        // Never truncated: a partial catalog would make the missing models look
        // retired. A provider-scoped read is not checked, because paging does
        // not apply to it and every provider today is far below the limit.
        let size = compact_size(&response)?;
        if size > frame_limit {
            if let Some(key) = whole_key {
                // Aliases and curated rows consult the store after the snapshot.
                // Do not remember a render that straddled a version change.
                if response.catalog_version == key.catalog_version
                    && store.catalog_version().map_err(|e| store_error(&e))? == key.catalog_version
                    && store.latest_era_id().map_err(|e| store_error(&e))? == key.latest_era_id
                    && response.resolved_at_ms.to_string().len() == key.timestamp_width
                {
                    *oversized.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(OversizedCatalog { key, size });
                }
            }
            return Err(oversized_catalog_error(size, frame_limit));
        }
    }
    Ok(response)
}

struct PageCursor {
    catalog_version: i64,
    resolved_at_ms: i64,
    provider: String,
    dated: bool,
}

fn page_token(
    dated: bool,
    version: i64,
    resolved_at: impl std::fmt::Display,
    provider: &str,
) -> String {
    let prefix = if dated { "v1-at" } else { "v1" };
    format!("{prefix}:{version}:{resolved_at}:{provider}")
}

fn parse_page_token(token: &str) -> Result<PageCursor, RouteError> {
    // Split only the first three separators so provider ids may contain ':'.
    let invalid = || RouteError::bad_request("malformed page_token: expected v1 snapshot token");
    let mut parts = token.splitn(4, ':');
    let dated = match parts.next() {
        Some("v1") => false,
        Some("v1-at") => true,
        _ => return Err(invalid()),
    };
    let number = |text: Option<&str>| -> Result<i64, RouteError> {
        let text = text.ok_or_else(invalid)?;
        let value: i64 = text.parse().map_err(|_| invalid())?;
        if value.to_string() != text {
            return Err(invalid());
        }
        Ok(value)
    };
    let catalog_version = number(parts.next())?;
    let resolved_at_ms = number(parts.next())?;
    let provider = parts.next().filter(|p| !p.is_empty()).ok_or_else(invalid)?;
    Ok(PageCursor {
        catalog_version,
        resolved_at_ms,
        provider: provider.to_string(),
        dated,
    })
}

fn check_page_version(version: i64, cursor: &PageCursor) -> Result<(), RouteError> {
    if version != cursor.catalog_version {
        return Err(RouteError {
            code: fusiform_protocol::CODE_CATALOG_CHANGED,
            message: "catalog changed since the first page; restart from page one".into(),
        });
    }
    Ok(())
}

fn model_provider(model: &str) -> &str {
    model
        .split_once('/')
        .expect("served identities contain a provider")
        .0
}

#[cfg(test)]
fn scope_page(response: &mut CatalogGetResponse, providers: &BTreeSet<String>) {
    response
        .models
        .retain(|model, _| providers.contains(model_provider(model)));
    response
        .withheld
        .retain(|row| providers.contains(model_provider(&row.model)));
    response
        .uncertain
        .retain(|row| providers.contains(&row.provider_id));
    response
        .curated
        .retain(|row| providers.contains(model_provider(&row.model)));
    response
        .login_methods
        .retain(|row| providers.contains(model_provider(&row.model)));
    response
        .retired
        .retain(|row| providers.contains(model_provider(&row.model)));
    response
        .aliased
        .retain(|row| providers.contains(model_provider(&row.model)));
    response
        .billing_rules
        .retain(|row| providers.contains(model_provider(&row.model)));
    response
        .canonical_links
        .retain(|row| providers.contains(model_provider(&row.model)));
    response
        .speed_twins
        .retain(|row| providers.contains(model_provider(&row.model)));
    response
        .wire_corrections
        .retain(|row| providers.contains(&row.provider));
    response
        .fact_overrides
        .retain(|row| providers.contains(model_provider(&row.model)));
    retain_quality_disclosure(response);
}

#[cfg(test)]
fn page_catalog(
    mut response: CatalogGetResponse,
    cursor: Option<&PageCursor>,
    budget: usize,
    dated: bool,
) -> Result<CatalogGetResponse, RouteError> {
    let mut providers = partition_providers(&mut response);
    if let Some(cursor) = cursor {
        providers.retain(|provider, _| provider > &cursor.provider);
    }
    response.next_page_token = None;
    let mut sized = Vec::with_capacity(providers.len());
    let mut token_reserve = 0;
    // A whole-catalog response carries one disclosure, for the newest grade
    // anywhere. Each page must instead be charged for the newest grade among
    // its own providers, so each provider's timestamp is measured before packing.
    let quality = QualityContribution::measure(response.quality_scores.as_ref())?;
    for (provider, rows) in providers {
        let contribution = rows.contribution(quality)?;
        let token = page_token(
            dated,
            response.catalog_version,
            response.resolved_at_ms,
            &provider,
        );
        let token_bytes = compact_size(&token)? + r#","next_page_token":"#.len();
        token_reserve = token_reserve.max(token_bytes);
        sized.push((provider, rows, contribution, token, token_bytes));
    }
    // Reserve the largest encoded token before packing. Replacing that reserve
    // with each boundary's actual token cost preserves exact greedy boundaries,
    // including the final page, which needs no token at all.
    let mut estimate = PageEstimate {
        bytes: compact_size(&response)? - quality.map_or(0, |q| q.bytes) + token_reserve,
        present: [false; 12],
        quality: None,
    };
    let mut count = 0;
    for (index, (_, _, contribution, _, token_bytes)) in sized.iter().enumerate() {
        let candidate = estimate.with_provider(contribution)?;
        let token_bytes = if index + 1 < sized.len() {
            *token_bytes
        } else {
            0
        };
        let size = candidate.bytes - token_reserve + token_bytes;
        if size > budget && count > 0 {
            break;
        }
        count += 1;
        estimate = candidate;
        if size > budget {
            break;
        }
    }
    let mut boundaries = Vec::with_capacity(count);
    let mut has_more = count < sized.len();
    for (provider, rows, _, token, _) in sized.into_iter().take(count) {
        rows.append_to(&mut response);
        boundaries.push((provider, token));
    }
    let initial_count = boundaries.len();
    loop {
        retain_quality_disclosure(&mut response);
        response.next_page_token = if has_more {
            boundaries.last().map(|(_, token)| token.clone())
        } else {
            None
        };
        // Normally the additive estimate is exact, so the completed page is
        // serialized just once. Keep a bounded fallback if the wire shape and
        // accounting ever diverge: remove a whole provider, never individual rows.
        if compact_size(&response)? <= budget || boundaries.len() <= 1 {
            return Ok(response);
        }
        let (last, _) = boundaries
            .pop()
            .expect("an oversized multi-provider page has a last provider");
        let mut remaining = partition_providers(&mut response);
        assert!(
            remaining.remove(&last).is_some(),
            "page rebuild must remove its last provider"
        );
        assert!(
            remaining.len() == boundaries.len() && boundaries.len() < initial_count,
            "page rebuild must strictly decrease the provider count"
        );
        for rows in remaining.into_values() {
            rows.append_to(&mut response);
        }
        has_more = true;
        // Always return one whole provider, even over budget. Providers exceeding
        // the supervisor's 4 MiB frame limit would need model-level paging.
    }
}

#[derive(Default, serde::Serialize)]
struct ProviderRows {
    models: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    curated: Vec<fusiform_protocol::CuratedModelWire>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    withheld: Vec<WithheldFactWire>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    retired: Vec<fusiform_protocol::RetiredModelWire>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    uncertain: Vec<UncertainFactWire>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    aliased: Vec<fusiform_protocol::AliasedModelWire>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    billing_rules: Vec<BillingRuleWire>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    canonical_links: Vec<fusiform_protocol::CanonicalLinkWire>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    speed_twins: Vec<fusiform_protocol::SpeedTwinWire>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    wire_corrections: Vec<fusiform_protocol::WireCorrectionWire>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fact_overrides: Vec<fusiform_protocol::FactOverrideWire>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    login_methods: Vec<fusiform_protocol::LoginMethodsWire>,
}

// The empty models object is already in the envelope. Optional array headers
// appear only once per page; provider payloads supply their interior JSON bytes.
const DISCLOSURE_HEADERS: [&str; 11] = [
    r#","curated":[]"#,
    r#","withheld":[]"#,
    r#","retired":[]"#,
    r#","uncertain":[]"#,
    r#","aliased":[]"#,
    r#","billing_rules":[]"#,
    r#","canonical_links":[]"#,
    r#","speed_twins":[]"#,
    r#","wire_corrections":[]"#,
    r#","fact_overrides":[]"#,
    r#","login_methods":[]"#,
];

#[derive(Clone, Copy)]
struct ProviderContribution {
    bytes: usize,
    present: [bool; 12],
    quality: Option<QualityContribution>,
}

#[derive(Clone, Copy)]
struct QualityContribution {
    graded_at_ms: i64,
    bytes: usize,
}

impl QualityContribution {
    fn measure(
        quality: Option<&fusiform_protocol::QualityScoresWire>,
    ) -> Result<Option<Self>, RouteError> {
        quality
            .map(|quality| {
                Ok(Self {
                    graded_at_ms: quality.graded_at_ms,
                    bytes: compact_size(quality)? + r#","quality_scores":"#.len(),
                })
            })
            .transpose()
    }
}

#[derive(Clone, Copy)]
struct PageEstimate {
    bytes: usize,
    present: [bool; 12],
    quality: Option<QualityContribution>,
}

impl PageEstimate {
    fn with_provider(mut self, provider: &ProviderContribution) -> Result<Self, RouteError> {
        let mut added = provider.bytes;
        // The page has one disclosure, from the newest judgment it serves.
        if let Some(quality) = provider.quality {
            if self
                .quality
                .is_none_or(|prior| quality.graded_at_ms > prior.graded_at_ms)
            {
                self.bytes -= self.quality.map_or(0, |prior| prior.bytes);
                self.quality = Some(quality);
                added += quality.bytes;
            }
        }
        for (index, present) in provider.present.iter().enumerate() {
            if *present {
                added += if self.present[index] {
                    1
                } else if index > 0 {
                    DISCLOSURE_HEADERS[index - 1].len()
                } else {
                    0
                };
                self.present[index] = true;
            }
        }
        self.bytes = self
            .bytes
            .checked_add(added)
            .ok_or_else(|| RouteError::unavailable("catalog page size overflow"))?;
        Ok(self)
    }
}

impl ProviderRows {
    fn contribution(
        &self,
        quality: Option<QualityContribution>,
    ) -> Result<ProviderContribution, RouteError> {
        let present = [
            !self.models.is_empty(),
            !self.curated.is_empty(),
            !self.withheld.is_empty(),
            !self.retired.is_empty(),
            !self.uncertain.is_empty(),
            !self.aliased.is_empty(),
            !self.billing_rules.is_empty(),
            !self.canonical_links.is_empty(),
            !self.speed_twins.is_empty(),
            !self.wire_corrections.is_empty(),
            !self.fact_overrides.is_empty(),
            !self.login_methods.is_empty(),
        ];
        let headers = DISCLOSURE_HEADERS
            .iter()
            .zip(&present[1..])
            .filter_map(|(header, present)| present.then_some(header.len()))
            .sum::<usize>();
        // This serializes each provider's map entries and disclosure rows once,
        // including escaped keys, values, and separators within each collection.
        let bytes = compact_size(self)? - r#"{"models":{}}"#.len() - headers;
        Ok(ProviderContribution {
            bytes,
            present,
            quality: quality.filter(|_| {
                self.models
                    .values()
                    .any(curate::counts_for_quality_disclosure)
            }),
        })
    }

    fn append_to(self, page: &mut CatalogGetResponse) {
        page.models.extend(self.models);
        page.curated.extend(self.curated);
        page.withheld.extend(self.withheld);
        page.retired.extend(self.retired);
        page.uncertain.extend(self.uncertain);
        page.aliased.extend(self.aliased);
        page.billing_rules.extend(self.billing_rules);
        page.canonical_links.extend(self.canonical_links);
        page.speed_twins.extend(self.speed_twins);
        page.wire_corrections.extend(self.wire_corrections);
        page.fact_overrides.extend(self.fact_overrides);
        page.login_methods.extend(self.login_methods);
    }
}

fn compact_size<T: serde::Serialize>(value: &T) -> Result<usize, RouteError> {
    struct CountBytes(usize);
    impl std::io::Write for CountBytes {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("catalog page size overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // Size checks use the identical serializer without allocating reply buffers
    // that would be discarded immediately.
    let mut count = CountBytes(0);
    serde_json::to_writer(&mut count, value).map_err(|e| RouteError::unavailable(e.to_string()))?;
    Ok(count.0)
}

fn partition_providers(response: &mut CatalogGetResponse) -> BTreeMap<String, ProviderRows> {
    let mut providers: BTreeMap<String, ProviderRows> = BTreeMap::new();
    for row in std::mem::take(&mut response.speed_twins) {
        providers
            .entry(model_provider(&row.model).to_owned())
            .or_default()
            .speed_twins
            .push(row);
    }
    for row in std::mem::take(&mut response.login_methods) {
        providers
            .entry(model_provider(&row.model).to_owned())
            .or_default()
            .login_methods
            .push(row);
    }
    for (model, facts) in std::mem::take(&mut response.models) {
        providers
            .entry(model_provider(&model).to_owned())
            .or_default()
            .models
            .insert(model, facts);
    }
    // Include disclosure-only providers: withholding every matching fact must
    // not make that provider's explanation disappear between pages.
    for row in std::mem::take(&mut response.withheld) {
        providers
            .entry(model_provider(&row.model).to_owned())
            .or_default()
            .withheld
            .push(row);
    }
    for row in std::mem::take(&mut response.uncertain) {
        providers
            .entry(row.provider_id.clone())
            .or_default()
            .uncertain
            .push(row);
    }
    for row in std::mem::take(&mut response.curated) {
        providers
            .entry(model_provider(&row.model).to_owned())
            .or_default()
            .curated
            .push(row);
    }
    for row in std::mem::take(&mut response.retired) {
        providers
            .entry(model_provider(&row.model).to_owned())
            .or_default()
            .retired
            .push(row);
    }
    for row in std::mem::take(&mut response.aliased) {
        providers
            .entry(model_provider(&row.model).to_owned())
            .or_default()
            .aliased
            .push(row);
    }
    for row in std::mem::take(&mut response.billing_rules) {
        providers
            .entry(model_provider(&row.model).to_owned())
            .or_default()
            .billing_rules
            .push(row);
    }
    for row in std::mem::take(&mut response.canonical_links) {
        providers
            .entry(model_provider(&row.model).to_owned())
            .or_default()
            .canonical_links
            .push(row);
    }
    for row in std::mem::take(&mut response.wire_corrections) {
        providers
            .entry(row.provider.clone())
            .or_default()
            .wire_corrections
            .push(row);
    }
    for row in std::mem::take(&mut response.fact_overrides) {
        providers
            .entry(model_provider(&row.model).to_owned())
            .or_default()
            .fact_overrides
            .push(row);
    }
    providers
}

struct SelectedPlane<'a> {
    wire: PlaneWire,
    declared: Option<&'a Plane>,
}

fn select_billing_plane<'a>(
    request: &CatalogGetRequest,
    planes: &'a curate::billing_planes::Planes,
) -> Result<Option<SelectedPlane<'a>>, RouteError> {
    let Some(auth_method) = request.auth_method.as_deref() else {
        return Ok(None);
    };
    // The accepted methods match the login methods the credential vault records
    // (docs/billing-planes.md).
    // The set is closed so that a new login method is refused until someone
    // reviews how it bills, instead of quietly receiving the API price card.
    if !billing_planes::AUTH_METHODS.contains(&auth_method) {
        return Err(RouteError::bad_request(format!(
            "unknown auth_method {auth_method:?}; expected apikey, chatgpt, oauth, antigravity or gateway"
        )));
    }
    let Some(provider_id) = request.provider_id.as_deref() else {
        return Err(RouteError::bad_request("auth_method requires provider_id"));
    };
    if request.at_ms.is_some() {
        return Err(RouteError::no_coverage(
            "billing plane views are current-only: auth_method cannot be used with at_ms",
        ));
    }
    let declared = planes.get(&(provider_id.to_string(), auth_method.to_string()));
    if declared.is_none() && auth_method != "apikey" {
        return Err(RouteError::no_coverage(format!(
            "no billing plane declared for ({provider_id}, {auth_method}); only an apikey read is served the API price card without one"
        )));
    }
    Ok(Some(SelectedPlane {
        wire: PlaneWire {
            provider_id: provider_id.to_string(),
            auth_method: auth_method.to_string(),
            kind: declared
                .map_or(PlaneKind::Upstream, |p| p.kind)
                .as_str()
                .to_string(),
            source_ref: declared.map(|p| p.source_ref.clone()),
            review_by_ms: declared.map(|p| p.review_by_ms),
        },
        declared,
    }))
}

fn selects_chat_score(prefix: &str) -> bool {
    "quality.elo".starts_with(prefix) || "quality.eq".starts_with(prefix)
}

fn selects_judgments(prefixes: &[String]) -> bool {
    prefixes.iter().any(|p| {
        selects_chat_score(p)
            || "quality.decommissioned".starts_with(p)
            || curate::selects_effort_facts(p)
    }) || curate::TaskGradeRequirement::required_facts_for_prefixes(prefixes)
        .next()
        .is_some()
}

#[derive(Default)]
struct CatalogJudgments {
    published: curate::Quality,
    local: fusiform_store::LocalOverrides,
}

impl CatalogJudgments {
    fn load(
        store: &CatalogStore,
        at: Option<i64>,
        prefixes: Option<&[String]>,
    ) -> Result<Self, RouteError> {
        // Judgments and their disclosures are served only when the prefixes select judgments.
        if prefixes.is_some_and(|p| !selects_judgments(p)) {
            return Ok(Self::default());
        }
        Ok(Self {
            published: read_quality_at(store, at)?,
            local: read_local_overrides_at(store, at)?,
        })
    }
}

/// Gather the additional reads needed by the selected view. The transformations
/// stay in the curation crate; historically resolved rules are passed as data.
#[allow(clippy::too_many_arguments)]
fn finish_plane_view(
    store: &CatalogStore,
    source: SourceId,
    mut response: CatalogGetResponse,
    plane: Option<SelectedPlane<'_>>,
    requested_prefixes: Option<&Vec<String>>,
    at: Option<Timestamp>,
    prepared: Option<&dated_model::DatedModel>,
    tables: &curate::CurationSet<'_>,
    judgments: Option<&CatalogJudgments>,
    history_sample: bool,
) -> Result<CatalogGetResponse, RouteError> {
    let historical_rules = at
        .map(|at| match prepared {
            Some(prepared) => Ok(prepared.lifetimes_at(at).cloned().unwrap_or_default()),
            None => crate::cache_lifetimes::at(store, at.0, None),
        })
        .transpose()
        .map_err(|e| store_error(&e))?;
    let lifetimes = historical_rules.as_ref().unwrap_or(tables.cache_lifetimes);
    let current = at.is_none().then_some(tables);
    let owned_judgments;
    // A served-history sample never carries judgments: their history is read
    // through quality.grades, not catalog.history.
    let judgments = if history_sample {
        owned_judgments = CatalogJudgments::default();
        &owned_judgments
    } else if let Some(judgments) = judgments {
        judgments
    } else {
        owned_judgments = CatalogJudgments::load(
            store,
            at.map(|time| time.0),
            requested_prefixes.map(Vec::as_slice),
        )?;
        &owned_judgments
    };
    let published_quality = &judgments.published;
    let (quality, metadata) = local_overrides::overlay(published_quality, &judgments.local);
    if metadata.is_empty() {
        curate::derive_row_facts(
            &mut response,
            current,
            lifetimes,
            published_quality,
            requested_prefixes,
        );
    } else {
        let mut published = response.clone();
        curate::derive_row_facts(
            &mut published,
            current,
            lifetimes,
            published_quality,
            requested_prefixes,
        );
        curate::derive_row_facts(
            &mut response,
            current,
            lifetimes,
            &quality,
            requested_prefixes,
        );
        local_overrides::disclose(&published, &mut response, &metadata);
    }
    if let Some(plane) = plane {
        let speed_twins = tables.speed_twins;
        let mut cards = BTreeMap::new();
        if let Some(provider) = plane.declared.and_then(Plane::borrows_rates_from) {
            for (identity, facts) in &response.models {
                if let Some(id) = curate::plane_rate_target(speed_twins, identity, facts, provider)
                {
                    if let Some(origin) = store
                        .read_model(
                            source,
                            provider,
                            id,
                            Some(Timestamp(response.resolved_at_ms)),
                        )
                        .map_err(|e| store_error(&e))?
                        .0
                        .filter(|model| model.is_present())
                    {
                        cards.insert(
                            id.to_string(),
                            origin
                                .facts
                                .into_iter()
                                .map(|(k, v)| (k.as_str().to_string(), v))
                                .collect(),
                        );
                    }
                }
            }
        }
        curate::project_plane(
            &mut response,
            plane.wire,
            plane.declared,
            speed_twins,
            &cards,
        );
    }
    if let Some(at) = at {
        if requested_prefixes.is_none_or(|prefixes| admits_rates(prefixes)) {
            if let Some(prepared) = prepared {
                prepared.speed_twins.apply(at, &mut response);
            } else {
                let twins = dated_model::DatedSpeedTwins::load(store, source, |row| {
                    response
                        .models
                        .contains_key(&format!("{}/{}", row.provider_id, row.base_model_id))
                })?;
                twins.apply(at, &mut response);
            }
        }
    } else {
        let method = response
            .plane
            .as_ref()
            .map(|plane| plane.auth_method.clone());
        read_speed_twin_cards(
            store,
            source,
            &mut response,
            method.as_deref(),
            tables.speed_twins,
        )?;
    }
    curate::finish_view(
        &mut response,
        current.map(|tables| tables.speed_twins),
        requested_prefixes,
    );
    Ok(response)
}

fn read_local_overrides_at(
    store: &CatalogStore,
    at: Option<i64>,
) -> Result<fusiform_store::LocalOverrides, RouteError> {
    #[cfg(test)]
    paging_tests::LOCAL_OVERRIDE_READS.with(|count| count.set(count.get() + 1));
    store.local_overrides_at(at).map_err(|e| store_error(&e))
}

fn read_quality_at(store: &CatalogStore, at: Option<i64>) -> Result<curate::Quality, RouteError> {
    #[cfg(test)]
    paging_tests::PUBLISHED_QUALITY_READS.with(|count| count.set(count.get() + 1));
    let grades = match at {
        Some(at) => store.quality_grades_at(at),
        None => store.quality_grades(),
    }
    .map_err(|e| store_error(&e))?
    .into_iter()
    .map(|row| curate::RecordedGrade {
        model: row.model,
        kind: row.kind,
        graded_effort: row.graded_effort,
        grade: match row.grade.expect("current grades exclude withdrawals") {
            fusiform_store::QualityGrade::Chat { elo, eq } => curate::Grade::Chat { elo, eq },
            fusiform_store::QualityGrade::NonChat { grade } => curate::Grade::NonChat { grade },
        },
        recorded_at_ms: row.recorded_at_ms,
    })
    .collect();
    let marks = match at {
        Some(at) => store.model_marks_at(at),
        None => store.model_marks(),
    }
    .map_err(|e| store_error(&e))?
    .into_iter()
    .map(|row| (row.model, row.recorded_at_ms))
    .collect();
    let efforts = store
        .effort_grades_at(at)
        .map_err(|e| store_error(&e))?
        .into_iter()
        .filter_map(|row| {
            Some(curate::RecordedEffortGrade {
                model: row.model,
                effort: row.effort,
                elo: row.elo?,
                eq: row.eq?,
                recorded_at_ms: row.recorded_at_ms,
            })
        })
        .collect();
    let workloads = store
        .workload_efforts_at(at)
        .map_err(|e| store_error(&e))?
        .into_iter()
        .map(|row| curate::RecordedWorkloadEffort {
            model: row.model,
            workload: row.workload,
            effort_default: row.effort_default,
            effort_minimum: row.effort_minimum,
            excluded: row.excluded,
            recorded_at_ms: row.recorded_at_ms,
        })
        .collect();
    Ok(curate::Quality {
        grades,
        marks,
        efforts,
        workloads,
    })
}

fn read_speed_twin_cards(
    store: &CatalogStore,
    source: SourceId,
    response: &mut CatalogGetResponse,
    auth_method: Option<&str>,
    twins: &curate::speed_twins::SpeedTwins,
) -> Result<(), RouteError> {
    for row in twins.values() {
        let base = format!("{}/{}", row.provider_id, row.base_model_id);
        let twin = format!("{}/{}", row.provider_id, row.twin_model_id);
        curate::annotate_speed_twin(row, response);
        if !response.models.contains_key(&base) {
            continue;
        }
        let present = store
            .read_model(
                source,
                &row.provider_id,
                &row.twin_model_id,
                Some(Timestamp(response.resolved_at_ms)),
            )
            .map_err(|error| store_error(&error))?
            .0
            .is_some_and(|model| model.is_present());
        if !present {
            continue;
        }
        let card = if let Some(facts) = response.models.get(&twin) {
            facts.clone()
        } else {
            // Named base reads still use the twin's full served context, not its
            // stored zeros or a differently selected billing plane.
            let args = serde_json::json!({
                "provider_id": row.provider_id, "model_id": row.twin_model_id,
                "auth_method": auth_method, "fact_prefixes": ["rate."],
            });
            let mut served = serve_catalog_get(
                store,
                &serde_json::to_vec(&args).expect("request serializes"),
            )?;
            let Some(facts) = served.models.remove(&twin) else {
                continue;
            };
            facts
        };
        curate::apply_speed_twin(row, &base, card, response);
    }
    Ok(())
}

fn single_model_snapshot(
    store: &CatalogStore,
    source: SourceId,
    provider_id: &str,
    model_id: &str,
    at: Option<Timestamp>,
    request: &CatalogGetRequest,
    prepared: Option<&dated_model::DatedModel>,
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
    let (found, withheld) = match prepared {
        Some(prepared) => prepared.read_model(provider_id, model_id, resolved_at),
        None => store
            .read_model(source, provider_id, model_id, Some(resolved_at))
            .map_err(|e| store_error(&e))?,
    };

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
        if !match prepared {
            Some(prepared) => prepared.provider_known(),
            None => provider_is_served(store, source, provider_id)?,
        } {
            return Err(RouteError::no_coverage(format!(
                "unknown provider {provider_id:?}: fusiform has never recorded \
                 a model under that id"
            )));
        }
        // A curated id at an instant with no curated claim in force: before
        // the row's first stored era, or after its tombstone. Asked before the
        // unknown-model check, which would call a right id a typo, and also
        // for an id upstream once listed: at an instant before either source
        // had the model, the curated reason is the one that says when a read
        // would answer.
        if let Some(at) = at {
            let refusal = match prepared {
                Some(prepared) => {
                    curated_refusal_from_eras(&prepared.curated, provider_id, model_id, at)
                }
                None => curated_refusal_at(store, provider_id, model_id, at)?,
            };
            if let Some(refusal) = refusal {
                return Err(refusal);
            }
        }
        if !match prepared {
            Some(prepared) => prepared.model_known(),
            None => store
                .model_is_known(source, provider_id, model_id)
                .map_err(|e| store_error(&e))?,
        } {
            return Err(unknown_model(provider_id, model_id));
        }
        // Known, and absent at this instant: that is a real answer about a real
        // model, so it stays an empty result rather than becoming an error.
    }

    let catalog_version = match prepared {
        Some(prepared) => prepared.version,
        None => store.catalog_version().map_err(|e| store_error(&e))?,
    };

    // Qualify only values in force that a correction has not withheld from this read. Use the
    // facts before the caller's filter so narrowing the card does not hide
    // their uncertainty disclosures.
    let prepared_uncertain = prepared.map(|prepared| {
        found
            .as_ref()
            .map_or_else(Vec::new, |model| prepared.uncertain_at(model, resolved_at))
    });
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
                successor: None,
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
                    .last_changed_at_or_before(
                        source,
                        &model.provider_id,
                        &model.model_id,
                        &fusiform_store::FactKey::existence(),
                        resolved_at,
                    )
                    .map_err(|e| store_error(&e))?
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

    let uncertain = if let Some(uncertain) = prepared_uncertain {
        uncertain
    } else if let Some(at) = at {
        store
            .uncertain_model_facts_at(source, at, provider_id, model_id)
            .map_err(|e| store_error(&e))?
            .into_iter()
            .map(|((provider_id, model_id, fact_key), (prior, boundary))| {
                fusiform_store::serve::UncertainFact {
                    provider_id,
                    model_id,
                    fact_key,
                    superseded_after: prior,
                    superseded_by: boundary,
                }
            })
            .collect()
    } else {
        Vec::new()
    };

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

/// Convert snapshot metadata and supply each upstream row to the pure renderer.
fn render(
    source: SourceId,
    snapshot: CatalogSnapshot,
    at: Option<Timestamp>,
    inherit: Option<(&CatalogStore, &crate::creators::Creators)>,
    requested_prefixes: Option<&Vec<String>>,
    historical_overrides: Option<&crate::fact_overrides::FactOverrides>,
    current: Option<&curate::CurationSet<'_>>,
) -> CatalogGetResponse {
    let withheld = snapshot
        .withheld
        .iter()
        .map(|w| WithheldFactWire {
            model: format!("{}/{}", w.provider_id, w.model_id),
            fact_key: w.fact_key.as_str().to_string(),
            corrections: w
                .corrections
                .iter()
                .map(|c| CorrectionDetail {
                    affected_from_ms: c.affected_from.0,
                    affected_until_ms: c.affected_until.0,
                    reason: c.reason.clone(),
                    fields: serde_json::from_str(&c.fields_json)
                        .unwrap_or_else(|_| serde_json::Value::String(c.fields_json.clone())),
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

    let mut response = curate::response_envelope(
        source,
        snapshot.resolved_at.0,
        snapshot.catalog_version,
        withheld,
        uncertain,
    );
    for model in snapshot.models {
        let provider = model.provider_id.as_str();
        let model_id = model.model_id.as_str();
        let identity = format!("{provider}/{model_id}");
        let withheld: Vec<_> = snapshot
            .withheld
            .iter()
            .filter(|w| w.provider_id == provider && w.model_id == model_id)
            .map(|w| w.fact_key.as_str())
            .collect();
        let mut facts = curate::render_row(
            &mut response,
            (provider, model_id),
            model
                .facts
                .iter()
                .map(|(k, raw)| (k.as_str(), raw.as_str())),
            &withheld,
            requested_prefixes,
            historical_overrides,
            current,
        );
        if let (Some((store, creators)), None) = (inherit, at) {
            inherit_rate_for(store, source, creators, &mut facts, model_id);
        }
        narrow_to_requested(&mut facts, requested_prefixes);
        response.models.insert(identity, facts);
    }
    response
}

/// A dated read must use the provider label in force at the requested instant.
fn read_provider_name(
    store: &CatalogStore,
    source: SourceId,
    provider_id: &str,
    at: Option<Timestamp>,
) -> Result<Option<String>, RouteError> {
    match at {
        None => store.provider_display_name(source, provider_id),
        Some(at) => store.provider_display_name_at(source, provider_id, at),
    }
    .map_err(|e| store_error(&e))
}

/// Whether fusiform serves anything under this provider id.
///
/// The store knows a provider only once it has recorded an era for it. A
/// provider served solely through curated rows, because models.dev does not
/// list it at all, has no era, so asking the store alone would refuse it as
/// unknown. That would refuse a point-in-time or filtered read of a provider
/// whose current rows are served, and would report a mistyped model id under
/// it as a wrong provider. Every route that asks "is this provider known"
/// asks here instead.
///
/// A held row counts: its id is still reported (as retired when the store has
/// no row for it), so its provider stays known. A provider whose only rows are held answers a plain read with
/// zero models and a retired-inclusive read with those ids marked absent.
fn provider_is_served(
    store: &CatalogStore,
    source: SourceId,
    provider_id: &str,
) -> Result<bool, RouteError> {
    if store.is_store_mode() {
        if !store
            .curated_models_at(i64::MAX, Some(provider_id))
            .map_err(|e| store_error(&e))?
            .is_empty()
        {
            return Ok(true);
        }
    } else if crate::curated_models::models()
        .keys()
        .any(|(provider, _)| provider == provider_id)
    {
        return Ok(true);
    }
    store
        .provider_is_known(source, provider_id)
        .map_err(|e| store_error(&e))
}

/// Whether models.dev publishes this model at an instant (`None` for now): the
/// store's row for it at that instant says `existence: present`.
///
/// Asked through `read_model`, the read the named path uses to decide whether
/// a model is retired, so "present" means here exactly what it means to every
/// other read at the same instant, corrections included. A model the store had
/// not recorded by then and a model upstream had listed and withdrawn both
/// answer false.
fn upstream_publishes_at(
    store: &CatalogStore,
    source: SourceId,
    provider_id: &str,
    model_id: &str,
    at: Option<Timestamp>,
) -> Result<bool, RouteError> {
    let (found, _) = store
        .read_model(source, provider_id, model_id, at)
        .map_err(|e| store_error(&e))?;
    Ok(found.is_some_and(|model| model.is_present()))
}

/// The curated row for this id, when it is served on a current read.
///
/// Point-in-time reads do not ask here: they read the claim in force at their
/// instant from the store's history of the curated file, and apply the same
/// upstream-wins rule at that instant (see `serve_catalog_get_with_cache`).
///
/// Upstream wins only while it currently publishes the model. A model
/// models.dev listed and then withdrew is still known to the store, as a
/// retired row, but upstream no longer offers it, so the curated row is served
/// in its place, whole: none of the withdrawn row's stored facts are mixed in.
/// Measured on the live store: models.dev listed
/// `cloudflare-workers-ai/@cf/cloudflare/clef` for five hours and withdrew it,
/// and asking only "has the store ever seen this id" hid the curated row on
/// every read from then on. When upstream lists the model again, this answers
/// `None` and the upstream row is served, with no edit to the curated file.
///
/// A row listed in the file's `held` array is never in effect: see
/// `curated_models::HeldModel`.
///
/// Every route that decides whether a curated row applies asks here, so the
/// rule has one statement.
fn curated_in_effect<'a>(
    store: &CatalogStore,
    source: SourceId,
    provider_id: &str,
    model_id: &str,
    curated: &'a curate::curated_models::Table,
) -> Result<Option<&'a crate::curated_models::CuratedModel>, RouteError> {
    let key = (provider_id.to_string(), model_id.to_string());
    let Some(row) = curated.models.get(&key) else {
        return Ok(None);
    };
    // A held row is never served as curated. When the store has no row for
    // the id, current reads report it as retired instead.
    if curated.held.contains_key(&key) {
        return Ok(None);
    }
    let present = upstream_publishes_at(store, source, provider_id, model_id, None)?;
    Ok(curate::curated_applies(present, false).then_some(row))
}

/// Why a point-in-time read of a curated id has no curated row to serve, as a
/// `no_coverage` refusal. `None` when the id is not a curated row, or when a
/// curated claim was in force at the instant (then upstream had the model
/// present, and its row answers).
///
/// Without this, an id that only curation ever served would be refused as an
/// unknown model, which sends the caller hunting a typo in an id that is right.
fn curated_refusal_at(
    store: &CatalogStore,
    provider_id: &str,
    model_id: &str,
    at: Timestamp,
) -> Result<Option<RouteError>, RouteError> {
    let eras = store
        .curated_model_eras(provider_id, model_id)
        .map_err(|e| store_error(&e))?;
    Ok(curated_refusal_from_eras(&eras, provider_id, model_id, at))
}

fn curated_refusal_from_eras(
    eras: &[fusiform_store::CuratedModelEra],
    provider_id: &str,
    model_id: &str,
    at: Timestamp,
) -> Option<RouteError> {
    let id = format!("{provider_id}/{model_id}");
    let Some(first) = eras.first() else {
        // The file carries the row but this store has no record of it: the
        // boot that records the file has not run, or failed.
        let in_file = crate::curated_models::models()
            .contains_key(&(provider_id.to_string(), model_id.to_string()));
        return in_file.then(|| {
            RouteError::no_coverage(format!(
                "{id} is a curated row with no stored history: fusiform records \
                 a curated row when a build carrying it starts, and this store \
                 holds no record of it, so it has no value to give at {}. Read \
                 it without at_ms; the response's curated list gives its source \
                 and review date",
                at.0
            ))
        });
    };
    // Eras are oldest first, so the last one at or before the instant is the
    // one in force, as on the read path.
    match eras.iter().rev().find(|era| era.boundary_at_ms <= at.0) {
        None => Some(RouteError::no_coverage(format!(
            "{id} is a curated row, and fusiform's record of it begins at {}: \
             fusiform did not serve it before then, so it has no value to give \
             at {}. A read at or after {} answers it, as does a read without \
             at_ms",
            first.boundary_at_ms, at.0, first.boundary_at_ms
        ))),
        Some(era) if era.claim.is_none() => Some(RouteError::no_coverage(format!(
            "{id} is a curated row fusiform stopped serving at {}: from then its \
             curated file no longer carried it, or held it, so fusiform has no \
             value to give at {}. A read before {} answers with the value served \
             then",
            era.boundary_at_ms, at.0, era.boundary_at_ms
        ))),
        Some(_) => None,
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

#[cfg(test)]
thread_local! {
    pub(crate) static AFTER_PLAN_PRICE_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Serve the curated subscription prices.
///
fn serve_plan_prices(store: &CatalogStore, args: &[u8]) -> Result<PlanPricesResponse, RouteError> {
    let request: PlanPricesRequest = serde_json::from_slice(args)
        .map_err(|e| RouteError::bad_request(format!("malformed plan.prices request: {e}")))?;

    if request.at_ms.is_some() && !store.is_store_mode() {
        return Err(RouteError::bad_request(
            "dated plan.prices requires store mode",
        ));
    }
    let now = request.at_ms.unwrap_or_else(now_ms);
    let rows = store
        .plan_prices_with_metadata_at(now, request.provider_id.as_deref())
        .map_err(|e| store_error(&e))?;
    #[cfg(test)]
    AFTER_PLAN_PRICE_READ.with(|slot| {
        if let Some(write) = slot.take() {
            write();
        }
    });

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
            .map(|(r, (quota, tiers, caps))| {
                let (quota_value, service_tiers, concurrency) = if store.is_store_mode() {
                    (
                        parse_plan_meta(quota)?,
                        parse_plan_meta(tiers)?,
                        parse_plan_meta(caps)?,
                    )
                } else {
                    let row = crate::plan_prices::rows()
                        .iter()
                        .find(|curated| {
                            curated.provider_id == r.provider_id && curated.tier == r.tier
                        })
                        .filter(|_| r.minor_units.is_some());
                    (
                        row.and_then(|r| r.quota_value.clone()),
                        row.and_then(|r| r.service_tiers.clone()),
                        row.and_then(|r| r.concurrency.clone()),
                    )
                };
                Ok(PlanPriceWire {
                    quota_value,
                    service_tiers,
                    concurrency,
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
            })
            .collect::<Result<Vec<_>, RouteError>>()?,
        resolved_at_ms: now,
        tier_vocabulary: crate::plan_prices::TIER_VOCABULARY.to_string(),
        unit_policy: crate::plan_prices::UNIT_POLICY.to_string(),
    })
}

fn parse_plan_meta<T: serde::de::DeserializeOwned>(
    raw: Option<String>,
) -> Result<Option<T>, RouteError> {
    raw.map(|value| {
        serde_json::from_str(&value).map_err(|err| {
            RouteError::unavailable(format!("stored plan metadata is malformed: {err}"))
        })
    })
    .transpose()
}

pub(crate) fn store_error(e: &CatalogError) -> RouteError {
    RouteError::unavailable(format!("catalog read failed: {e}"))
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
mod inheritance_tests;

#[cfg(test)]
mod alias_tests;

#[cfg(test)]
mod billing_plane_tests;

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
    // Keep an existing model readable when a sourced identity, endpoint or
    // added fact can fill an absent fact. Rendering drops this extra presence
    // fact afterward.
    if prefixes.iter().any(|p| {
        "model.canonical_id".starts_with(p.as_str())
            || "wire.api".starts_with(p.as_str())
            || crate::fact_overrides::ADDED_FACTS
                .iter()
                .any(|key| key.starts_with(p.as_str()))
    }) && !out.iter().any(|p| "existence".starts_with(p))
    {
        out.push("existence".to_string());
    }
    if admits_rates(prefixes) && !out.iter().any(|p| p == fusiform_store::prefix::MODEL) {
        out.push(fusiform_store::prefix::MODEL.to_string());
    }
    // A sourced reasoning default is served only while the row's own effort
    // options list it, so a read filtered to the default must also fetch the
    // options it is checked against.
    if prefixes
        .iter()
        .any(|p| "capability.reasoning_default".starts_with(p.as_str()))
        && !out
            .iter()
            .any(|p| "capability.reasoning_options".starts_with(p.as_str()))
    {
        out.push("capability.reasoning_options".to_string());
    }
    out
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
    creators: &crate::creators::Creators,
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

    inherit_rate_for(store, source, creators, &mut facts, model_id);

    serde_json::from_value(facts.get(fact.as_str())?.get("inherited_from")?.clone()).ok()
}

fn inherit_rate_for(
    store: &CatalogStore,
    source: SourceId,
    creators: &crate::creators::Creators,
    facts: &mut curate::Facts,
    model_id: &str,
) {
    let Some((creator, family)) = curate::inheritance_source(creators, facts) else {
        return;
    };
    // Do not turn a missing or unreadable creator card into a refusal of the
    // reseller's own row; it still has established non-price facts to serve.
    let Ok((Some(origin), _)) = store.read_model(source, creator, model_id, None) else {
        return;
    };
    curate::inherit_rates(
        facts,
        creator,
        &family,
        origin
            .facts
            .iter()
            .map(|(k, raw)| (k.as_str(), raw.as_str())),
    );
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
    let known = store
        .model_is_known(source, provider_id, model_id)
        .map_err(|e| store_error(&e))?;
    Ok(curate::alias_applies(known).then_some(alias))
}

/// A named `catalog.get` of an alias id.
///
/// Refuses rather than answering empty in both cases it cannot serve, and says
/// which target it would have served, so the caller can ask for that row.
#[allow(clippy::too_many_arguments)]
fn serve_named_alias(
    store: &CatalogStore,
    source: SourceId,
    provider_id: &str,
    model_id: &str,
    alias: &crate::aliases::Alias,
    at: Option<Timestamp>,
    prefixes: Option<&Vec<String>>,
    tables: &curate::CurationSet<'_>,
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
    aliased_response(
        store,
        source,
        provider_id,
        model_id,
        alias,
        prefixes,
        tables,
    )?
    .ok_or_else(|| {
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
/// Runs the same named read the target would get, including inheritance,
/// then removes route-dependent claims, restricts reasoning efforts to the
/// alias's tier map and marks borrowed rates. Other model facts stay intact.
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
    tables: &curate::CurationSet<'_>,
) -> Result<Option<CatalogGetResponse>, RouteError> {
    let target_request = CatalogGetRequest {
        provider_id: Some(alias.target_provider_id.clone()),
        model_id: Some(alias.target_model_id.clone()),
        fact_prefixes: prefixes.cloned(),
        ..CatalogGetRequest::default()
    };
    // If the store has never recorded the target model, the alias has nothing
    // to stand for and serves nothing. Ask the store directly. Do not infer it
    // from the refusal code `single_model_snapshot` returns (`no_coverage`):
    // that code is reported to callers, and changing how an unknown model is
    // reported must not change whether an alias is served.
    if !store
        .model_is_known(source, &alias.target_provider_id, &alias.target_model_id)
        .map_err(|e| store_error(&e))?
    {
        return Ok(None);
    }
    // The store confirmed the target exists, so any refusal
    // `single_model_snapshot` returns now is a real error and propagates.
    let (snapshot, retired) = single_model_snapshot(
        store,
        source,
        &alias.target_provider_id,
        &alias.target_model_id,
        None,
        &target_request,
        None,
    )?;
    if !retired.is_empty() {
        return Ok(None);
    }

    let wide = prefixes.map(|p| widened_for_inheritance(p));
    let response = render(
        source,
        snapshot,
        None,
        Some((store, tables.creators)),
        wide.as_ref(),
        None,
        Some(tables),
    );
    if response.models.is_empty() {
        return Ok(None);
    }
    let name = read_provider_name(store, source, provider_id, None)?;
    Ok(curate::project_alias(
        response,
        provider_id,
        model_id,
        alias,
        prefixes,
        name.as_deref(),
    ))
}

/// Add alias entries to a current bulk read.
///
/// An alias is included when its provider passes the caller's provider filter
/// and its target is present — even when the target's provider is filtered
/// out, because the caller asked about the ALIAS's provider. Like the bulk
/// read itself, an entry the fact filter leaves empty is dropped.
///
/// Disclosures about the target (`withheld`, `uncertain`) are
/// merged once each, since the bulk read may already carry them for the
/// target's own entry. Fact-override disclosures already name the alias entry,
/// so they are added as they are.
#[allow(clippy::too_many_arguments)]
fn add_aliases_to_bulk(
    store: &CatalogStore,
    source: SourceId,
    aliases: &crate::aliases::Aliases,
    response: &mut CatalogGetResponse,
    provider_filter: Option<&str>,
    prefixes: Option<&Vec<String>>,
    range: Option<&ProviderRange>,
    tables: &curate::CurationSet<'_>,
) -> Result<(), RouteError> {
    for (provider_id, model_id) in aliases.keys() {
        if provider_filter.is_some_and(|p| p != provider_id)
            || range.is_some_and(|r| !r.contains(provider_id))
        {
            continue;
        }
        let Some(alias) = alias_in_effect(store, source, aliases, provider_id, model_id)? else {
            continue;
        };
        let Some(entry) = aliased_response(
            store,
            source,
            provider_id,
            model_id,
            alias,
            prefixes,
            tables,
        )?
        else {
            continue;
        };
        curate::merge_alias(response, entry);
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
        mapping: None,
    }
}

#[cfg(test)]
mod curated_tests;

#[cfg(test)]
mod system_one_tests;
