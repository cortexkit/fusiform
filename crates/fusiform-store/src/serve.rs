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
use rusqlite::{params, OptionalExtension};

use crate::{prefix, CatalogError, CatalogStore, FactKey};

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

/// A fact this read refused to answer, and why.
///
/// Omitting a corrected fact silently would make it indistinguishable from one
/// the upstream never published — the same "wrong answer that reads as a
/// legitimate one" the handler refuses an empty catalog for. A consumer that
/// cannot see a rate must be able to tell "no such rate" from "the rate we
/// recorded here is known bad".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithheldFact {
    pub provider_id: String,
    pub model_id: String,
    pub fact_key: FactKey,
    /// Every correction covering the read instant, oldest first.
    pub corrections: Vec<crate::CorrectionRecord>,
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
    /// Facts withheld because their record is known bad at this instant.
    ///
    /// Empty in the ordinary case. Non-empty means the answer is incomplete in
    /// a specific, named way rather than in an invisible one.
    pub withheld: Vec<WithheldFact>,
    /// Facts whose value is real but may already have been superseded at this
    /// instant, each with the interval the change happened in.
    ///
    /// The values ARE included in `models` — unlike `withheld`, this is a
    /// qualification rather than a refusal. Fusiform knows something about the
    /// instant, just not everything, and dropping the value would discard
    /// information the store holds.
    ///
    /// **Always empty for a read of the current catalog**, because an era
    /// covering `now` has no successor. It is the historical read — "what was
    /// the rate on the 9th" — that can be uncertain, which is the question a
    /// wrong answer costs money on.
    pub uncertain: Vec<UncertainFact>,
}

/// A fact whose recorded value at the read instant may already have ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncertainFact {
    pub provider_id: String,
    pub model_id: String,
    pub fact_key: FactKey,
    /// The last instant fusiform confirmed this value.
    pub superseded_after: Timestamp,
    /// The instant it observed the change.
    pub superseded_by: Timestamp,
}

impl CatalogSnapshot {
    pub fn model_count(&self) -> usize {
        self.models.len()
    }

    pub fn fact_count(&self) -> usize {
        self.models.iter().map(|m| m.facts.len()).sum()
    }

