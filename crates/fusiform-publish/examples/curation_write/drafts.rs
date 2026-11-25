use std::collections::BTreeMap;

use fusiform_curate::{
    aliases, curated_models, fact_overrides::options_list_effort, model_lineage::valid_model_key,
};
use fusiform_module::store_poll::validate_judgments;
use fusiform_release::{
    CurationSet, Decommissioned, EffortGrade, Facts, Grade, State, WorkloadEffort,
};
use serde::de::DeserializeOwned;
use serde_json::{Number, Value, value::RawValue};

use super::{Request, Result, draft_path, json, metadata};

pub(super) struct Input {
    key: String,
    proposal: Proposal,
    note: String,
}

// Each proposal is the value of one model-key entry, not the entire family map.
enum Proposal {
    Grades(Option<Grade>),
    EffortGrades(Option<BTreeMap<String, EffortGrade>>),
    KindGrades(Option<BTreeMap<String, Number>>),
    WorkloadEfforts(Option<BTreeMap<String, WorkloadEffort>>),
    Decommissioned(Option<Decommissioned>),
}

impl Input {
    pub(super) fn new(family: &str, key: &str, text: &str, note: String) -> Result<Self> {
        let proposal = Proposal::parse(family, text)?;
        check_key(key)?;
        if key.starts_with("local:") && !matches!(&proposal, Proposal::KindGrades(_)) {
            return Err("local engine keys only support kind_grades drafts".into());
        }
        metadata("note", &note)?;
        Ok(Self {
            key: key.into(),
            proposal,
            note,
        })
    }

    pub(super) fn prepare(self, state: &State) -> Result<Request> {
        let (_, request) = self.compare(state)?;
        Ok(request)
    }

    fn compare(self, state: &State) -> Result<(bool, Request)> {
        let family = self.proposal.family();
        if !self.key.starts_with("local:") {
            let options = model_options(state, &self.key)?;
            for effort in self.proposal.efforts() {
                if !options
                    .iter()
                    .any(|option| options_list_effort(Some(option), effort))
                {
                    return Err(format!(
                        "{}: effort {effort:?} is not listed in capability.reasoning_options for {}",
                        family, self.key
                    ));
                }
            }
        }
        let (current, value) = self
            .proposal
            .checked_values(&state.curation_set, &self.key)?;
        let changed = current != value;
        Ok((
            changed,
            Request {
                path: draft_path(family, &self.key),
                body: json(serde_json::json!({ "value": value, "note": self.note })),
                summary: format!(
                    "{family} {}:\n  Store: {current}\n  Proposed: {value}",
                    self.key
                ),
                subdirectory: None,
            },
        ))
    }
}

impl Proposal {
    fn parse(family: &str, text: &str) -> Result<Self> {
        let raw: &RawValue =
            serde_json::from_str(text).map_err(|error| format!("value: {error}"))?;
        let text = raw.get();
        if text.len() > 65_536 || !(text.starts_with('{') || text == "null") {
            return Err("value must be a JSON object of at most 64 KiB, or null".into());
        }
        fn entry<T: DeserializeOwned>(family: &str, text: &str) -> Result<Option<T>> {
            serde_json::from_str(text).map_err(|error| format!("{family} value: {error}"))
        }
        Ok(match family {
            "grades" => Self::Grades(entry(family, text)?),
            "effort_grades" => Self::EffortGrades(entry(family, text)?),
            "kind_grades" => Self::KindGrades(entry(family, text)?),
            "workload_efforts" => Self::WorkloadEfforts(entry(family, text)?),
            "decommissioned" => Self::Decommissioned(entry(family, text)?),
            _ => return Err(format!("unknown draft family {family:?}")),
        })
    }

