//! Reading the catalog: what it says now, and what it said at an instant.
//!
//! This is the read side of the store, shaped by what the design note settled
//! in §10 rather than by what is convenient to query. Three properties carry
//! over from there and explain most of what follows.
//!
//! **A retired model is a row, not an absence.** Existence is a fact with its
//! own eras, so a model that stopped being published is `Absent` rather than
//! missing. A caller asking for the catalog gets present models by default,
//! because that is what "the catalog" means; a caller auditing history asks for
//! everything and gets tombstones with their boundaries.
//!
//! **A point-in-time read is not a filtered current read.** It resolves each
//! fact independently against the instant, because facts move at different
//! times: a model repriced yesterday and re-limited last week has two different
//! answers at any instant between.
//!
//! **Nothing here decides what a consumer does.** A rate that is
//! `Unpriced(MissingRate)` is returned as that, never as zero or as an absent
//! key. The read layer's job is to say what the catalog holds, including when
//! what it holds is "no answer".

use std::collections::BTreeMap;

use fusiform_core::{SourceId, Timestamp};
use rusqlite::params;

use crate::{CatalogError, CatalogStore, FactKey};

/// One model's facts, as the catalog currently holds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFacts {
    pub provider_id: String,
    pub model_id: String,
    /// Fact key to raw JSON value, exactly as stored.
    ///
    /// The values stay as JSON text rather than being parsed into a typed
    /// struct here. The typed served shape belongs in the published crate
    /// (`cortexkit-model-catalog`'s next major version, per §10), and building
    /// a second typed shape in the store would create two representations that
    /// must agree — the failure this repository has hit repeatedly.
    pub facts: BTreeMap<FactKey, String>,
}

impl ModelFacts {
    /// Whether the catalog currently describes this model as existing.
    ///
    /// A model with no existence fact at all is treated as absent rather than
    /// present. The alternative — defaulting to present — would make a
    /// partially-written model look real.
    pub fn is_present(&self) -> bool {
        self.facts
            .get(&FactKey::existence())
            .map(|v| v == "\"present\"")
            .unwrap_or(false)
    }
}

/// Which models a read covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Only models the catalog currently describes as existing.
    ///
    /// The default for a consumer read: a retired model is not part of "what
    /// models exist", and returning it would have every consumer write the same
    /// filter.
    PresentOnly,
    /// Every model the store has ever recorded, including tombstoned ones.
    ///
    /// For audit and operator reads. A tombstone carries its own boundary, so
    /// "when did this model stop being published" is answerable.
    IncludingRetired,
}

/// A filter over which facts to return.
///
/// Measured on 2026-08-11 against a live 6,253-model document: the full fact
/// set renders as 3.0 MB of JSON. The capability-consuming subset
/// (`limit.context`, `limit.output`, `capability.reasoning`) is 0.64 MB, and
/// rates alone are 1.55 MB. A consumer that wants one plane should not carry
/// the other across the wire on every push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactFilter {
    /// Every fact.
    All,
    /// Only facts whose key starts with one of these prefixes.
    ///
    /// Prefix rather than an enum of planes, because the plane boundaries are a
    /// consumer's concern and fusiform holds no policy about them. A tiered
    /// rate key is `rate.input.above_context.200000`, so the prefix `rate.`
    /// selects a model's whole pricing surface including its tiers.
    Prefixes(Vec<String>),
}

impl FactFilter {
    fn admits(&self, key: &FactKey) -> bool {
        match self {
            FactFilter::All => true,
            FactFilter::Prefixes(prefixes) => prefixes
                .iter()
                .any(|p| key.as_str().starts_with(p.as_str())),
        }
    }
}

/// What a read asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogQuery {
    pub source: SourceId,
    /// The instant to resolve against. `None` means now.
    ///
    /// Explicitly an `Option` rather than defaulting to the current wall clock,
    /// because "the current catalog" and "the catalog at the instant this
    /// happens to be called" are different requests. The first is what a
    /// consumer wants; the second silently makes a read non-reproducible.
    pub at: Option<Timestamp>,
    pub presence: Presence,
    pub facts: FactFilter,
}

impl CatalogQuery {
    /// The current catalog, present models only, every fact.
    pub fn current(source: SourceId) -> Self {
        Self {
            source,
            at: None,
            presence: Presence::PresentOnly,
            facts: FactFilter::All,
        }
    }

    /// The catalog as it stood at an instant.
    pub fn at(source: SourceId, instant: Timestamp) -> Self {
        Self {
            source,
            at: Some(instant),
            presence: Presence::PresentOnly,
            facts: FactFilter::All,
        }
    }

    pub fn including_retired(mut self) -> Self {
        self.presence = Presence::IncludingRetired;
        self
    }

    pub fn with_prefixes(mut self, prefixes: &[&str]) -> Self {
        self.facts = FactFilter::Prefixes(prefixes.iter().map(|p| p.to_string()).collect());
        self
    }
}