    /// How many models carry at least one rate.
    ///
    /// The model total cannot express this and the difference is not small:
    /// measured on the live store, 420 of 6,293 present models have no cost
    /// object upstream and therefore no rate rows at all.
    ///
    /// Counted from the facts actually present rather than from a stored flag,
    /// so it cannot disagree with what a read of the same snapshot returns.
    pub fn priced_model_count(&self) -> usize {
        self.models
            .iter()
            .filter(|m| m.facts.keys().any(|k| k.as_str().starts_with(prefix::RATE)))
            .count()
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

/// Facts whose value at an instant sits inside a later era's observation
/// window, for one source.
///
/// One query for the whole read rather than one per fact, the same shape as
/// the corrections lookup: a bulk read resolves ~68,000 facts and asking about
/// each separately would be 68,000 queries to discover that nearly all of them
/// are certain.
///
/// Empty for a read of the current catalog, always: an era covering `now`
/// cannot have a successor.
pub const UNCERTAIN_FACTS_SQL: &str =
    "SELECT provider_id, model_id, fact_key, prior_observation_at_ms, boundary_at_ms \
     FROM era \
     WHERE source = ?1 \
       AND prior_observation_at_ms IS NOT NULL \
       AND prior_observation_at_ms < ?2 AND boundary_at_ms > ?2";

/// Uncertainty brackets keyed by fact identity: provider, model, fact key.
///
/// The value is the interval the change happened in — `(last confirmed, first
/// observed changed)`.
pub type UncertaintyByFact = BTreeMap<(String, String, FactKey), (Timestamp, Timestamp)>;

impl CatalogStore {
    /// Facts whose recorded value at an instant may already have been
    /// superseded, keyed by identity.
    ///
    /// The era answering a read for `at` ended somewhere inside the returned
    /// bracket, so the value is real and its continued force at `at` is not
    /// established. See [`PointInTime::KnownStale`].
    pub fn uncertain_facts_at(
        &self,
        source: SourceId,
        at: Timestamp,
    ) -> Result<UncertaintyByFact, CatalogError> {
        let rows = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(UNCERTAIN_FACTS_SQL)?;
            let mapped = stmt.query_map(params![source.as_str(), at.0], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })?;

        Ok(rows
            .into_iter()
            .map(|(p, m, k, prior, boundary)| {
                (
                    (p, m, FactKey::from_stored(k)),
                    (Timestamp(prior), Timestamp(boundary)),
                )
            })
            .collect())
    }

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

        // Facts whose value at this instant sits inside a later era's window.
        // One query for the whole read, and empty by construction for a read of
        // the current catalog.
        let uncertainty = self.uncertain_facts_at(query.source, resolved_at)?;

        let mut by_model: BTreeMap<(String, String), BTreeMap<FactKey, String>> = BTreeMap::new();
        let mut withheld: Vec<WithheldFact> = Vec::new();
        let mut uncertain: Vec<UncertainFact> = Vec::new();
        for (provider_id, model_id, fact_key, value_json) in rows {
            let fact_key = FactKey::from_stored(fact_key);
            if !corrections.is_empty() {
                let key = (provider_id.clone(), model_id.clone(), fact_key.clone());
                if let Some(records) = corrections.get(&key) {
                    // Named rather than dropped. A silently missing fact is
                    // indistinguishable from one the upstream never published.
                    withheld.push(WithheldFact {
                        provider_id,
                        model_id,
                        fact_key,
                        corrections: records.clone(),
                    });
                    continue;
                }
            }
            // Uncertain rather than withheld: the value is real and is
            // returned. What is not established is that it was still in force
            // at the read instant.
            if let Some((prior, boundary)) =
                uncertainty.get(&(provider_id.clone(), model_id.clone(), fact_key.clone()))
            {
                uncertain.push(UncertainFact {
                    provider_id: provider_id.clone(),
                    model_id: model_id.clone(),
                    fact_key: fact_key.clone(),
                    superseded_after: *prior,
                    superseded_by: *boundary,
                });
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

        // Withheld facts are reported even when the plane filter would have
        // excluded them.
        //
        // This said "a consumer reading only rates still needs to know a rate
        // was withheld", which cannot demonstrate the rule: a `rate.` filter
        // ADMITS `rate.input`, so that case is identical either way. The case
        // that separates them is a withheld fact on a plane the reader did NOT
        // ask for — read `limit.`, and a withheld `rate.input` either survives
        // or vanishes.
        //
        // Unfiltered is right because `withheld` is a statement about THE
        // MODEL'S RECORD rather than about a plane: fusiform is actively
        // suppressing something here because the stored value is known bad. A
        // consumer reading limits who learns a rate on the same model is under
        // correction has learned how far to trust the limits too.
        //
        // Filtering would make that disclosure depend on which question was
        // asked, and a disclosure you receive only when you happen to ask the
        // matching question is one nobody can rely on.
        Ok(CatalogSnapshot {
            source: query.source,
            resolved_at,
            catalog_version: self.catalog_version()?,
            models,
            withheld,
            uncertain,
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
    ) -> Result<(Option<ModelFacts>, Vec<WithheldFact>), CatalogError> {
        let resolved_at = at.unwrap_or_else(|| Timestamp(now_ms()));
        let rows = self.point_in_time_rows_for_model(source, provider_id, model_id, resolved_at)?;
        if rows.is_empty() {
            return Ok((None, Vec::new()));
        }

        // Corrections apply here for the same reason they apply to a bulk read,
        // and this path had to be fixed separately because it resolves its own
        // rows. Three surfaces answered "what is this fact now" and each
        // decided about corrections independently; two of them decided wrong.
        let corrections = self.all_corrections_covering(source, resolved_at)?;

        let mut facts = BTreeMap::new();
        let mut withheld = Vec::new();
        for (fact_key, value_json) in rows {
            let fact_key = FactKey::from_stored(fact_key);
            if !corrections.is_empty() {
                let key = (
                    provider_id.to_string(),
                    model_id.to_string(),
                    fact_key.clone(),
                );
                if let Some(records) = corrections.get(&key) {
                    withheld.push(WithheldFact {
                        provider_id: provider_id.to_string(),
                        model_id: model_id.to_string(),
                        fact_key,
                        corrections: records.clone(),
                    });
                    continue;
                }
            }
            facts.insert(fact_key, value_json);
        }

        // A model whose every fact was withheld is not absent: it is a model
        // whose record is known bad. Returning None would report it as never
        // published, which is the misreading this whole path exists to prevent.
        Ok((
            Some(ModelFacts {
                provider_id: provider_id.to_string(),
                model_id: model_id.to_string(),
                facts,
            }),
            withheld,
        ))
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
    /// Eras this poll opened. Zero for a 304, an unchanged document, or a
    /// failure.
    pub eras: i64,
    /// Models this poll saw for the first time, or saw return.
    pub models_arrived: i64,
    /// Models this poll found no longer published.
    pub models_withdrawn: i64,
    /// Facts that moved on a model already known — the count an operator
    /// usually means by "what changed".
    ///
    /// Separate from `eras` because an arriving model writes one era per fact
    /// it has, and measured on real polls that dominates: 96 eras for 19
    /// changes, 61 for 15.
    pub facts_changed: i64,
}

/// One era in a fact's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    pub value_json: String,
    pub boundary_at: Timestamp,
    pub boundary_kind: String,
    /// The other edge of the observation window, when the boundary has one.
    pub prior_observation_at: Option<Timestamp>,
    /// The extent and reason, present only on a `corrected` boundary.
    ///
    /// Carried here rather than left in the row because an operator reading a
    /// history needs to know WHICH interval a correction covers. Reporting the
    /// kind alone records a cause without surfacing it.
    pub correction: Option<crate::CorrectionRecord>,
}

impl HistoryRow {
    /// The interval within which this value became true, when one exists.
    pub fn window(&self) -> Option<(Timestamp, Timestamp)> {
        self.prior_observation_at.map(|p| (p, self.boundary_at))
    }
}

impl CatalogStore {
    /// The most recent polls, newest first, each with what it actually changed.
    ///
    /// Includes failures and 304s. An operator asking "what has fusiform been
    /// doing" needs the polls that changed nothing most of all: a source that
    /// has been returning 304 for a week and a source that has been failing for
    /// a week look identical from the catalog alone.
    ///
    /// # Why the composition rather than an era count
    ///
    /// Measured over 11 hours of live polling: 72% of era churn was models
    /// ARRIVING AND LEAVING rather than facts changing, because an arriving
    /// model writes one era per fact it has. One real poll wrote 96 eras of
    /// which 19 were genuine changes; another wrote 61 for 15.
    ///
    /// So an era count alone misleads in a consistent direction, and it
    /// misleads most on the busiest polls — exactly the ones an operator looks
    /// at. A model arriving is the upstream publishing something new; a fact
    /// changing on a known model is the upstream REVISING something, and only
    /// the second is what a consumer's cache or a ledger acts on.
    ///
    /// Derived rather than stored: eras carry `observation_id`, so the
    /// breakdown is a property of history rather than a number a writer had to
    /// remember to record. That also makes it correct for polls written before
    /// this existed.
    pub fn recent_observations(
        &self,
        source: SourceId,
        limit: u32,
    ) -> Result<Vec<ObservationRow>, CatalogError> {
        let rows = self.raw_conn(|conn| {
            // The composition comes from the eras this observation opened. A
            // model's arrival and withdrawal are its `existence` eras; every
            // other era belonging to a model that did NOT change existence in
            // the same poll is a fact revision on a model already known.
            let mut stmt = conn.prepare(
                "SELECT o.id, o.observed_at_ms, o.outcome, o.failure_class, \
                        o.detail, o.normalized_hash, o.raw_hash, o.duration_ms, \
                        (SELECT COUNT(*) FROM era e WHERE e.observation_id = o.id) \
                          AS eras, \
                        (SELECT COUNT(*) FROM era e WHERE e.observation_id = o.id \
                           AND e.fact_key = 'existence' \
                           AND e.value_json = '\"present\"') AS arrived, \
                        (SELECT COUNT(*) FROM era e WHERE e.observation_id = o.id \
                           AND e.fact_key = 'existence' \
                           AND e.value_json = '\"absent\"') AS withdrawn, \
                        (SELECT COUNT(*) FROM era e WHERE e.observation_id = o.id \
                           AND NOT EXISTS ( \
                             SELECT 1 FROM era x WHERE x.observation_id = o.id \
                               AND x.fact_key = 'existence' \
                               AND x.provider_id = e.provider_id \
                               AND x.model_id = e.model_id)) AS facts_changed \
                 FROM observation o WHERE o.source = ?1 \
                 ORDER BY o.observed_at_ms DESC, o.id DESC LIMIT ?2",
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
                    eras: r.get(8)?,
                    models_arrived: r.get(9)?,
                    models_withdrawn: r.get(10)?,
                    facts_changed: r.get(11)?,
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
    /// Has fusiform ever recorded anything for this provider?
    ///
    /// Includes retired models and every historical era, because the question
    /// is "do I know this name" rather than "is it current". A provider whose
    /// every model was withdrawn is still a name fusiform knows, and telling an
    /// operator otherwise would send them hunting a typo they did not make.
    pub fn provider_is_known(
        &self,
        source: SourceId,
        provider_id: &str,
    ) -> Result<bool, CatalogError> {
        let found: Option<i64> = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT 1 FROM era WHERE source = ?1 AND provider_id = ?2 LIMIT 1",
                params![source.as_str(), provider_id],
                |r| r.get(0),
            )
            .optional()
        })?;
        Ok(found.is_some())
    }

    /// Has fusiform ever recorded anything for this exact model?
    ///
    /// Per `provider_is_known`: history counts, so a withdrawn model is known.
    pub fn model_is_known(
        &self,
        source: SourceId,
        provider_id: &str,
        model_id: &str,
    ) -> Result<bool, CatalogError> {
        let found: Option<i64> = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT 1 FROM era WHERE source = ?1 AND provider_id = ?2 AND model_id = ?3 \
                 LIMIT 1",
                params![source.as_str(), provider_id, model_id],
                |r| r.get(0),
            )
            .optional()
        })?;
        Ok(found.is_some())
    }

    pub fn fact_history(
        &self,
        source: SourceId,
        provider_id: &str,
        model_id: &str,
        fact: &FactKey,
    ) -> Result<Vec<HistoryRow>, CatalogError> {
        let rows = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT value_json, boundary_at_ms, boundary_kind, prior_observation_at_ms, \
                        corrected_fields_json, affected_from_ms, affected_until_ms, \
                        correction_reason \
                 FROM era \
                 WHERE source = ?1 AND provider_id = ?2 AND model_id = ?3 AND fact_key = ?4 \
                 ORDER BY boundary_at_ms ASC",
            )?;
            let mapped = stmt.query_map(
                params![source.as_str(), provider_id, model_id, fact.as_str()],
                |r| {
                    let boundary_at = Timestamp(r.get(1)?);
                    // The schema's all-or-nothing CHECK guarantees these four
                    // columns move together, so testing one is enough.
                    let correction = match r.get::<_, Option<String>>(4)? {
                        Some(fields_json) => Some(crate::CorrectionRecord {
                            corrected_at: boundary_at,
                            fields_json,
                            affected_from: Timestamp(r.get(5)?),
                            affected_until: Timestamp(r.get(6)?),
                            reason: r.get(7)?,
                        }),
                        None => None,
                    };
                    Ok(HistoryRow {
                        value_json: r.get(0)?,
                        boundary_at,
                        boundary_kind: r.get(2)?,
                        prior_observation_at: r.get::<_, Option<i64>>(3)?.map(Timestamp),
                        correction,
                    })
                },
            )?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows)
    }

