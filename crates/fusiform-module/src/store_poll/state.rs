use fusiform_release::{
    state_hash, upstream_state_hash, verify_envelope, ChangeOp, CurationSet, ProviderFacts,
    Release, ReleaseEnvelope, Snapshot, State, Upstream,
};

use super::{step, ReleaseKeys};

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
    pub(super) fn from_snapshot(snapshot: Snapshot, keys: &ReleaseKeys) -> Result<Self, String> {
        verify_snapshot(&snapshot, keys)?;
        Ok(Self {
            envelope: snapshot.envelope,
            state: snapshot.state,
        })
    }

    pub(super) fn first(release: Release, keys: &ReleaseKeys) -> Result<Self, String> {
        let name = format!("verify release {}", release.envelope.release);
        step(
            &name,
            (|| {
                verify_change(None, &release, keys)?;
                let mut upstream = Upstream::new();
                apply_ops(&mut upstream, release.change.upstream)?;
                let curation_set = release
                    .change
                    .curation_set
                    .ok_or("release 1 must carry its curation set")?;
                Self::build(
                    release.envelope,
                    State {
                        upstream,
                        curation_set,
                    },
                    true,
                )
            })(),
        )
    }

    pub(super) fn advance(
        previous: &Self,
        release: &Release,
        keys: &ReleaseKeys,
    ) -> Result<Self, String> {
        let verify = || {
            verify_change(Some(previous), release, keys)?;
            let curation_set = match &release.change.curation_set {
                Some(set) if set.version <= previous.state.curation_set.version => {
                    return Err("curation version does not advance".into());
                }
                Some(set) => set.clone(),
                None => previous.state.curation_set.clone(),
            };
            let mut upstream = previous.state.upstream.clone();
            apply_ops(&mut upstream, release.change.upstream.iter().cloned())?;
            Self::build(
                release.envelope.clone(),
                State {
                    upstream,
                    curation_set,
                },
                release.change.curation_set.is_some(),
            )
        };
        step(
            &format!("verify release {}", release.envelope.release),
            verify(),
        )
    }

    fn build(
        envelope: ReleaseEnvelope,
        state: State,
        curation_changed: bool,
    ) -> Result<Self, String> {
        check_state(&envelope, &state)?;
        if curation_changed {
            validate_judgments(&state.curation_set)?;
            crate::curation::validate_published(&state.curation_set.vendor_facts, &state.upstream)?;
        }
        Ok(Self { envelope, state })
    }
}

fn verify_change(
    previous: Option<&Verified>,
    release: &Release,
    keys: &ReleaseKeys,
) -> Result<(), String> {
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
    if !change
        .upstream
        .windows(2)
        .all(|pair| pair[0].key() < pair[1].key())
    {
        return Err("upstream operations are not strictly sorted".into());
    }
    if previous.is_some_and(|previous| {
        !envelope.chronology_follows(&previous.envelope, !change.upstream.is_empty())
    }) {
        return Err("release times do not advance the verified chain".into());
    }
    Ok(())
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

fn empty_provider() -> ProviderFacts {
    ProviderFacts {
        provider: Default::default(),
        models: Default::default(),
    }
}

fn apply_ops(
    upstream: &mut Upstream,
    ops: impl IntoIterator<Item = ChangeOp>,
) -> Result<(), String> {
    for op in ops {
        let (provider, model, key) = op.key();
        if provider.is_empty() || model == Some("") || key.is_empty() {
            return Err(
                "upstream operations must name a provider and a fact; model ids cannot be empty"
                    .into(),
            );
        }
        match op {
            ChangeOp::Set {
                provider,
                model,
                fact,
                value,
            } => {
                let row = upstream.entry(provider).or_insert_with(empty_provider);
                row.models.entry(model).or_default().insert(fact, value);
            }
            ChangeOp::SetProvider {
                provider,
                fact,
                value,
            } => {
                let row = upstream.entry(provider).or_insert_with(empty_provider);
                row.provider.insert(fact, value);
            }
            ChangeOp::Remove { .. } | ChangeOp::RemoveProvider { .. } => {
                let missing = || format!("cannot remove missing upstream key {:?}", op.key());
                let row = upstream.get_mut(provider).ok_or_else(missing)?;
                let facts = match model {
                    Some(model) => row.models.get_mut(model).ok_or_else(missing)?,
                    None => &mut row.provider,
                };
                facts.remove(key).ok_or_else(missing)?;
                if facts.is_empty() {
                    if let Some(model) = model {
                        row.models.remove(model);
                    }
                }
                if row.provider.is_empty() && row.models.is_empty() {
                    upstream.remove(provider);
                }
            }
        }
    }
    Ok(())
}

/// Check the keys, grade values and workload names in a curation set.
pub fn validate_judgments(set: &CurationSet) -> Result<(), String> {
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
                || effort
                    .default
                    .as_deref()
                    .is_some_and(|effort| effort.trim().is_empty())
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
