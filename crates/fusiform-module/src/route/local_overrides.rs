use super::*;
use fusiform_protocol::LocalOverrideWire;
use fusiform_store::LocalOverrides;

type Metadata<'a> = BTreeMap<String, BTreeMap<String, (i64, &'a str)>>;

fn model_key<'a>(id: &'a str, facts: &'a curate::Facts) -> &'a str {
    facts
        .get("model.canonical_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(id)
}

pub(super) fn overlay<'a>(
    published: &curate::Quality,
    local: &'a LocalOverrides,
) -> (curate::Quality, Metadata<'a>) {
    let mut quality = published.clone();
    let mut metadata = Metadata::new();
    let mut record = |model: &str, fact: String, at: i64, reason: &'a str| {
        if !model.starts_with("local:") {
            metadata
                .entry(model.to_string())
                .or_default()
                .insert(fact, (at, reason));
        }
    };
    for row in &local.grades {
        let Some(grade) = row.grade.clone() else {
            continue;
        };
        quality
            .grades
            .retain(|g| g.model != row.model || g.kind != row.kind);
        let grade = match grade {
            fusiform_store::QualityGrade::Chat { elo, eq } => {
                for fact in ["quality.elo", "quality.eq", "quality.graded_effort"] {
                    record(&row.model, fact.into(), row.recorded_at_ms, &row.reason);
                }
                if let Some(effort) = &row.graded_effort {
                    for prefix in [curate::ELO_EFFORT, curate::EQ_EFFORT] {
                        record(
                            &row.model,
                            format!("{prefix}{effort}"),
                            row.recorded_at_ms,
                            &row.reason,
                        );
                    }
                }
                curate::Grade::Chat { elo, eq }
            }
            fusiform_store::QualityGrade::NonChat { grade } => {
                if let Some(kind) = &row.kind {
                    record(
                        &row.model,
                        format!("quality.grade.{kind}"),
                        row.recorded_at_ms,
                        &row.reason,
                    );
                }
                curate::Grade::NonChat { grade }
            }
        };
        quality.grades.push(curate::RecordedGrade {
            model: row.model.clone(),
            kind: row.kind.clone(),
            grade,
            graded_effort: row.graded_effort.clone(),
            recorded_at_ms: row.recorded_at_ms,
        });
    }
    for row in &local.efforts {
        let (Some(elo), Some(eq)) = (row.elo.clone(), row.eq.clone()) else {
            continue;
        };
        quality
            .efforts
            .retain(|g| g.model != row.model || g.effort != row.effort);
        for prefix in [curate::ELO_EFFORT, curate::EQ_EFFORT] {
            record(
                &row.model,
                format!("{prefix}{}", row.effort),
                row.recorded_at_ms,
                &row.reason,
            );
        }
        quality.efforts.push(curate::RecordedEffortGrade {
            model: row.model.clone(),
            effort: row.effort.clone(),
            elo,
            eq,
            recorded_at_ms: row.recorded_at_ms,
        });
    }
    for row in &local.workloads {
        quality
            .workloads
            .retain(|g| g.model != row.model || g.workload != row.workload);
        for suffix in ["effort_default", "effort_minimum", "excluded"] {
            record(
                &row.model,
                format!("workload.{}.{suffix}", row.workload),
                row.recorded_at_ms,
                &row.reason,
            );
        }
        quality.workloads.push(curate::RecordedWorkloadEffort {
            model: row.model.clone(),
            workload: row.workload.clone(),
            effort_default: row.effort_default.clone(),
            effort_minimum: row.effort_minimum.clone(),
            excluded: row.excluded,
            recorded_at_ms: row.recorded_at_ms,
        });
    }
    for row in &local.marks {
        if row.decommissioned {
            quality.marks.insert(row.model.clone(), row.recorded_at_ms);
        } else {
            quality.marks.remove(&row.model);
        }
        record(
            &row.model,
            "quality.decommissioned".into(),
            row.recorded_at_ms,
            &row.reason,
        );
    }
    (quality, metadata)
}

pub(super) fn comparison_rows(
    source: SourceId,
    local: &CatalogGetResponse,
    metadata: &Metadata<'_>,
) -> CatalogGetResponse {
    let mut published = curate::response_envelope(
        source,
        local.resolved_at_ms,
        local.catalog_version,
        Vec::new(),
        Vec::new(),
    );
    if !metadata.is_empty() {
        published.models = local
            .models
            .iter()
            .filter(|(id, facts)| metadata.contains_key(model_key(id, facts)))
            .map(|(id, facts)| (id.clone(), facts.clone()))
            .collect();
        // A judgment can join an alias by its canonical key, but not by its alias ID.
        published.aliased.extend(
            local
                .aliased
                .iter()
                .filter(|row| published.models.contains_key(&row.model))
                .cloned(),
        );
    }
    published
}

pub(super) fn disclose(
    published: &CatalogGetResponse,
    local: &mut CatalogGetResponse,
    metadata: &Metadata<'_>,
) {
    let mut disclosures = BTreeMap::new();
    for (id, previous) in &published.models {
        let Some(facts) = local.models.get(id) else {
            continue;
        };
        let key = model_key(id, facts);
        let Some(entries) = metadata.get(key) else {
            continue;
        };
        for (fact, (at, reason)) in entries {
            let published = previous.get(fact);
            let local = facts.get(fact);
            if published.is_none() && local.is_none() {
                continue;
            }
            disclosures.insert(
                (key, fact),
                LocalOverrideWire {
                    key: key.to_owned(),
                    fact: fact.clone(),
                    published: published.cloned(),
                    local: local.cloned(),
                    set_at_ms: *at,
                    reason: (*reason).to_string(),
                },
            );
        }
    }
    local.overrides = disclosures.into_values().collect();
}
