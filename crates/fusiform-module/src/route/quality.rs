//! Preview and record live operator judgments without changing upstream facts.

use super::*;
use fusiform_protocol::{
    ModelMarkWire, QualityGradeRequest, QualityGradeWire, QualityGradesRequest,
    QualityGradesResponse, QualityMarkRequest, QualityMarkWriteResponse, QualityMarksRequest,
    QualityMarksResponse, QualityWithdrawRequest, QualityWriteResponse,
};
use fusiform_store::{QualityGrade, QualityGradeEra};

fn local_key(model: &str) -> Result<bool, RouteError> {
    let Some(key) = model.strip_prefix("local:") else {
        return Ok(false);
    };
    if !crate::model_lineage::valid_model_key(key) {
        return Err(RouteError::bad_request("local engine keys must be local:<provider>/<model> with nonempty parts and no whitespace or control characters"));
    }
    Ok(true)
}

fn mark_wire(row: fusiform_store::ModelMarkEra) -> ModelMarkWire {
    ModelMarkWire {
        model: row.model,
        decommissioned: row.decommissioned,
        marked_by: row.marked_by,
        reason: row.reason,
        recorded_at_ms: row.recorded_at_ms,
    }
}

pub(super) fn marks(store: &CatalogStore, body: &[u8]) -> Result<QualityMarksResponse, RouteError> {
    let request: QualityMarksRequest = if body.is_empty() {
        QualityMarksRequest::default()
    } else {
        serde_json::from_slice(body)
            .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?
    };
    let rows = if let Some(model) = request.model {
        let mut history = store
            .model_mark_history(&model)
            .map_err(|e| store_error(&e))?;
        if history.is_empty() {
            return Err(RouteError {
                code: "no_coverage",
                message: format!("no model mark history for {model:?}"),
            });
        }
        history.reverse();
        history
    } else {
        store.model_marks().map_err(|e| store_error(&e))?
    };
    Ok(QualityMarksResponse {
        marks: rows.into_iter().map(mark_wire).collect(),
    })
}

pub(super) fn mark(
    store: &CatalogStore,
    body: &[u8],
    decommissioned: bool,
) -> Result<QualityMarkWriteResponse, RouteError> {
    let request: QualityMarkRequest = serde_json::from_slice(body)
        .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?;
    if request.model.starts_with("local:") {
        return Err(RouteError::bad_request(
            "local engine keys cannot carry decommission marks; their declaring module owns their availability",
        ));
    }
    if request.reason.trim().is_empty() {
        return Err(RouteError::bad_request("reason must not be empty"));
    }
    // Count all served identities, independent of the public reply frame budget.
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
            (canonical.unwrap_or(id) == request.model).then(|| id.clone())
        })
        .collect();
    if affected.is_empty() {
        return Err(RouteError::bad_request(format!("unknown model mark key {:?}; use a canonical model id carried by a current served row, or the provider/model row id when it has no canonical id", request.model)));
    }
    let current = store
        .model_mark_history(&request.model)
        .map_err(|e| store_error(&e))?
        .pop()
        .map(mark_wire);
    let new = ModelMarkWire {
        model: request.model.clone(),
        decommissioned,
        marked_by: "operator".into(),
        reason: request.reason.clone(),
        recorded_at_ms: now_ms(),
    };
    if !request.dry_run {
        store
            .append_model_mark(
                &request.model,
                decommissioned,
                "operator",
                &request.reason,
                new.recorded_at_ms,
            )
            .map_err(|e| store_error(&e))?;
    }
    Ok(QualityMarkWriteResponse {
        dry_run: request.dry_run,
        model: request.model,
        current,
        new,
        affected_count: affected.len(),
        affected_ids: affected.into_iter().take(10).collect(),
    })
}

fn wire(row: QualityGradeEra) -> QualityGradeWire {
    QualityGradeWire {
        model: row.model,
        kind: row.kind,
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
    if request.model.is_some() && request.prefix.is_some() {
        return Err(RouteError::bad_request("model and prefix are exclusive"));
    }
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
        grades: rows
            .into_iter()
            .filter(|row| {
                request
                    .prefix
                    .as_ref()
                    .is_none_or(|prefix| row.model.starts_with(prefix))
            })
            .map(wire)
            .collect(),
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
        request.kind,
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
        request.kind,
        None,
        request.reason,
        request.dry_run,
        true,
    )
}

