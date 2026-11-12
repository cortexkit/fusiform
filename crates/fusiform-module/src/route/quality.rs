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
        elo: row.grade.as_ref().and_then(QualityGrade::elo).cloned(),
        eq: row.grade.as_ref().and_then(QualityGrade::eq).cloned(),
        grade: row.grade.as_ref().and_then(QualityGrade::grade).cloned(),
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
    for (name, number) in [
        ("elo", &request.elo),
        ("eq", &request.eq),
        ("grade", &request.grade),
    ] {
        let Some(number) = number else { continue };
        if !number.as_f64().is_some_and(|n| n.is_finite() && n > 0.0) {
            return Err(RouteError::bad_request(format!(
                "{name} must be a finite positive number"
            )));
        }
    }
    let grade = match (request.elo, request.eq, request.grade) {
        (Some(elo), Some(eq), None) => Some(QualityGrade::Chat { elo, eq }),
        (None, None, Some(grade)) => Some(QualityGrade::NonChat { grade }),
        _ => None,
    };
    write(
        store,
        request.model,
        grade,
        request.reason,
        request.dry_run,
        false,
    )
}

pub(super) fn withdraw(
    store: &CatalogStore,
    body: &[u8],
) -> Result<QualityWriteResponse, RouteError> {
    let request: QualityWithdrawRequest = serde_json::from_slice(body)
        .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?;
    write(
        store,
        request.model,
        None,
        request.reason,
        request.dry_run,
        true,
    )
}

fn write(
    store: &CatalogStore,
    model: String,
    grade: Option<QualityGrade>,
    reason: String,
    dry_run: bool,
    withdraw: bool,
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
    if withdraw && current.is_none() {
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
    if !withdraw {
        if affected.is_empty() {
            return Err(RouteError::bad_request(format!("unknown quality grade key {model:?}; model.type is unavailable because no current served row matches; use a canonical model id carried by a current served row, or the provider/model row id when it has no canonical id")));
        }
        for id in &affected {
            let kind = catalog.models[id]
                .get("model.type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("chat");
            let valid = matches!((&grade, kind), (Some(QualityGrade::Chat { .. }), "chat"))
                || matches!((&grade, kind), (Some(QualityGrade::NonChat { .. }), kind) if kind != "chat");
            if !valid {
                let expected = if kind == "chat" {
                    "elo and eq only (no grade)"
                } else {
                    "grade only (no elo or eq)"
                };
                return Err(RouteError::bad_request(format!(
                    "row {id:?} has model.type={kind:?}; quality.grade requires {expected}"
                )));
            }
        }
    }
    let new = QualityGradeWire {
        model: model.clone(),
        elo: grade.as_ref().and_then(QualityGrade::elo).cloned(),
        eq: grade.as_ref().and_then(QualityGrade::eq).cloned(),
        grade: grade.as_ref().and_then(QualityGrade::grade).cloned(),
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