/// The result of a catalog read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSnapshot {
    pub source: SourceId,
    /// The instant this snapshot resolves to, always concrete.
    ///
    /// A read for "now" records the instant it resolved at, so the answer is
    /// reproducible: a caller can re-ask for exactly this snapshot later, and a
    /// bug report can name the instant rather than a wall-clock guess.
    pub resolved_at: Timestamp,
    /// The catalog version at the time of the read.
    pub catalog_version: i64,
    pub models: Vec<ModelFacts>,
}

impl CatalogSnapshot {
    pub fn model_count(&self) -> usize {
        self.models.len()
    }

    pub fn fact_count(&self) -> usize {
        self.models.iter().map(|m| m.facts.len()).sum()
    }
}

/// The point-in-time read, as one statement.
///
/// Public so a test can assert on the query the store actually runs. A test
/// holding its own copy would verify that the copy is index-backed while the
/// shipped query drifted away from it.
///
/// The correlated MAX with a `<= ?2` bound is the same shape as the
/// steady-state read, for the same measured reason: it resolves each per-fact
/// maximum from the covering index rather than scanning. The instant bound sits
/// inside the subquery as well as outside it — omitting it there would find the
/// latest boundary overall and then discard it, returning nothing for any fact
/// that has moved since the instant asked about.
pub const POINT_IN_TIME_SQL: &str = "SELECT e.provider_id, e.model_id, e.fact_key, e.value_json \
     FROM era e \
     WHERE e.source = ?1 \
       AND e.boundary_at_ms <= ?2 \
       AND e.boundary_at_ms = ( \
           SELECT MAX(e2.boundary_at_ms) FROM era e2 \
           WHERE e2.source = e.source \
             AND e2.provider_id = e.provider_id \
             AND e2.model_id = e.model_id \
             AND e2.fact_key = e.fact_key \
             AND e2.boundary_at_ms <= ?2 \
       )";

impl CatalogStore {
    /// Read the catalog.
    pub fn read_catalog(&self, query: &CatalogQuery) -> Result<CatalogSnapshot, CatalogError> {
        // Resolve "now" to a concrete instant ONCE, before the read. Two rows
        // resolved against two different instants would be a snapshot that
        // never existed.
        let resolved_at = match query.at {
            Some(at) => at,
            None => Timestamp(now_ms()),
        };

        let rows = self.point_in_time_rows(query.source, resolved_at)?;

        // Corrections covering this instant, fetched once for the whole read.
        //
        // A fact inside a corrected interval must not appear with its recorded
        // value: that value is known bad, and `value_at` refuses it. A bulk
        // read that returned it anyway would make the honest surface the one
        // nobody calls — consumers read catalogs, not single facts.
        //
        // The fact is OMITTED rather than replaced with a marker. A consumer
        // that needs to know why asks `value_at`, which names the correction;
        // putting a sentinel in the value position would mean every consumer
        // parsing a rate has to recognise it, and the ones that do not would
        // read it as data.
        let corrections = self.all_corrections_covering(query.source, resolved_at)?;

        let mut by_model: BTreeMap<(String, String), BTreeMap<FactKey, String>> = BTreeMap::new();
        for (provider_id, model_id, fact_key, value_json) in rows {
            let fact_key = FactKey::from_stored(fact_key);
            if !corrections.is_empty()
                && corrections.contains_key(&(
                    provider_id.clone(),
                    model_id.clone(),
                    fact_key.clone(),
                ))
            {
                continue;
            }
            by_model
                .entry((provider_id, model_id))
                .or_default()
                .insert(fact_key, value_json);
        }

        let mut models = Vec::with_capacity(by_model.len());
        for ((provider_id, model_id), facts) in by_model {
            let model = ModelFacts {
                provider_id,
                model_id,
                facts,
            };

            // Presence is decided BEFORE the fact filter, on the full fact set.
            // Filtering first would drop the existence fact for a rates-only
            // read and make every model look retired.
            if query.presence == Presence::PresentOnly && !model.is_present() {
                continue;
            }

            let facts: BTreeMap<FactKey, String> = model
                .facts
                .into_iter()
                .filter(|(k, _)| query.facts.admits(k))
                .collect();

            // A model whose every fact was filtered out contributes nothing but
            // its name, and a consumer reading a plane it does not participate
            // in would have to special-case an empty entry.
            if facts.is_empty() {
                continue;
            }

            models.push(ModelFacts {
                provider_id: model.provider_id,
                model_id: model.model_id,
                facts,
            });
        }

        Ok(CatalogSnapshot {
            source: query.source,
            resolved_at,
            catalog_version: self.catalog_version()?,
            models,
        })
    }

