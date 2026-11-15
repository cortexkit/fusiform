//! Read-only candidate instants and key inventory for served history.

use std::collections::BTreeSet;

use fusiform_core::SourceId;
use rusqlite::params;

use crate::{CatalogError, CatalogStore};

impl CatalogStore {
    /// Every instant at which a dated model read can change, across all facts.
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
                 UNION SELECT boundary_at_ms, 0 FROM era
                 WHERE source = ?1 AND provider_id = ?2 AND fact_key = 'provider.name'
                 UNION SELECT boundary_at_ms, 0 FROM fact_override_era
                 WHERE provider_id = ?2 AND model_id = ?3
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
            let rows = stmt.query_map(params![source.as_str(), provider, model], |row| {
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
                 WHERE provider_id = ?2 AND model_id = ?3
                 UNION SELECT facts.key FROM curated_model_era, json_each(facts_json) AS facts
                 WHERE provider_id = ?2 AND model_id = ?3 AND facts_json IS NOT NULL",
            )?;
            let rows =
                stmt.query_map(params![source.as_str(), provider, model], |row| row.get(0))?;
            rows.collect()
        })
    }
}
