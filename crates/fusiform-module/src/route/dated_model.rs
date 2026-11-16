//! Request-owned inputs for named dated reads. History and one-instant reads
//! evaluate the same inputs and then use the ordinary renderer and finishers.

use super::*;
use fusiform_store::serve::{HistoryRow, ModelFacts, UncertainFact, WithheldFact};

pub(super) struct DatedModel {
    pub(super) begins: Option<Timestamp>,
    pub(super) version: i64,
    facts: BTreeMap<FactKey, Vec<HistoryRow>>,
    pub(super) curated: Vec<fusiform_store::CuratedModelEra>,
    overrides: Vec<fusiform_store::FactOverrideEra>,
    lifetimes: Vec<(i64, crate::cache_lifetimes::Rules)>,
    provider_known: bool,
    model_known: bool,
}

impl DatedModel {
    pub(super) fn load(
        store: &CatalogStore,
        source: SourceId,
        provider: &str,
        model: &str,
    ) -> Result<Self, RouteError> {
        let facts = store
            .model_fact_histories(source, provider, model)
            .map_err(|e| store_error(&e))?;
        let curated = store
            .curated_model_eras(provider, model)
            .map_err(|e| store_error(&e))?;
        let overrides = store
            .model_fact_override_eras(provider, model)
            .map_err(|e| store_error(&e))?;
        let lifetimes = store
            .model_cache_lifetime_eras(provider, model)
            .map_err(|e| store_error(&e))?
            .into_iter()
            .map(|era| {
                Ok((
                    era.boundary_at_ms,
                    crate::cache_lifetimes::from_eras([era]).map_err(|e| store_error(&e))?,
                ))
            })
            .collect::<Result<_, RouteError>>()?;
        Ok(Self {
            begins: store
                .record_begins_at(source)
                .map_err(|e| store_error(&e))?,
            version: store.catalog_version().map_err(|e| store_error(&e))?,
            provider_known: provider_is_served(store, source, provider)?,
            model_known: store
                .model_is_known(source, provider, model)
                .map_err(|e| store_error(&e))?,
            facts,
            curated,
            overrides,
            lifetimes,
        })
    }

    pub(super) fn raw(&self, key: &FactKey, at: Timestamp) -> Option<&str> {
        let eras = self.facts.get(key)?;
        let index = eras.partition_point(|era| era.boundary_at <= at);
        index
            .checked_sub(1)
            .map(|index| eras[index].value_json.as_str())
    }

    pub(super) fn curated_at(&self, at: Timestamp) -> Option<&fusiform_store::NewCuratedModel> {
        self.curated
            .iter()
            .rev()
            .find(|era| era.boundary_at_ms <= at.0)?
            .claim
            .as_ref()
    }

    pub(super) fn overrides_at(&self, at: Timestamp) -> crate::fact_overrides::FactOverrides {
        let mut latest = BTreeMap::new();
        for era in self
            .overrides
            .iter()
            .filter(|era| era.boundary_at_ms <= at.0)
        {
            latest.insert(&era.fact_key, era.clone());
        }
        crate::fact_overrides::from_eras(latest.into_values())
    }

    pub(super) fn lifetimes_at(&self, at: Timestamp) -> Option<&crate::cache_lifetimes::Rules> {
        self.lifetimes
            .iter()
            .rev()
            .find(|(boundary, _)| *boundary <= at.0)
            .map(|(_, rules)| rules)
    }

    pub(super) fn read_model(
        &self,
        provider: &str,
        model: &str,
        at: Timestamp,
    ) -> (Option<ModelFacts>, Vec<WithheldFact>) {
        let mut facts = BTreeMap::new();
        let mut covering = BTreeMap::new();
        let mut found = false;
        for (key, eras) in &self.facts {
            let Some(value) = self.raw(key, at) else {
                continue;
            };
            found = true;
            // Corrections can be recorded after the requested instant: their
            // closed affected interval, not their boundary, controls refusal.
            let corrections: Vec<_> = eras
                .iter()
                .filter_map(|era| era.correction.as_ref())
                .filter(|c| c.affected_from <= at && at <= c.affected_until)
                .cloned()
                .collect();
            facts.insert(key.clone(), value.to_string());
            if !corrections.is_empty() {
                covering.insert((provider.into(), model.into(), key.clone()), corrections);
            }
        }
        let mut model = ModelFacts {
            provider_id: provider.into(),
            model_id: model.into(),
            facts,
        };
        let withheld = model.withhold_corrected(&covering);
        (found.then_some(model), withheld)
    }

    pub(super) fn uncertain_at(
        &self,
        provider: &str,
        model: &str,
        at: Timestamp,
    ) -> Vec<UncertainFact> {
        self.facts
            .iter()
            .filter_map(|(key, eras)| {
                let era = eras.iter().find(|era| {
                    era.boundary_at > at && era.prior_observation_at.is_some_and(|prior| prior < at)
                })?;
                Some(UncertainFact {
                    provider_id: provider.into(),
                    model_id: model.into(),
                    fact_key: key.clone(),
                    superseded_after: era.prior_observation_at.unwrap(),
                    superseded_by: era.boundary_at,
                })
            })
            .collect()
    }

    pub(super) fn provider_known(&self) -> bool {
        self.provider_known
    }
    pub(super) fn model_known(&self) -> bool {
        self.model_known
    }
}
