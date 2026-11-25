use fusiform_release::{
    state_hash, upstream_state_hash, validate_judgments, validate_received, validate_upstream_key,
    verify_envelope, ChangeOp, ProviderFacts, Release, ReleaseEnvelope, Snapshot, State, Upstream,
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
                apply_ops(&mut upstream, &release.change.upstream)?;
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
            apply_ops(&mut upstream, &release.change.upstream)?;
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
            validate_received(&state.curation_set.vendor_facts)?;
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

fn apply_ops(upstream: &mut Upstream, ops: &[ChangeOp]) -> Result<(), String> {
    for group in ops.chunk_by(|left, right| left.key().0 == right.key().0) {
        let provider = group[0].key().0;
        let had_models = upstream
            .get(provider)
            .is_some_and(|row| !row.models.is_empty());
        let adds_model = group.iter().any(|op| matches!(op, ChangeOp::Set { .. }));
        for op in group {
            let key = op.key();
            validate_upstream_key(key)?;
            let (_, model, key) = key;
            match op {
                ChangeOp::Set {
                    model, fact, value, ..
                } => {
                    let row = upstream
                        .entry(provider.to_owned())
                        .or_insert_with(empty_provider);
                    row.models
                        .entry(model.clone())
                        .or_default()
                        .insert(fact.clone(), value.clone());
                }
                ChangeOp::SetProvider { fact, value, .. } => {
                    if !had_models && !adds_model {
                        return Err(format!("set_provider names provider {provider:?} with no existing or incoming models"));
                    }
                    let row = upstream
                        .entry(provider.to_owned())
                        .or_insert_with(empty_provider);
                    row.provider.insert(fact.clone(), value.clone());
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
                }
            }
        }
        // A batch can remove one model and add another for the same provider.
        // Only reject an orphan if the provider still has no models afterward.
        if let Some(row) = upstream.get(provider).filter(|row| row.models.is_empty()) {
            if !row.provider.is_empty() {
                return Err(format!(
                    "change leaves provider facts without models for {provider:?}"
                ));
            }
            upstream.remove(provider);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(upstream: &mut Upstream, json: &str) -> Result<(), String> {
        apply_ops(
            upstream,
            &serde_json::from_str::<Vec<ChangeOp>>(json).unwrap(),
        )
    }

    #[test]
    fn provider_level_replay_refuses_a_set_without_models() {
        let mut upstream = Upstream::new();
        let failure = apply(
            &mut upstream,
            r#"[{"op":"set_provider","provider":"p","fact":"name","value":"P"}]"#,
        )
        .expect_err("consumer accepted a provider set without existing or incoming models");
        assert!(
            failure.contains("no existing or incoming models"),
            "{failure}"
        );
    }

    #[test]
    fn provider_level_replay_refuses_facts_orphaned_by_last_model_removal() {
        let mut upstream = Upstream::new();
        apply(
            &mut upstream,
            r#"[{"op":"set_provider","provider":"p","fact":"name","value":"P"},
                {"op":"set","provider":"p","model":"m","fact":"name","value":"M"}]"#,
        )
        .unwrap();
        let failure = apply(
            &mut upstream,
            r#"[{"op":"remove","provider":"p","model":"m","fact":"name"}]"#,
        )
        .expect_err("consumer retained the provider's facts after removing its last model");
        assert!(
            failure.contains("leaves provider facts without models"),
            "{failure}"
        );
    }

    #[test]
    fn provider_level_replay_allows_replacing_models_and_removing_the_provider() {
        let mut upstream = Upstream::new();
        apply(
            &mut upstream,
            r#"[{"op":"set_provider","provider":"p","fact":"name","value":"P"},
                {"op":"set","provider":"p","model":"m","fact":"name","value":"M"}]"#,
        )
        .unwrap();
        apply(
            &mut upstream,
            r#"[{"op":"remove","provider":"p","model":"m","fact":"name"},
                {"op":"set","provider":"p","model":"n","fact":"name","value":"N"}]"#,
        )
        .unwrap();
        assert!(!upstream["p"].provider.is_empty());
        assert_eq!(upstream["p"].models.keys().collect::<Vec<_>>(), ["n"]);
        apply(
            &mut upstream,
            r#"[{"op":"remove_provider","provider":"p","fact":"name"},
                {"op":"remove","provider":"p","model":"n","fact":"name"}]"#,
        )
        .unwrap();
        assert!(upstream.is_empty());
    }
}