fn write(
    store: &CatalogStore,
    model: String,
    kind: Option<String>,
    grade: Option<QualityGrade>,
    reason: String,
    dry_run: bool,
    withdraw: bool,
) -> Result<QualityWriteResponse, RouteError> {
    if reason.trim().is_empty() {
        return Err(RouteError::bad_request("reason must not be empty"));
    }
    let modality = match kind.as_deref() {
        Some("transcription") => Some(("capability.input_modalities", "audio")),
        Some("speech") => Some(("capability.output_modalities", "audio")),
        Some("image") => Some(("capability.output_modalities", "image")),
        Some("video") => Some(("capability.output_modalities", "video")),
        Some("search") => None,
        None => None,
        Some(_) => {
            return Err(RouteError::bad_request(
                "kind must be transcription, speech, image, video or search",
            ))
        }
    };
    let local = local_key(&model)?;
    if local && (kind.is_none() || matches!(&grade, Some(QualityGrade::Chat { .. }))) {
        return Err(RouteError::bad_request(
            "local engine keys require a task kind and cannot carry ELO/EQ",
        ));
    }
    if !withdraw
        && !matches!(
            (&grade, &kind),
            (Some(QualityGrade::Chat { .. }), None) | (Some(QualityGrade::NonChat { .. }), Some(_))
        )
    {
        return Err(RouteError::bad_request(
            "quality.grade requires elo and eq without kind, or grade with a task kind",
        ));
    }
    let current = store
        .quality_grade_history(&model)
        .map_err(|e| store_error(&e))?
        .into_iter()
        .rev()
        .find(|row| row.kind == kind)
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
    let catalog = if local {
        None
    } else {
        Some(serve_catalog_get_with_limits(
            store,
            b"{}",
            usize::MAX,
            usize::MAX,
        )?)
    };
    let affected: Vec<_> = catalog
        .as_ref()
        .into_iter()
        .flat_map(|catalog| {
            catalog.models.iter().filter_map(|(id, facts)| {
                let canonical = facts
                    .get("model.canonical_id")
                    .and_then(serde_json::Value::as_str);
                if canonical.is_none() && catalog.aliased.iter().any(|a| &a.model == id) {
                    return None;
                }
                (!local && canonical.unwrap_or(id) == model).then(|| id.clone())
            })
        })
        .collect();
    if !withdraw && !local {
        let catalog = catalog.as_ref().expect("catalog keys resolve current rows");
        if affected.is_empty() {
            return Err(RouteError::bad_request(format!("unknown quality grade key {model:?}; model.type is unavailable because no current served row matches; use a canonical model id carried by a current served row, or the provider/model row id when it has no canonical id")));
        }
        if let Some((fact, required)) = modality {
            let capable = affected.iter().any(|id| {
                catalog.models[id]
                    .get(fact)
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|values| {
                        values.iter().any(|value| value.as_str() == Some(required))
                    })
            });
            if !capable {
                return Err(RouteError::bad_request(format!(
                    "quality grade kind {:?} requires {fact} containing {required:?} on at least one current row reached by {model:?}", kind.as_deref().unwrap()
                )));
            }
        } else if kind.as_deref() == Some("search") {
            if !affected.iter().any(|id| {
                catalog.models[id]
                    .get("model.type")
                    .and_then(serde_json::Value::as_str)
                    == Some("search")
            }) {
                return Err(RouteError::bad_request("quality grade kind search requires model.type=search on at least one current row reached by the key"));
            }
        } else if affected.iter().any(|id| {
            catalog.models[id]
                .get("model.type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind != "chat")
        }) {
            return Err(RouteError::bad_request(
                "ELO/EQ requires chat rows; media grades require kind and grade",
            ));
        }
    }
    let new = QualityGradeWire {
        model: model.clone(),
        kind: kind.clone(),
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
                kind.as_deref(),
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