    /// How many models the store currently holds as present.
    ///
    /// Counts existence facts whose latest era says present, which is the same
    /// definition a catalog read uses. Answered by a targeted query rather than
    /// by reading the catalog and counting: the shrink guard runs on every
    /// changed poll, and rendering 67,000 facts to learn one number is a cost
    /// paid for nothing.
    pub fn present_model_count(&self, source: SourceId) -> Result<usize, CatalogError> {
        let n = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM ( \
                   SELECT e.provider_id, e.model_id \
                   FROM era e \
                   WHERE e.source = ?1 AND e.fact_key = 'existence' \
                     AND e.boundary_at_ms = ( \
                       SELECT MAX(e2.boundary_at_ms) FROM era e2 \
                       WHERE e2.source = e.source \
                         AND e2.provider_id = e.provider_id \
                         AND e2.model_id = e.model_id \
                         AND e2.fact_key = 'existence') \
                     AND e.value_json = '\"present\"')",
                params![source.as_str()],
                |r| r.get::<_, i64>(0),
            )
        })?;
        Ok(n as usize)
    }

    /// When the catalog last changed, for one source.
    ///
    /// The newest era's boundary. `None` only when the source has no eras at
    /// all, which is a genuinely empty store rather than a quiet one.
    ///
    /// Exists so a restarted process can adopt this instant instead of
    /// reporting that it has never written. The write clock lives in an atomic
    /// that a restart empties, and a store holding 68,000 eras reporting "no
    /// write ever" is the same defect as the staleness clock resetting: the
    /// atomic describes the process, and an operator reads it as describing the
    /// catalog.
    pub fn newest_era_boundary(&self, source: SourceId) -> Result<Option<Timestamp>, CatalogError> {
        let at = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT MAX(boundary_at_ms) FROM era WHERE source = ?1",
                params![source.as_str()],
                |r| r.get::<_, Option<i64>>(0),
            )
        })?;
        Ok(at.map(Timestamp))
    }

    /// How many polls have ever failed, and when the most recent one was.
    ///
    /// # Why this is durable and `process_consecutive_failures` is not
    ///
    /// The streak is a CURRENT-STATE gauge: it resets on the first success, so
    /// reading health at 09:00 after a failure at 02:00 that recovered by 03:00
    /// reports zero — indistinguishable from nothing ever having gone wrong.
    /// That is correct for "is fusiform failing NOW" and useless for "has
    /// fusiform ever failed", and only the second question survives the
    /// operator not being present while the event holds.
    ///
    /// BROCA found the same shape in their own refusal gauge tonight and fixed
    /// it by adding a total. The general form is my own rare-event argument
    /// turned on an instrument rather than an experiment: AN INSTRUMENT THAT
    /// ONLY READS DURING THE EVENT IS ONLY AS GOOD AS THE ODDS SOMEONE IS
    /// LOOKING AT THE RIGHT MOMENT.
    ///
    /// Read from the observation table rather than counted in an atomic,
    /// because the count must survive a restart — a process-scoped total would
    /// have exactly the gap it exists to close, one level up. The live store
    /// holds one failed poll, network class, from 2026-08-13 08:19:38Z: outside
    /// the default ten-poll status window and invisible in every health metric.
    pub fn failure_history(
        &self,
        source: SourceId,
    ) -> Result<(i64, Option<Timestamp>, Option<fusiform_core::FailureClass>), CatalogError> {
        // The CLASS comes with the count and the instant, from the same row.
        //
        // Health already reports `last_failure_class`, and that field clears
        // with the failure streak — correct for "what is failing now", null for
        // a failure that healed. So the durable half said "1 failure, 23 hours
        // ago" and could not say WHAT KIND, which is the reading that decides
        // whether an operator investigates the upstream or the payload.
        //
        // Measured on the live module after the durable count shipped:
        //   failures_ever: 1, last_failure_age_ms: 84628131,
        //   last_failure_class: null
        //
        // The correlated subquery rather than a GROUP BY, because the class
        // belongs to the NEWEST failure specifically and a grouped query would
        // silently pick an arbitrary one when classes differ.
        let row = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*), MAX(observed_at_ms), \
                        (SELECT failure_class FROM observation \
                          WHERE source = ?1 AND outcome = 'failed' \
                          ORDER BY observed_at_ms DESC LIMIT 1) \
                 FROM observation \
                 WHERE source = ?1 AND outcome = 'failed'",
                params![source.as_str()],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
        })?;
        // Decoded through the store's own mapping rather than by the caller,
        // so a reader never re-parses a word this crate wrote.
        Ok((
            row.0,
            row.1.map(Timestamp),
            row.2.as_deref().and_then(crate::failure_class_from_stored),
        ))
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
