use super::*;
use fusiform_protocol::{
    QualityGradeWire, QualityWriteResponse, WorkloadEffortRequest, WorkloadEffortWire,
    WorkloadEffortWriteResponse, WORKLOADS,
};
use fusiform_store::{EffortGradeEra, WorkloadEffortEra};
use serde_json::Number;

fn target(
    store: &CatalogStore,
    model: &str,
    reason: &str,
) -> Result<(CatalogGetResponse, Vec<String>), RouteError> {
    if reason.trim().is_empty() {
        return Err(RouteError::bad_request("reason must not be empty"));
    }
    let catalog = serve_catalog_get_with_limits(store, b"{}", usize::MAX, usize::MAX)?;
    let affected = catalog
        .models
        .iter()
        .filter_map(|(id, facts)| {
            let canonical = facts
                .get("model.canonical_id")
                .and_then(serde_json::Value::as_str);
            if canonical.is_none() && catalog.aliased.iter().any(|alias| &alias.model == id) {
                return None;
            }
            (canonical.unwrap_or(id) == model).then(|| id.clone())
        })
        .collect();
    Ok((catalog, affected))
}

pub(super) fn require_effort(
    catalog: &CatalogGetResponse,
    ids: &[String],
    effort: &str,
) -> Result<(), RouteError> {
    if effort.trim().is_empty()
        || !ids.iter().any(|id| {
            curate::fact_overrides::options_list_effort(
                catalog.models[id].get("capability.reasoning_options"),
                effort,
            )
        })
    {
        return Err(RouteError::bad_request(format!("effort {effort:?} is not listed in capability.reasoning_options on any current row reached by this key")));
    }
    Ok(())
}

fn missing() -> RouteError {
    RouteError {
        code: "no_coverage",
        message: "no current judgment for that key".into(),
    }
}

fn grade_wire(row: EffortGradeEra, graded_by: &str) -> QualityGradeWire {
    QualityGradeWire {
        model: row.model,
        kind: None,
        graded_effort: None,
        effort_grade: Some(row.effort),
        withdrawn: row.elo.is_none(),
        elo: row.elo,
        eq: row.eq,
        grade: None,
        graded_by: graded_by.into(),
        reason: row.reason,
        recorded_at_ms: row.recorded_at_ms,
    }
}

pub(super) fn grade(
    store: &CatalogStore,
    model: String,
    effort: String,
    pair: Option<(Number, Number)>,
    reason: String,
    dry_run: bool,
) -> Result<QualityWriteResponse, RouteError> {
    let (catalog, affected) = target(store, &model, &reason)?;
    let local = store
        .local_overrides_at(None)
        .map_err(|e| store_error(&e))?
        .efforts
        .into_iter()
        .find(|row| row.model == model && row.effort == effort);
    let current = if local.is_some() || store.is_store_mode() && pair.is_none() {
        local.map(|row| grade_wire(row, "user"))
    } else {
        store
            .effort_grades_at(None)
            .map_err(|e| store_error(&e))?
            .into_iter()
            .find(|row| row.model == model && row.effort == effort)
            .map(|row| grade_wire(row, "operator"))
    };
    if pair.is_some() {
        require_effort(&catalog, &affected, &effort)?;
    } else if current.is_none() {
        return Err(missing());
    }
    let (elo, eq) = pair
        .map(|(elo, eq)| (Some(elo), Some(eq)))
        .unwrap_or_default();
    let row = EffortGradeEra {
        model: model.clone(),
        effort,
        elo,
        eq,
        reason,
        recorded_at_ms: now_ms(),
    };
    if !dry_run {
        store
            .append_effort_grade(&row, "operator")
            .map_err(|e| store_error(&e))?;
    }
    Ok(QualityWriteResponse {
        dry_run,
        model,
        current,
        new: grade_wire(
            row,
            if store.is_store_mode() {
                "user"
            } else {
                "operator"
            },
        ),
        affected_count: affected.len(),
        affected_ids: affected.into_iter().take(10).collect(),
    })
}

fn workload_wire(row: WorkloadEffortEra) -> WorkloadEffortWire {
    let withdrawn = row.is_withdrawn();
    WorkloadEffortWire {
        model: row.model,
        workload: row.workload,
        withdrawn,
        effort_default: row.effort_default,
        effort_minimum: row.effort_minimum,
        excluded: row.excluded,
        reason: row.reason,
        recorded_at_ms: row.recorded_at_ms,
    }
}

pub(super) fn workload(
    store: &CatalogStore,
    body: &[u8],
) -> Result<WorkloadEffortWriteResponse, RouteError> {
    let request: WorkloadEffortRequest = serde_json::from_slice(body)
        .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?;
    if !WORKLOADS.contains(&request.workload.as_str()) {
        return Err(RouteError::bad_request(
            "workload must be reason, code, synthesize, lookup or quick",
        ));
    }
    let valid = match (request.exclude, request.withdraw) {
        (false, false) => request.effort_default.is_some(),
        (true, false) | (false, true) => {
            request.effort_default.is_none() && request.effort_minimum.is_none()
        }
        (true, true) => false,
    };
    if !valid {
        return Err(RouteError::bad_request(
            "specify a default effort (and optionally minimum), exclude, or withdraw; exclude and withdraw take no effort",
        ));
    }
    let (catalog, affected) = target(store, &request.model, &request.reason)?;
    if request.exclude && affected.is_empty() {
        return Err(RouteError::bad_request(
            "no current row reached by this key",
        ));
    }
    let local = store
        .local_overrides_at(None)
        .map_err(|e| store_error(&e))?
        .workloads
        .into_iter()
        .find(|row| row.model == request.model && row.workload == request.workload);
    let current = if local.is_some() || store.is_store_mode() && request.withdraw {
        local.map(workload_wire)
    } else {
        store
            .workload_efforts_at(None)
            .map_err(|e| store_error(&e))?
            .into_iter()
            .find(|row| row.model == request.model && row.workload == request.workload)
            .map(workload_wire)
    };
    if request.withdraw && current.is_none() {
        return Err(missing());
    }
    if let Some(effort) = &request.effort_default {
        require_effort(&catalog, &affected, effort)?;
    }
    if let Some(effort) = &request.effort_minimum {
        if effort.trim().is_empty()
            || !catalog.models.values().any(|facts| {
                curate::fact_overrides::options_list_effort(
                    facts.get("capability.reasoning_options"),
                    effort,
                )
            })
        {
            return Err(RouteError::bad_request(format!("minimum effort {effort:?} is not listed in capability.reasoning_options on any current catalog row")));
        }
    }
    let row = WorkloadEffortEra {
        model: request.model.clone(),
        workload: request.workload,
        effort_default: request.effort_default,
        effort_minimum: request.effort_minimum,
        excluded: request.exclude,
        reason: request.reason,
        recorded_at_ms: now_ms(),
    };
    if !request.dry_run {
        store
            .append_workload_effort(&row, "operator")
            .map_err(|e| store_error(&e))?;
    }
    Ok(WorkloadEffortWriteResponse {
        dry_run: request.dry_run,
        model: request.model,
        current,
        new: workload_wire(row),
        affected_count: affected.len(),
        affected_ids: affected.into_iter().take(10).collect(),
    })
}