    /// Every fact for one model, resolved at an instant.
    ///
    /// Answered by the same query as the full read with an extra predicate,
    /// rather than by a second SQL statement: two statements that must agree
    /// about what "current" means is two places for that meaning to drift.
    pub fn read_model(
        &self,
        source: SourceId,
        provider_id: &str,
        model_id: &str,
        at: Option<Timestamp>,
    ) -> Result<Option<ModelFacts>, CatalogError> {
        let resolved_at = at.unwrap_or_else(|| Timestamp(now_ms()));
        let rows = self.point_in_time_rows_for_model(source, provider_id, model_id, resolved_at)?;
        if rows.is_empty() {
            return Ok(None);
        }
        let mut facts = BTreeMap::new();
        for (fact_key, value_json) in rows {
            facts.insert(FactKey::from_stored(fact_key), value_json);
        }
        Ok(Some(ModelFacts {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
            facts,
        }))
    }

    fn point_in_time_rows(
        &self,
        source: SourceId,
        at: Timestamp,
    ) -> Result<Vec<(String, String, String, String)>, CatalogError> {
        let rows = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(POINT_IN_TIME_SQL)?;
            let mapped = stmt.query_map(params![source.as_str(), at.0], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows)
    }

    fn point_in_time_rows_for_model(
        &self,
        source: SourceId,
        provider_id: &str,
        model_id: &str,
        at: Timestamp,
    ) -> Result<Vec<(String, String)>, CatalogError> {
        let sql = format!("{POINT_IN_TIME_SQL} AND e.provider_id = ?3 AND e.model_id = ?4");
        let rows = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mapped = stmt
                .query_map(params![source.as_str(), at.0, provider_id, model_id], |r| {
                    Ok((r.get(2)?, r.get(3)?))
                })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows)
    }
}

/// One recorded poll, as an operator reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationRow {
    pub id: i64,
    pub observed_at: Timestamp,
    /// The stored outcome word: changed / unchanged / not_modified / failed.
    pub outcome: String,
    /// Present only on a failure, naming the coarse class.
    pub failure_class: Option<String>,
    pub detail: Option<String>,
    pub normalized_hash: Option<String>,
    pub raw_hash: Option<String>,
    pub duration_ms: Option<i64>,
}

/// One era in a fact's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    pub value_json: String,
    pub boundary_at: Timestamp,
    pub boundary_kind: String,
    /// The other edge of the observation window, when the boundary has one.
    pub prior_observation_at: Option<Timestamp>,
}

impl HistoryRow {
    /// The interval within which this value became true, when one exists.
    pub fn window(&self) -> Option<(Timestamp, Timestamp)> {
        self.prior_observation_at.map(|p| (p, self.boundary_at))
    }
}

impl CatalogStore {
    /// The most recent polls, newest first.
    ///
    /// Includes failures and 304s. An operator asking "what has fusiform been
    /// doing" needs the polls that changed nothing most of all: a source that
    /// has been returning 304 for a week and a source that has been failing for
    /// a week look identical from the catalog alone.
    pub fn recent_observations(
        &self,
        source: SourceId,
        limit: u32,
    ) -> Result<Vec<ObservationRow>, CatalogError> {
        let rows = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, observed_at_ms, outcome, failure_class, detail, \
                        normalized_hash, raw_hash, duration_ms \
                 FROM observation WHERE source = ?1 \
                 ORDER BY observed_at_ms DESC, id DESC LIMIT ?2",
            )?;
            let mapped = stmt.query_map(params![source.as_str(), limit], |r| {
                Ok(ObservationRow {
                    id: r.get(0)?,
                    observed_at: Timestamp(r.get(1)?),
                    outcome: r.get(2)?,
                    failure_class: r.get(3)?,
                    detail: r.get(4)?,
                    normalized_hash: r.get(5)?,
                    raw_hash: r.get(6)?,
                    duration_ms: r.get(7)?,
                })
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows)
    }

    /// Every era for one fact, oldest first.
    ///
    /// Oldest first because a history is read forwards: "it was X, then Y from
    /// this window, then Z". The catalog reads are newest-first because they
    /// answer a different question.
    pub fn fact_history(
        &self,
        source: SourceId,
        provider_id: &str,
        model_id: &str,
        fact: &FactKey,
    ) -> Result<Vec<HistoryRow>, CatalogError> {
        let rows = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT value_json, boundary_at_ms, boundary_kind, prior_observation_at_ms \
                 FROM era \
                 WHERE source = ?1 AND provider_id = ?2 AND model_id = ?3 AND fact_key = ?4 \
                 ORDER BY boundary_at_ms ASC",
            )?;
            let mapped = stmt.query_map(
                params![source.as_str(), provider_id, model_id, fact.as_str()],
                |r| {
                    Ok(HistoryRow {
                        value_json: r.get(0)?,
                        boundary_at: Timestamp(r.get(1)?),
                        boundary_kind: r.get(2)?,
                        prior_observation_at: r.get::<_, Option<i64>>(3)?.map(Timestamp),
                    })
                },
            )?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows)
    }

    /// How many eras the store holds, for one source.
    pub fn era_count(&self, source: SourceId) -> Result<i64, CatalogError> {
        let n = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM era WHERE source = ?1",
                params![source.as_str()],
                |r| r.get::<_, i64>(0),
            )
        })?;
        Ok(n)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