    fn family(&self) -> &'static str {
        match self {
            Self::Grades(_) => "grades",
            Self::EffortGrades(_) => "effort_grades",
            Self::KindGrades(_) => "kind_grades",
            Self::WorkloadEfforts(_) => "workload_efforts",
            Self::Decommissioned(_) => "decommissioned",
        }
    }

    fn efforts(&self) -> Vec<&str> {
        match self {
            Self::Grades(Some(grade)) => grade.graded_effort.as_deref().into_iter().collect(),
            Self::EffortGrades(Some(grades)) => grades.keys().map(String::as_str).collect(),
            Self::WorkloadEfforts(Some(workloads)) => workloads
                .values()
                .flat_map(|row| {
                    [row.default.as_deref(), row.minimum.as_deref()]
                        .into_iter()
                        .flatten()
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn checked_values(self, current: &CurationSet, key: &str) -> Result<(Value, Value)> {
        let mut proposed = current.clone();
        fn replace<T: serde::Serialize>(
            map: &mut BTreeMap<String, T>,
            key: &str,
            proposal: Option<T>,
        ) -> (Value, Value) {
            let new = serde_json::to_value(&proposal).expect("draft entry is JSON");
            let old = match proposal {
                Some(value) => map.insert(key.to_owned(), value),
                None => map.remove(key),
            };
            (
                serde_json::to_value(&old).expect("stored entry is JSON"),
                new,
            )
        }
        let pair = match self {
            Self::Grades(value) => replace(&mut proposed.grades, key, value),
            Self::EffortGrades(value) => replace(&mut proposed.effort_grades, key, value),
            Self::KindGrades(value) => replace(&mut proposed.kind_grades, key, value),
            Self::WorkloadEfforts(value) => replace(&mut proposed.workload_efforts, key, value),
            Self::Decommissioned(value) => replace(&mut proposed.decommissioned, key, value),
        };
        validate_judgments(&proposed)?;
        if json(&pair.1).len() > 65_536 {
            return Err("serialized draft value exceeds 64 KiB".into());
        }
        Ok(pair)
    }
}

pub(super) struct LocalInput(Vec<Input>);

impl LocalInput {
    pub(super) fn new(key: String, text: &str) -> Result<Self> {
        check_key(&key)?;
        let mut fields: BTreeMap<String, Box<RawValue>> =
            serde_json::from_str(text).map_err(|error| format!("local judgments: {error}"))?;
        let note = fields
            .remove("note")
            .ok_or("local judgments require a note")?;
        let note: String =
            serde_json::from_str(note.get()).map_err(|error| format!("note: {error}"))?;
        metadata("note", &note)?;
        let inputs = fields
            .into_iter()
            .map(|(family, value)| Input::new(&family, &key, value.get(), note.clone()))
            .collect::<Result<_>>()?;
        Ok(Self(inputs))
    }

    pub(super) fn prepare(self, state: &State) -> Result<Vec<Request>> {
        let mut requests = Vec::new();
        for input in self.0 {
            let family = input.proposal.family();
            let (changed, mut request) = input.compare(state)?;
            if changed {
                request.subdirectory = Some(family);
                requests.push(request);
            }
        }
        Ok(requests)
    }
}

fn check_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 200 || key.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(
            "model key must be 1–200 bytes without whitespace or control characters".into(),
        );
    }
    if key
        .strip_prefix("local:")
        .is_some_and(|id| !valid_model_key(id))
    {
        return Err("local engine keys must be local:<provider>/<model> with nonempty parts and no whitespace or control characters".into());
    }
    Ok(())
}

const OPTIONS: &str = "capability.reasoning_options";

fn model_options(state: &State, key: &str) -> Result<Vec<Value>> {
    let documents = &state.curation_set.vendor_facts;
    let curated = documents
        .get("curated-models")
        .map_or_else(curated_models::Table::default, |doc| {
            curated_models::load_table_reporting_rejects(&doc.to_string()).0
        });
    let mut found = false;
    let mut options = Vec::new();
    let mut collect = |provider: &str, model: &str, facts: &Facts| {
        if matches_id(key, provider, model)
            || facts.get("model.canonical_id").and_then(Value::as_str) == Some(key)
        {
            found = true;
            options.extend(facts.get(OPTIONS).cloned());
        }
    };
    for (provider, row) in &state.upstream {
        for (model, facts) in &row.models {
            collect(provider, model, facts);
        }
    }
    for ((provider, model), row) in &curated.models {
        if !curated
            .held
            .contains_key(&(provider.clone(), model.clone()))
        {
            collect(provider, model, &row.facts);
        }
    }
    if !found && let Some(doc) = documents.get("model-aliases") {
        for ((provider, model), alias) in aliases::load_reporting_rejects(&doc.to_string()).0 {
            if !matches_id(key, &provider, &model) {
                continue;
            }
            let target = (alias.target_provider_id, alias.target_model_id);
            let target_facts = state
                .upstream
                .get(&target.0)
                .and_then(|row| row.models.get(&target.1))
                .or_else(|| {
                    (!curated.held.contains_key(&target))
                        .then(|| curated.models.get(&target))
                        .flatten()
                        .map(|row| &row.facts)
                });
            if let Some(facts) = target_facts {
                found = true;
                if let Some(target_options) = facts.get(OPTIONS) {
                    if alias.reasoning_wire_ids.is_empty() {
                        options.push(target_options.clone());
                    } else {
                        let values: Vec<_> = alias
                            .reasoning_wire_ids
                            .keys()
                            .filter(|effort| options_list_effort(Some(target_options), effort))
                            .collect();
                        options.push(serde_json::json!([{"type":"effort","values":values}]));
                    }
                }
            }
        }
    }
    if !found {
        return Err(format!(
            "model key {key:?} is absent from verified upstream and curated models/aliases"
        ));
    }
    Ok(options)
}

fn matches_id(key: &str, provider: &str, model: &str) -> bool {
    key.split_once('/') == Some((provider, model))
}
