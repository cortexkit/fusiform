//! Read-only candidate instants and key inventory for served history.

use std::collections::{BTreeMap, BTreeSet};

use fusiform_core::SourceId;
use rusqlite::params;

use crate::serve::HistoryRow;
use crate::{CatalogError, CatalogStore, FactKey};

impl CatalogStore {
    /// One model's upstream eras, including later corrections and observation
    /// windows needed to qualify earlier reads. Never loads other models.
    pub fn model_fact_histories(
        &self,
        source: SourceId,
        provider: &str,
        model: &str,
    ) -> Result<BTreeMap<FactKey, Vec<HistoryRow>>, CatalogError> {
        self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT value_json, boundary_at_ms, boundary_kind, prior_observation_at_ms,
                        corrected_fields_json, affected_from_ms, affected_until_ms,
                        correction_reason, fact_key
                 FROM era WHERE source = ?1 AND provider_id = ?2 AND model_id = ?3
                 ORDER BY fact_key, boundary_at_ms",
            )?;
            let rows = stmt.query_map(params![source.as_str(), provider, model], |row| {
                Ok((
                    FactKey::from_stored(row.get(8)?),
                    crate::serve::history_row(row)?,
                ))
            })?;
            let mut facts: BTreeMap<_, Vec<_>> = BTreeMap::new();
            for row in rows {
                let (key, era) = row?;
                facts.entry(key).or_default().push(era);
            }
            Ok(facts)
        })
    }

    /// Boundaries belonging to the requested model, not its provider's other
    /// models. Provider-label joins have their own boundary inventory.
    /// Correction extents are closed, so the first readable instant is end + 1.
    pub fn model_history_boundaries(
        &self,
        source: SourceId,
        provider: &str,
        model: &str,
    ) -> Result<BTreeSet<i64>, CatalogError> {
        self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT boundary_at_ms, 0 FROM era
                 WHERE source = ?1 AND provider_id = ?2 AND model_id = ?3
                 UNION SELECT boundary_at_ms, 0 FROM fact_override_era
                 WHERE provider_id = ?2 AND model_id IN (?3, '*')
                 UNION SELECT boundary_at_ms, 0 FROM curated_model_era
                 WHERE provider_id = ?2 AND model_id = ?3
                 UNION SELECT boundary_at_ms, 0 FROM cache_lifetime_era
                 WHERE provider_id = ?2 AND model_id = ?3
                 UNION SELECT affected_from_ms, 0 FROM era
                 WHERE source = ?1 AND provider_id = ?2 AND model_id = ?3
                   AND affected_from_ms IS NOT NULL
                 UNION SELECT affected_until_ms, 1 FROM era
                 WHERE source = ?1 AND provider_id = ?2 AND model_id = ?3
                   AND affected_until_ms IS NOT NULL",
            )?;
            collect_boundaries(&mut stmt, params![source.as_str(), provider, model])
        })
    }

    /// A curated card can inherit the label of the first present model under
    /// its provider. Either the donor's presence or its readable label can
    /// change that answer, including retroactive corrections to either fact.
    pub fn provider_label_boundaries(
        &self,
        source: SourceId,
        provider: &str,
    ) -> Result<BTreeSet<i64>, CatalogError> {
        self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT boundary_at_ms, 0 FROM era
                 WHERE source = ?1 AND provider_id = ?2 AND fact_key IN ('existence', 'provider.name')
                 UNION SELECT affected_from_ms, 0 FROM era
                 WHERE source = ?1 AND provider_id = ?2 AND fact_key IN ('existence', 'provider.name')
                   AND boundary_kind = 'corrected'
                 UNION SELECT affected_until_ms, 1 FROM era
                 WHERE source = ?1 AND provider_id = ?2 AND fact_key IN ('existence', 'provider.name')
                   AND boundary_kind = 'corrected'",
            )?;
            collect_boundaries(&mut stmt, params![source.as_str(), provider])
        })
    }

    /// Stored upstream/override keys and every key in every curated claim.
    /// Derived cache-lifetime keys are collected from dated resolver results.
    pub fn model_history_keys(
        &self,
        source: SourceId,
        provider: &str,
        model: &str,
    ) -> Result<BTreeSet<String>, CatalogError> {
        self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT fact_key FROM era
                 WHERE source = ?1 AND provider_id = ?2 AND model_id = ?3
                 UNION SELECT fact_key FROM fact_override_era
                 WHERE provider_id = ?2 AND model_id IN (?3, '*')
                 UNION SELECT facts.key FROM curated_model_era, json_each(facts_json) AS facts
                 WHERE provider_id = ?2 AND model_id = ?3 AND facts_json IS NOT NULL",
            )?;
            let rows =
                stmt.query_map(params![source.as_str(), provider, model], |row| row.get(0))?;
            rows.collect()
        })
    }
}

fn collect_boundaries(
    stmt: &mut rusqlite::Statement<'_>,
    params: impl rusqlite::Params,
) -> rusqlite::Result<BTreeSet<i64>> {
    let rows = stmt.query_map(params, |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?))
    })?;
    let mut boundaries = BTreeSet::new();
    for row in rows {
        let (at, closed_end) = row?;
        if let Some(at) = at.checked_add(i64::from(closed_end)) {
            boundaries.insert(at);
        }
    }
    Ok(boundaries)
}
