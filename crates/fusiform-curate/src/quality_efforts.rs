use std::collections::BTreeMap;

use fusiform_protocol::{QualityScoresWire, WORKLOADS};
use serde_json::{Number, Value};

use crate::{fact_overrides::options_list_effort, Quality, ServedState};

pub const GRADED_EFFORT: &str = "quality.graded_effort";
pub const ELO_EFFORT: &str = "quality.elo.effort.";
pub const EQ_EFFORT: &str = "quality.eq.effort.";

#[derive(Clone)]
pub struct RecordedEffortGrade {
    pub model: String,
    pub effort: String,
    pub elo: Number,
    pub eq: Number,
    pub recorded_at_ms: i64,
}

#[derive(Clone)]
pub struct RecordedWorkloadEffort {
    pub model: String,
    pub workload: String,
    pub effort_default: String,
    pub effort_minimum: Option<String>,
    pub recorded_at_ms: i64,
}

pub fn is_effort_fact(key: &str) -> bool {
    key == GRADED_EFFORT
        || key.starts_with(ELO_EFFORT)
        || key.starts_with(EQ_EFFORT)
        || WORKLOADS.iter().any(|name| {
            key.strip_prefix("workload.")
                .and_then(|rest| rest.strip_prefix(name))
                .is_some_and(|rest| matches!(rest, ".effort_default" | ".effort_minimum"))
        })
}

pub fn selects_effort_facts(prefix: &str) -> bool {
    GRADED_EFFORT.starts_with(prefix)
        || [ELO_EFFORT, EQ_EFFORT]
            .iter()
            .any(|base| base.starts_with(prefix) || prefix.starts_with(base))
        || WORKLOADS.iter().any(|name| {
            ["effort_default", "effort_minimum"]
                .iter()
                .any(|field| format!("workload.{name}.{field}").starts_with(prefix))
        })
}

pub fn apply_effort_facts(
    quality: &Quality,
    response: &mut ServedState,
    prefixes: Option<&Vec<String>>,
) {
    let graded: BTreeMap<_, _> = quality
        .grades
        .iter()
        .filter_map(|row| {
            row.graded_effort
                .as_deref()
                .map(|effort| (row.model.as_str(), (effort, row.recorded_at_ms)))
        })
        .collect();
    let mut derived = BTreeMap::<_, Vec<_>>::new();
    for row in &quality.efforts {
        derived.entry(row.model.as_str()).or_default().push(row);
    }
    let mut workloads = BTreeMap::<_, Vec<_>>::new();
    for row in &quality.workloads {
        workloads.entry(row.model.as_str()).or_default().push(row);
    }
    if graded.is_empty() && derived.is_empty() && workloads.is_empty() {
        return;
    }
    let mut newest = response
        .quality_scores
        .as_ref()
        .map(|score| score.graded_at_ms);
    for (identity, facts) in &mut response.models {
        let canonical = facts.get("model.canonical_id").and_then(Value::as_str);
        if canonical.is_none()
            && response
                .aliased
                .iter()
                .any(|alias| alias.model == *identity)
        {
            continue;
        }
        let model = canonical.unwrap_or(identity);
        let options = facts.get("capability.reasoning_options");
        let mut additions = Vec::new();
        let mut add = |fact: String, value: Value, effort: &str, at: i64| {
            if prefixes.is_none_or(|prefixes| prefixes.iter().any(|p| fact.starts_with(p)))
                && options_list_effort(options, effort)
            {
                additions.push((fact, value));
                newest = Some(newest.map_or(at, |old| old.max(at)));
            }
        };
        let measured = graded.get(model);
        if let Some(&(effort, at)) = measured {
            add(
                GRADED_EFFORT.into(),
                Value::String(effort.into()),
                effort,
                at,
            );
        }
        for row in derived.get(model).into_iter().flatten() {
            if measured.is_some_and(|(effort, _)| *effort == row.effort) {
                continue;
            }
            for (base, value) in [(ELO_EFFORT, &row.elo), (EQ_EFFORT, &row.eq)] {
                add(
                    format!("{base}{}", row.effort),
                    Value::Number(value.clone()),
                    &row.effort,
                    row.recorded_at_ms,
                );
            }
        }
        for row in workloads.get(model).into_iter().flatten() {
            for (suffix, effort) in [
                ("effort_default", Some(row.effort_default.as_str())),
                ("effort_minimum", row.effort_minimum.as_deref()),
            ] {
                if let Some(effort) = effort {
                    add(
                        format!("workload.{}.{suffix}", row.workload),
                        Value::String(effort.into()),
                        effort,
                        row.recorded_at_ms,
                    );
                }
            }
        }
        facts.extend(additions);
    }
    response.quality_scores = newest.map(|graded_at_ms| QualityScoresWire {
        graded_by: "operator".into(),
        graded_at_ms,
    });
}
