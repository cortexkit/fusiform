//! Request-owned inputs for named dated reads. History and one-instant reads
//! evaluate the same inputs and then use the ordinary renderer and finishers.

use super::*;
use fusiform_store::serve::{HistoryRow, ModelFacts, UncertainFact, WithheldFact};

pub(super) struct DatedModel {
    pub(super) begins: Option<Timestamp>,
    pub(super) version: i64,
    facts: BTreeMap<FactKey, FactHistory>,
    pub(super) curated: Vec<fusiform_store::CuratedModelEra>,
    overrides: BTreeMap<(String, String), Vec<fusiform_store::FactOverrideEra>>,
    lifetimes: Vec<(i64, crate::cache_lifetimes::Rules)>,
    pub(super) speed_twins: DatedSpeedTwins,
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
            .map_err(|e| store_error(&e))?
            .into_iter()
            .map(|(key, eras)| (key, FactHistory::new(eras)))
            .collect();
        let curated = store
            .curated_model_eras(provider, model)
            .map_err(|e| store_error(&e))?;
        let mut overrides: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for era in store
            .model_fact_override_eras(provider, model)
            .map_err(|e| store_error(&e))?
        {
            overrides
                .entry((era.model_id.clone(), era.fact_key.clone()))
                .or_default()
                .push(era);
        }
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
            speed_twins: DatedSpeedTwins::load(store, source, |row| {
                row.provider_id == provider && row.base_model_id == model
            })?,
        })
    }

    pub(super) fn raw(&self, key: &FactKey, at: Timestamp) -> Option<&str> {
        self.facts.get(key)?.raw_at(at)
    }

    pub(super) fn curated_at(&self, at: Timestamp) -> Option<&fusiform_store::NewCuratedModel> {
        self.curated_era_at(at)?.claim.as_ref()
    }

    pub(super) fn curated_era_at(&self, at: Timestamp) -> Option<&fusiform_store::CuratedModelEra> {
        let index = self
            .curated
            .partition_point(|era| era.boundary_at_ms <= at.0);
        self.curated.get(index.checked_sub(1)?)
    }

    pub(super) fn overrides_at(&self, at: Timestamp) -> crate::fact_overrides::FactOverrides {
        let latest = self.overrides.values().filter_map(|eras| {
            let index = eras.partition_point(|era| era.boundary_at_ms <= at.0);
            index.checked_sub(1).map(|i| eras[i].clone())
        });
        crate::fact_overrides::from_eras(latest)
    }

    pub(super) fn lifetimes_at(&self, at: Timestamp) -> Option<&crate::cache_lifetimes::Rules> {
        let index = self
            .lifetimes
            .partition_point(|(boundary, _)| *boundary <= at.0);
        index.checked_sub(1).map(|i| &self.lifetimes[i].1)
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
        for (key, history) in &self.facts {
            let Some(value) = self.raw(key, at) else {
                continue;
            };
            found = true;
            // Corrections can be recorded after the requested instant: their
            // closed affected interval, not their boundary, controls refusal.
            let mut covering_rows = Vec::new();
            history
                .corrections
                .visit(at.0, |row| covering_rows.push(row));
            // The disclosures list corrections in recording order, not in the
            // order in which their affected intervals started.
            covering_rows.sort_unstable();
            let corrections: Vec<_> = covering_rows
                .into_iter()
                .filter_map(|row| history.eras[row].correction.as_ref())
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

    pub(super) fn uncertain_at(&self, model: &ModelFacts, at: Timestamp) -> Vec<UncertainFact> {
        model
            .facts
            .keys()
            .filter_map(|key| {
                let history = self.facts.get(key)?;
                let mut first = usize::MAX;
                history
                    .uncertainty
                    .visit(at.0, |row| first = first.min(row));
                let era = history.eras.get(first)?;
                Some(UncertainFact {
                    provider_id: model.provider_id.clone(),
                    model_id: model.model_id.clone(),
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

pub(super) struct DatedSpeedTwins(Vec<DatedSpeedTwin>);

struct DatedSpeedTwin {
    base: String,
    existence: FactHistory,
    rates: BTreeMap<String, FactHistory>,
}

impl DatedSpeedTwins {
    pub(super) fn load(
        store: &CatalogStore,
        source: SourceId,
        matches_base: impl Fn(&curate::speed_twins::SpeedTwin) -> bool,
    ) -> Result<Self, RouteError> {
        // The current pairing identifies the serving variant, regardless of
        // when the catalog first learned to describe it as a mode.
        let rows = crate::curation::PublishedCuration::current_speed_twins(store)
            .map_err(RouteError::unavailable)?;
        let mut twins = Vec::new();
        for row in rows.values().filter(|row| matches_base(row)) {
            let mut facts = store
                .model_fact_histories(source, &row.provider_id, &row.twin_model_id)
                .map_err(|err| store_error(&err))?;
            let Some(existence) = facts.remove(&FactKey::existence()) else {
                continue;
            };
            let rates = facts
                .into_iter()
                .filter_map(|(key, eras)| {
                    curate::speed_twins::mode_rate_key(key.as_str(), &row.mode)
                        .map(|key| (key, FactHistory::new(eras)))
                })
                .collect();
            twins.push(DatedSpeedTwin {
                base: format!("{}/{}", row.provider_id, row.base_model_id),
                existence: FactHistory::new(existence),
                rates,
            });
        }
        Ok(Self(twins))
    }

    pub(super) fn extend_boundaries(&self, boundaries: &mut BTreeSet<i64>) {
        for twin in &self.0 {
            for history in std::iter::once(&twin.existence).chain(twin.rates.values()) {
                for era in &history.eras {
                    boundaries.insert(era.boundary_at.0);
                    if let Some(correction) = &era.correction {
                        boundaries.insert(correction.affected_from.0);
                        boundaries.extend(correction.affected_until.0.checked_add(1));
                    }
                }
            }
        }
    }

    pub(super) fn has_rate_at(&self, base: &str, key: &str, at: Timestamp) -> bool {
        self.0.iter().any(|twin| {
            twin.base == base
                && twin.present_at(at)
                && twin
                    .rates
                    .get(key)
                    .and_then(|rate| dated_rate(rate, at))
                    .is_some()
        })
    }

    pub(super) fn apply(&self, at: Timestamp, response: &mut CatalogGetResponse) {
        for twin in &self.0 {
            let Some(facts) = response.models.get_mut(&twin.base) else {
                continue;
            };
            if !twin.present_at(at) {
                continue;
            }
            for (key, rate) in &twin.rates {
                if response
                    .withheld
                    .iter()
                    .any(|w| w.model == twin.base && w.fact_key == *key)
                {
                    continue;
                }
                if let Some(value) = dated_rate(rate, at) {
                    facts.insert(key.clone(), value);
                }
            }
        }
    }
}

impl DatedSpeedTwin {
    fn present_at(&self, at: Timestamp) -> bool {
        self.existence.readable_at(at) == Some("\"present\"")
    }
}

fn dated_rate(rate: &FactHistory, at: Timestamp) -> Option<serde_json::Value> {
    let mut value = fact_for_the_wire(rate.readable_at(at)?);
    // Ingest writes missing_rate as the tombstone when a published rate
    // disappears. The mode key advertises only rates held by the twin.
    if value["state"] == "unpriced" && value["reason"] == "missing_rate" {
        return None;
    }
    name_the_provenance_of_a_pre_0_11_row(&mut value);
    Some(value)
}

struct FactHistory {
    eras: Vec<HistoryRow>,
    corrections: IntervalIndex,
    uncertainty: IntervalIndex,
}

impl FactHistory {
    fn raw_at(&self, at: Timestamp) -> Option<&str> {
        let index = self.eras.partition_point(|era| era.boundary_at <= at);
        index
            .checked_sub(1)
            .map(|index| self.eras[index].value_json.as_str())
    }

    fn readable_at(&self, at: Timestamp) -> Option<&str> {
        let raw = self.raw_at(at)?;
        let mut withheld = false;
        self.corrections.visit(at.0, |_| withheld = true);
        (!withheld).then_some(raw)
    }

    fn new(eras: Vec<HistoryRow>) -> Self {
        let mut corrections = Vec::new();
        let mut uncertainty = Vec::new();
        for (row, era) in eras.iter().enumerate() {
            if let Some(c) = &era.correction {
                corrections.push(Interval::new(c.affected_from.0, c.affected_until.0, row));
            }
            if let Some(prior) = era.prior_observation_at {
                // Observation uncertainty excludes both endpoints.
                if let (Some(from), Some(until)) =
                    (prior.0.checked_add(1), era.boundary_at.0.checked_sub(1))
                {
                    if from <= until {
                        uncertainty.push(Interval::new(from, until, row));
                    }
                }
            }
        }
        Self {
            eras,
            corrections: IntervalIndex::new(corrections),
            uncertainty: IntervalIndex::new(uncertainty),
        }
    }
}

struct Interval {
    from: i64,
    until: i64,
    row: usize,
    subtree_until: i64,
}

impl Interval {
    fn new(from: i64, until: i64, row: usize) -> Self {
        Self {
            from,
            until,
            row,
            subtree_until: until,
        }
    }
}

/// A balanced interval tree stored in start-time order. The midpoint of each
/// slice is its root and records the largest end in that subtree, so a read can
/// skip intervals that ended before the requested instant.
struct IntervalIndex(Vec<Interval>);

impl IntervalIndex {
    fn new(mut intervals: Vec<Interval>) -> Self {
        intervals.sort_unstable_by_key(|interval| interval.from);
        fn build(entries: &mut [Interval]) -> i64 {
            let (left, right) = entries.split_at_mut(entries.len() / 2);
            let Some((root, right)) = right.split_first_mut() else {
                return i64::MIN;
            };
            root.subtree_until = root.until.max(build(left)).max(build(right));
            root.subtree_until
        }
        build(&mut intervals);
        Self(intervals)
    }

    fn visit(&self, at: i64, mut visit_row: impl FnMut(usize)) {
        fn walk(entries: &[Interval], at: i64, visit_row: &mut impl FnMut(usize)) {
            let (left, right) = entries.split_at(entries.len() / 2);
            let Some((root, right)) = right.split_first() else {
                return;
            };
            if root.subtree_until < at {
                return;
            }
            walk(left, at, visit_row);
            if root.from <= at {
                if at <= root.until {
                    visit_row(root.row);
                }
                walk(right, at, visit_row);
            }
        }
        walk(&self.0, at, &mut visit_row);
    }
}
