use std::collections::{BTreeMap, BTreeSet};

use fusiform_release::{ChangeOp, CurationSet};
use fusiform_store::{ImportedTables, ReleaseArrival, StoreRelease};
use serde_json::{json, Value};

use super::state::Verified;

fn add(rows: &mut ImportedTables, table: &str, row: Value) {
    let Value::Object(row) = row else {
        unreachable!("an era row is an object");
    };
    rows.entry(table.to_owned()).or_default().push(row);
}

pub(super) fn arrival(
    previous: Option<&Verified>,
    next: &Verified,
    operations: &[ChangeOp],
    curation_changed: bool,
    last_observation: &mut i64,
) -> Result<ReleaseArrival, String> {
    let mut rows = ImportedTables::new();
    let before = previous.map(|p| &p.state.curation_set);
    let release = &next.envelope;
    if let Some(previous) = previous {
        if !operations.is_empty() {
            *last_observation = last_observation
                .checked_add(1)
                .ok_or("observation id overflow")?;
            add(
                &mut rows,
                "observation",
                json!({"id":last_observation,"source":"models.dev","outcome":"changed",
                "observed_at_ms":release.upstream_observed_at_ms,"normalized_hash":release.upstream_hash.to_string()}),
            );
            for op in operations {
                let (provider, model, fact) = op.key();
                let value = match op {
                    ChangeOp::Set { value, .. } | ChangeOp::SetProvider { value, .. } => {
                        value.to_string()
                    }
                    _ if fact == "existence" => r#""absent""#.to_owned(),
                    _ if fact.starts_with("rate.") => {
                        json!({"state":"unpriced","reason":"missing_rate"}).to_string()
                    }
                    _ => "null".to_owned(),
                };
                add(
                    &mut rows,
                    "era",
                    json!({"source":"models.dev","provider_id":provider,"model_id":model.unwrap_or(""),
                    "fact_key":fact,"value_json":value,"boundary_at_ms":release.upstream_observed_at_ms,
                    "boundary_kind":"observed","observation_id":last_observation,
                    "prior_observation_at_ms":previous.envelope.upstream_observed_at_ms}),
                );
            }
        }
    }
    if curation_changed {
        let curation = &next.state.curation_set;
        let at = release.published_at_ms;
        if let Some(before) = before {
            quality_rows(&mut rows, before, curation, at);
        }
        crate::curation::document_rows(
            &mut rows,
            before.map(|p| &p.vendor_facts),
            &curation.vendor_facts,
            at,
        )?;
    }
    Ok(ReleaseArrival {
        published_at_ms: release.published_at_ms,
        release: StoreRelease {
            release: release.release,
            state_hash: release.state_hash.to_string(),
            curation_hash: release.curation_hash.to_string(),
            applied_at_ms: crate::route::now_ms(),
        },
        rows,
    })
}

pub(crate) fn changes<'a, K: Ord, V: PartialEq>(
    old: Option<&'a BTreeMap<K, V>>,
    new: &'a BTreeMap<K, V>,
) -> impl Iterator<Item = (&'a K, Option<&'a V>)> {
    let keys: BTreeSet<_> = old
        .into_iter()
        .flat_map(|map| map.keys())
        .chain(new.keys())
        .collect();
    keys.into_iter().filter_map(move |key| {
        let old = old.and_then(|map| map.get(key));
        let value = new.get(key);
        (old != value).then_some((key, value))
    })
}

fn nested_changes<'a, V: PartialEq>(
    old: &'a BTreeMap<String, BTreeMap<String, V>>,
    new: &'a BTreeMap<String, BTreeMap<String, V>>,
) -> Vec<(&'a str, &'a str, Option<&'a V>)> {
    let keys: BTreeSet<_> = old.keys().chain(new.keys()).collect();
    let mut out = Vec::new();
    for key in keys {
        let previous = old.get(key);
        if let Some(current) = new.get(key) {
            out.extend(
                changes(previous, current)
                    .map(|(dimension, value)| (key.as_str(), dimension.as_str(), value)),
            );
        } else if let Some(previous) = previous {
            out.extend(
                previous
                    .keys()
                    .map(|dimension| (key.as_str(), dimension.as_str(), None)),
            );
        }
    }
    out
}

fn quality_rows(rows: &mut ImportedTables, old: &CurationSet, new: &CurationSet, at: i64) {
    let reason = format!("published curation version {}", new.version);
    for (model, value) in changes(Some(&old.grades), &new.grades) {
        add(
            rows,
            "quality_grade_era",
            json!({"model":model,"elo":value.map(|g| g.elo.to_string()),
            "eq":value.map(|g| g.eq.to_string()),"graded_effort":value.and_then(|g| g.graded_effort.as_deref()),
            "withdrawn":value.is_none(),"graded_by":"operator","reason":reason,"recorded_at_ms":at}),
        );
    }
    for (model, effort, value) in nested_changes(&old.effort_grades, &new.effort_grades) {
        add(
            rows,
            "effort_grade_era",
            json!({"model":model,"effort":effort,"elo":value.map(|g| g.elo.to_string()),
            "eq":value.map(|g| g.eq.to_string()),"withdrawn":value.is_none(),"graded_by":"operator",
            "reason":reason,"recorded_at_ms":at}),
        );
    }
    for (model, kind, value) in nested_changes(&old.kind_grades, &new.kind_grades) {
        add(
            rows,
            "quality_grade_era",
            json!({"model":model,"kind":kind,"grade":value.map(ToString::to_string),
            "withdrawn":value.is_none(),"graded_by":"operator","reason":reason,"recorded_at_ms":at}),
        );
    }
    for (model, workload, value) in nested_changes(&old.workload_efforts, &new.workload_efforts) {
        add(
            rows,
            "workload_effort_era",
            json!({"model":model,"workload":workload,"effort_default":value.and_then(|g| g.default.as_deref()),
            "effort_minimum":value.and_then(|g| g.minimum.as_deref()),"excluded":value.is_some_and(|g| g.excluded),
            "withdrawn":value.is_none(),"set_by":"operator",
            "reason":reason,"recorded_at_ms":at}),
        );
    }
    for (model, value) in changes(Some(&old.decommissioned), &new.decommissioned) {
        add(
            rows,
            "model_mark_era",
            json!({"model":model,"decommissioned":value.is_some(),"marked_by":"operator",
            "reason":value.map_or(reason.as_str(), |mark| &mark.reason),"recorded_at_ms":at}),
        );
    }
}
