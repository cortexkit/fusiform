//! Preview and record live operator judgments without changing upstream facts.

use super::*;
use curate::TaskGradeRequirement;
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

fn local_mark_wire(row: fusiform_store::LocalMark) -> ModelMarkWire {
    ModelMarkWire {
        model: row.model,
        decommissioned: row.decommissioned,
        marked_by: "user".into(),
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
    let read = || mark_rows(store, request.model.as_deref());
    let marks = if store.is_store_mode() {
        store
            .with_read_snapshot(read)
            .map_err(|e| store_error(&e))??
    } else {
        read()?
    };
    Ok(QualityMarksResponse { marks })
}

fn mark_rows(store: &CatalogStore, model: Option<&str>) -> Result<Vec<ModelMarkWire>, RouteError> {
    if let Some(model) = model {
        let mut history = store
            .model_mark_history(model)
            .map_err(|e| store_error(&e))?;
        if store.is_store_mode() {
            history.extend(
                store
                    .local_mark_history(model)
                    .map_err(|e| store_error(&e))?,
            );
        }
        if history.is_empty() {
            return Err(RouteError {
                code: "no_coverage",
                message: format!("no model mark history for {model:?}"),
            });
        }
        history.reverse();
        if store.is_store_mode() {
            // The reverse insertion order breaks timestamp ties, putting local marks first.
            history.sort_by_key(|row| std::cmp::Reverse(row.recorded_at_ms));
        }
        return Ok(history.into_iter().map(mark_wire).collect());
    }

    let published = store.model_marks().map_err(|e| store_error(&e))?;
    if !store.is_store_mode() {
        return Ok(published.into_iter().map(mark_wire).collect());
    }
    let local = store
        .local_overrides_at(None)
        .map_err(|e| store_error(&e))?
        .marks;
    let effective: BTreeMap<_, _> = published
        .into_iter()
        .map(mark_wire)
        .chain(local.into_iter().map(local_mark_wire))
        .map(|row| (row.model.clone(), row))
        .collect();
    Ok(effective
        .into_values()
        .filter(|row| row.decommissioned)
        .collect())
}

pub(super) fn mark(
    store: &CatalogStore,
    body: &[u8],
    decommissioned: bool,
) -> Result<QualityMarkWriteResponse, RouteError> {
    let request: QualityMarkRequest = serde_json::from_slice(body)
        .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?;
    if request.withdraw && !store.is_store_mode() {
        return Err(RouteError::bad_request(
            "withdrawing a local mark requires store mode",
        ));
    }
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
    let published = || {
        store
            .model_mark_history(&request.model)
            .map(|mut history| history.pop().map(mark_wire))
            .map_err(|e| store_error(&e))
    };
    let local_override = store
        .local_overrides_at(None)
        .map_err(|e| store_error(&e))?
        .marks
        .into_iter()
        .find(|row| row.model == request.model)
        .map(local_mark_wire);
    if request.withdraw && local_override.is_none() {
        return Err(RouteError::no_coverage(
            "no current local override to withdraw",
        ));
    }
    let current = match local_override {
        Some(row) => Some(row),
        None => published()?,
    };
    let restored = if request.withdraw {
        published()?.is_some_and(|row| row.decommissioned)
    } else {
        decommissioned
    };
    let new = ModelMarkWire {
        model: request.model.clone(),
        decommissioned: restored,
        marked_by: if store.is_store_mode() {
            "user"
        } else {
            "operator"
        }
        .into(),
        reason: request.reason.clone(),
        recorded_at_ms: now_ms(),
    };
    if !request.dry_run {
        if request.withdraw {
            store
                .append_override_mark(&request.model, None, &request.reason, new.recorded_at_ms)
                .map_err(|e| store_error(&e))?;
        } else {
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
        graded_effort: row.graded_effort,
        effort_grade: None,
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
    let read = || grade_rows(store, request.model.as_deref());
    let rows = if store.is_store_mode() {
        store
            .with_read_snapshot(read)
            .map_err(|e| store_error(&e))??
    } else {
        read()?
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

fn grade_rows(
    store: &CatalogStore,
    model: Option<&str>,
) -> Result<Vec<QualityGradeEra>, RouteError> {
    if let Some(model) = model {
        let mut history = store
            .quality_grade_history(model)
            .map_err(|e| store_error(&e))?;
        if store.is_store_mode() {
            history.extend(
                store
                    .local_quality_grade_history(model)
                    .map_err(|e| store_error(&e))?,
            );
        }
        if history.is_empty() {
            return Err(RouteError {
                code: "no_coverage",
                message: format!("no quality grade history for {model:?}"),
            });
        }
        history.reverse();
        if store.is_store_mode() {
            // IDs are only ordered within one table. Break timestamp ties by
            // keeping the newest insertion first, with local rows before published rows.
            history.sort_by_key(|row| std::cmp::Reverse(row.recorded_at_ms));
        }
        return Ok(history);
    }

    let published = store.quality_grades().map_err(|e| store_error(&e))?;
    if !store.is_store_mode() {
        return Ok(published);
    }
    let local = store.local_quality_grades().map_err(|e| store_error(&e))?;
    let by_key: std::collections::BTreeMap<_, _> = published
        .into_iter()
        .chain(local)
        .map(|row| ((row.model.clone(), row.kind.clone()), row))
        .collect();
    Ok(by_key.into_values().collect())
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
    if let Some(effort) = request.effort_grade {
        let Some(QualityGrade::Chat { elo, eq }) = grade else {
            return Err(RouteError::bad_request("effort-grade requires elo and eq"));
        };
        if request.kind.is_some() || request.graded_effort.is_some() {
            return Err(RouteError::bad_request(
                "effort-grade cannot be combined with kind or graded_effort",
            ));
        }
        return super::quality_efforts::grade(
            store,
            request.model,
            effort,
            Some((elo, eq)),
            request.reason,
            request.dry_run,
        );
    }
    if request.graded_effort.is_some()
        && !matches!(
            (&grade, &request.kind),
            (Some(QualityGrade::Chat { .. }), None)
        )
    {
        return Err(RouteError::bad_request(
            "graded_effort requires elo and eq without kind",
        ));
    }
    write(
        store,
        request.model,
        request.kind,
        grade,
        request.reason,
        request.dry_run,
        request.graded_effort,
        false,
    )
}

pub(super) fn withdraw(
    store: &CatalogStore,
    body: &[u8],
) -> Result<QualityWriteResponse, RouteError> {
    let request: QualityWithdrawRequest = serde_json::from_slice(body)
        .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?;
    if let Some(effort) = request.effort_grade {
        if request.kind.is_some() {
            return Err(RouteError::bad_request(
                "effort-grade cannot be combined with kind",
            ));
        }
        return super::quality_efforts::grade(
            store,
            request.model,
            effort,
            None,
            request.reason,
            request.dry_run,
        );
    }
    write(
        store,
        request.model,
        request.kind,
        None,
        request.reason,
        request.dry_run,
        None,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn write(
    store: &CatalogStore,
    model: String,
    kind: Option<String>,
    grade: Option<QualityGrade>,
    reason: String,
    dry_run: bool,
    graded_effort: Option<String>,
    withdraw: bool,
) -> Result<QualityWriteResponse, RouteError> {
    if reason.trim().is_empty() {
        return Err(RouteError::bad_request("reason must not be empty"));
    }
    let requirement = kind
        .as_deref()
        .map(|kind| {
            TaskGradeRequirement::for_kind(kind)
                .map(|requirement| (kind, requirement))
                .ok_or_else(|| {
                    RouteError::bad_request(
                        "kind must be transcription, speech, image, video, search or decision",
                    )
                })
        })
        .transpose()?;
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
    let published = || {
        store
            .quality_grade_history(&model)
            .map(|history| {
                history
                    .into_iter()
                    .rev()
                    .find(|row| row.kind == kind)
                    .filter(|row| row.grade.is_some())
                    .map(wire)
            })
            .map_err(|e| store_error(&e))
    };
    let local_override = store
        .local_overrides_at(None)
        .map_err(|e| store_error(&e))?
        .grades
        .into_iter()
        .find(|row| row.model == model && row.kind == kind)
        .map(wire);
    let current = match local_override {
        None if withdraw && store.is_store_mode() => None,
        Some(local) => Some(local),
        None => published()?,
    };
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
    let mut affected: Vec<_> = catalog
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
    if !withdraw && !local && affected.is_empty() {
        return Err(RouteError::bad_request(format!("unknown quality grade key {model:?}; model.type is unavailable because no current served row matches; use a canonical model id carried by a current served row, or the provider/model row id when it has no canonical id")));
    }
    if let (Some((kind, requirement)), Some(catalog)) = (requirement, &catalog) {
        affected.retain(|id| requirement.matches_row(&catalog.models[id]));
        if !withdraw && affected.is_empty() {
            let message = match requirement {
                TaskGradeRequirement::Contains(fact, required) => format!(
                    "quality grade kind {kind:?} requires {fact} containing {required:?} on at least one current row reached by {model:?}"
                ),
                TaskGradeRequirement::Equals(fact, required) => format!(
                    "quality grade kind {kind} requires {fact}={required} on at least one current row reached by the key"
                ),
            };
            return Err(RouteError::bad_request(message));
        }
    } else if !withdraw && !local {
        let catalog = catalog.as_ref().expect("catalog keys resolve current rows");
        if let Some(effort) = &graded_effort {
            super::quality_efforts::require_effort(catalog, &affected, effort)?;
        }
        if affected.iter().any(|id| {
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
        graded_effort,
        effort_grade: None,
        elo: grade.as_ref().and_then(QualityGrade::elo).cloned(),
        eq: grade.as_ref().and_then(QualityGrade::eq).cloned(),
        grade: grade.as_ref().and_then(QualityGrade::grade).cloned(),
        withdrawn: grade.is_none(),
        graded_by: if store.is_store_mode() {
            "user"
        } else {
            "operator"
        }
        .into(),
        reason: reason.clone(),
        recorded_at_ms: now_ms(),
    };
    if !dry_run {
        store
            .append_quality_grade_with_effort(
                &model,
                kind.as_deref(),
                grade.as_ref(),
                "operator",
                &reason,
                new.recorded_at_ms,
                new.graded_effort.as_deref(),
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
