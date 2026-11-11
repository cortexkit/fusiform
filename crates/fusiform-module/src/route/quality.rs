//! Preview and record live operator judgments without changing upstream facts.

use super::*;
use fusiform_protocol::{
    QualityGradeRequest, QualityGradeWire, QualityGradesRequest, QualityGradesResponse,
    QualityWithdrawRequest, QualityWriteResponse,
};
use fusiform_store::{QualityGrade, QualityGradeEra};

fn wire(row: QualityGradeEra) -> QualityGradeWire {
    QualityGradeWire {
        model: row.model,
        elo: row.grade.as_ref().map(|g| g.elo.clone()),
        eq: row.grade.as_ref().map(|g| g.eq.clone()),
        withdrawn: row.grade.is_none(),
        graded_by: row.graded_by,
        reason: row.reason,
        recorded_at_ms: row.recorded_at_ms,
    }
}

pub(super) fn grades(
    store: &CatalogStore,
    body: &[u8],
) -> Result<QualityGradesResponse, RouteError> {
    let request: QualityGradesRequest = if body.is_empty() {
        QualityGradesRequest::default()
    } else {
        serde_json::from_slice(body)
            .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?
    };
    let rows = if let Some(model) = request.model {
        let mut history = store
            .quality_grade_history(&model)
            .map_err(|e| store_error(&e))?;
        if history.is_empty() {
            return Err(RouteError {
                code: "no_coverage",
                message: format!("no quality grade history for {model:?}"),
            });
        }
        history.reverse();
        history
    } else {
        store.quality_grades().map_err(|e| store_error(&e))?
    };
    Ok(QualityGradesResponse {
        grades: rows.into_iter().map(wire).collect(),
    })
}

pub(super) fn grade(store: &CatalogStore, body: &[u8]) -> Result<QualityWriteResponse, RouteError> {
    let request: QualityGradeRequest = serde_json::from_slice(body)
        .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?;
    for (name, number) in [("elo", &request.elo), ("eq", &request.eq)] {
        if !number.as_f64().is_some_and(|n| n.is_finite() && n > 0.0) {
            return Err(RouteError::bad_request(format!(
                "{name} must be a finite positive number"
            )));
        }
    }
    write(
        store,
        request.model,
        Some(QualityGrade {
            elo: request.elo,
            eq: request.eq,
        }),
        request.reason,
        request.dry_run,
    )
}

pub(super) fn withdraw(
    store: &CatalogStore,
    body: &[u8],
) -> Result<QualityWriteResponse, RouteError> {
    let request: QualityWithdrawRequest = serde_json::from_slice(body)
        .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?;
    write(store, request.model, None, request.reason, request.dry_run)
}

fn write(
    store: &CatalogStore,
    model: String,
    grade: Option<QualityGrade>,
    reason: String,
    dry_run: bool,
) -> Result<QualityWriteResponse, RouteError> {
    if reason.trim().is_empty() {
        return Err(RouteError::bad_request("reason must not be empty"));
    }
    let current = store
        .quality_grade_history(&model)
        .map_err(|e| store_error(&e))?
        .pop()
        .filter(|row| row.grade.is_some())
        .map(wire);
    if grade.is_none() && current.is_none() {
        return Err(RouteError {
            code: "no_coverage",
            message: format!("no current quality grade for {model:?}"),
        });
    }
    // Operator previews need the full served identity set, not the public reply
    // frame budget. Nothing from this read is returned except ten row ids.
    let catalog = serve_catalog_get_with_limits(store, b"{}", usize::MAX, usize::MAX)?;
    let affected: Vec<_> = catalog
        .models
        .iter()
        .filter_map(|(id, facts)| {
            let canonical = facts
                .get("model.canonical_id")
                .and_then(serde_json::Value::as_str);
            if canonical.is_none() && catalog.aliased.iter().any(|a| &a.model == id) {
                return None;
            }
            (canonical.unwrap_or(id) == model).then(|| id.clone())
        })
        .collect();
    if grade.is_some() && affected.is_empty() {
        return Err(RouteError::bad_request(format!("unknown quality grade key {model:?}; use a canonical model id carried by a current served row, or the provider/model row id when it has no canonical id")));
    }
    let new = QualityGradeWire {
        model: model.clone(),
        elo: grade.as_ref().map(|g| g.elo.clone()),
        eq: grade.as_ref().map(|g| g.eq.clone()),
        withdrawn: grade.is_none(),
        graded_by: "operator".into(),
        reason: reason.clone(),
        recorded_at_ms: now_ms(),
    };
    if !dry_run {
        store
            .append_quality_grade(
                &model,
                grade.as_ref(),
                "operator",
                &reason,
                new.recorded_at_ms,
            )
            .map_err(|e| store_error(&e))?;
    }
    Ok(QualityWriteResponse {
        dry_run,
        model,
        current,
        new,
        affected_count: affected.len(),
        affected_ids: affected.into_iter().take(10).collect(),
    })
}
