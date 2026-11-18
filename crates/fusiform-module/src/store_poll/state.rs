use fusiform_release::{
    state_hash, upstream_state_hash, verify_envelope, ChangeOp, CurationSet, ProviderFacts,
    Release, ReleaseEnvelope, Snapshot, State, Upstream,
};

use super::{step, ReleaseKeys};

#[derive(Clone)]
pub(super) struct Verified {
    pub envelope: ReleaseEnvelope,
    pub state: State,
}

pub(super) fn verify_snapshot(snapshot: &Snapshot, keys: &ReleaseKeys) -> Result<(), String> {
    step(
        "snapshot envelope signature",
        verify_envelope(&snapshot.envelope, &snapshot.signatures, keys),
    )?;
    check_state(&snapshot.envelope, &snapshot.state)
}

impl Verified {
    pub(super) fn advance(
        previous: Option<&Self>,
        release: &Release,
        keys: &ReleaseKeys,
    ) -> Result<Self, String> {
        let verify = || -> Result<Self, String> {
            let Release {
                envelope,
                change,
                signatures,
            } = release;
            verify_envelope(envelope, signatures, keys).map_err(|e| e.to_string())?;
            if envelope.parent != previous.map(|p| p.envelope.release) {
                return Err("parent does not match the verified chain".into());
            }
            if change.change_hash() != envelope.change_hash {
                return Err("change_hash does not match the release's change".into());
            }
            if change.upstream.is_empty() && change.curation_set.is_none() {
                return Err("release contains no changes".into());
            }
            if let Some(previous) = previous {
                if envelope.published_at_ms < previous.envelope.published_at_ms
                    || envelope.upstream_observed_at_ms < previous.envelope.upstream_observed_at_ms
                    || !change.upstream.is_empty()
                        && envelope.upstream_observed_at_ms
                            == previous.envelope.upstream_observed_at_ms
                {
                    return Err("release times do not advance the verified chain".into());
                }
            }
            let curation_set = match (&change.curation_set, previous) {
                (Some(set), Some(previous))
                    if set.version <= previous.state.curation_set.version =>
                {
                    return Err("curation version does not advance".into());
                }
                (Some(set), _) => set.clone(),
                (None, Some(previous)) => previous.state.curation_set.clone(),
                (None, None) => return Err("release 1 must carry its curation set".into()),
            };
            let mut upstream = previous.map_or_else(Upstream::new, |p| p.state.upstream.clone());
            apply_ops(&mut upstream, &change.upstream)?;
            let state = State {
                upstream,
                curation_set,
            };
            check_state(envelope, &state)?;
            if change.curation_set.is_some() {
                validate_judgments(&state.curation_set)?;
                crate::curation::validate_published(
                    &state.curation_set.vendor_facts,
                    &state.upstream,
                )?;
            }
            Ok(Self {
                envelope: envelope.clone(),
                state,
            })
        };
        step(
            &format!("verify release {}", release.envelope.release),
            verify(),
        )
    }
}

fn check_state(envelope: &ReleaseEnvelope, state: &State) -> Result<(), String> {
    if state.curation_set.version != envelope.curation_version {
        return Err("curation version does not match envelope".into());
    }
    let upstream = upstream_state_hash(&state.upstream);
    if upstream != envelope.upstream_hash {
        return Err("upstream_hash does not match verified state".into());
    }
    let curation = state.curation_set.curation_hash();
    if curation != envelope.curation_hash {
        return Err("curation_hash does not match verified state".into());
    }
    if state_hash(curation, upstream) != envelope.state_hash {
        return Err("state_hash does not match verified state".into());
    }
    Ok(())
}

fn apply_ops(upstream: &mut Upstream, ops: &[ChangeOp]) -> Result<(), String> {
    if !ops.windows(2).all(|pair| pair[0].key() < pair[1].key()) {
        return Err("upstream operations are not strictly sorted".into());
    }
    for op in ops {
        let (provider, model, key) = op.key();
        if provider.is_empty() || model == Some("") || key.is_empty() {
            return Err(
                "upstream operations must name a provider and a fact; model ids cannot be empty"
                    .into(),
            );
        }
        let remove = match op {
            ChangeOp::Set { value, .. } | ChangeOp::SetProvider { value, .. } => {
                let provider =
                    upstream
                        .entry(provider.to_owned())
                        .or_insert_with(|| ProviderFacts {
                            provider: Default::default(),
                            models: Default::default(),
                        });
                let facts = match model {
                    Some(model) => provider.models.entry(model.to_owned()).or_default(),
                    None => &mut provider.provider,
                };
                facts.insert(key.to_owned(), value.clone());
                continue;
            }
            ChangeOp::Remove { .. } | ChangeOp::RemoveProvider { .. } => true,
        };
        if remove {
            let provider_row = upstream
                .get_mut(provider)
                .ok_or_else(|| format!("cannot remove missing upstream key {:?}", op.key()))?;
            let facts = match model {
                Some(model) => provider_row
                    .models
                    .get_mut(model)
                    .ok_or_else(|| format!("cannot remove missing upstream key {:?}", op.key()))?,
                None => &mut provider_row.provider,
            };
            if facts.remove(key).is_none() {
                return Err(format!("cannot remove missing upstream key {:?}", op.key()));
            }
            if facts.is_empty() {
                if let Some(model) = model {
                    provider_row.models.remove(model);
                }
            }
            if provider_row.provider.is_empty() && provider_row.models.is_empty() {
                upstream.remove(provider);
            }
        }
    }
    Ok(())
}

fn validate_judgments(set: &CurationSet) -> Result<(), String> {
    let model_keys = set
        .grades
        .keys()
        .chain(set.effort_grades.keys())
        .chain(set.workload_efforts.keys())
        .chain(set.decommissioned.keys())
        .map(|model| (model, false))
        .chain(set.kind_grades.keys().map(|model| (model, true)));
    for (model, task) in model_keys {
        if model.is_empty()
            || !task && model.starts_with("local:")
            || model.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(format!("invalid published model key {model:?}"));
        }
    }
    let valid_number =
        |number: &serde_json::Number| number.as_f64().is_some_and(|n| n > 0.0 && n.is_finite());
    for (model, grade) in &set.grades {
        if grade
            .graded_effort
            .as_deref()
            .is_some_and(|effort| effort.trim().is_empty())
            || !valid_number(&grade.elo)
            || !valid_number(&grade.eq)
        {
            return Err(format!("invalid chat grade for {model}"));
        }
    }
    for (model, efforts) in &set.effort_grades {
        if efforts.iter().any(|(effort, grade)| {
            effort.trim().is_empty() || !valid_number(&grade.elo) || !valid_number(&grade.eq)
        }) {
            return Err(format!("invalid effort grade for {model}"));
        }
    }
    for (model, kinds) in &set.kind_grades {
        if kinds.iter().any(|(kind, grade)| {
            !matches!(
                kind.as_str(),
                "transcription" | "speech" | "image" | "video" | "search"
            ) || !valid_number(grade)
        }) {
            return Err(format!("invalid task grade for {model}"));
        }
    }
    for (model, workloads) in &set.workload_efforts {
        if workloads.iter().any(|(name, effort)| {
            !fusiform_protocol::WORKLOADS.contains(&name.as_str())
                || effort.default.trim().is_empty()
                || effort
                    .minimum
                    .as_deref()
                    .is_some_and(|effort| effort.trim().is_empty())
        }) {
            return Err(format!("invalid workload effort for {model}"));
        }
    }
    Ok(())
}
