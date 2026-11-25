#![forbid(unsafe_code)]

//! Durable observation and era history for the catalog.
//!
//! The store's job is to make three questions answerable without qualification:
//! what does the catalog say now, what did it say at an instant, and how did it
//! come to say that. The schema (see [`schema`]) is shaped so the answers are
//! arithmetic rather than interpretation.
//!
//! Persistence itself is not reinvented here. The module receives a resolved
//! [`StorageDescriptor`] over the daemon handshake and hands it to
//! `cortexkit-store`, which acquires the single-writer lease, opens the file
//! with durable pragmas, and applies migrations. What this crate owns is the
//! domain: the schema, the writes, and the reads.

mod cache_lifetimes;
pub mod correct;
mod curated;
mod fact_overrides;
mod history;
mod history_base;
pub mod ingest;
mod local_overrides;
pub use local_overrides::{LocalMark, LocalOverrides};
mod quality;
mod quality_efforts;
pub use quality_efforts::{EffortGradeEra, WorkloadEffortEra};
mod readers;
pub mod schema;
pub mod serve;
mod store_consumer;
pub use store_consumer::{
    ImportedTables, ReleaseArrival, ReleaseObservation, StoreRelease, STORE_HISTORY_TABLES,
};

#[cfg(test)]
#[path = "tests/read_connections.rs"]
mod read_connections;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use cortexkit_store::{open_sqlite, SqliteStore, StorageBackend, StorageDescriptor, StoreError};
use fusiform_core::{
    BoundaryKind, Correction, FailureClass, FieldId, ObservationOutcome, SourceId, Timestamp,
};
use rusqlite::{params, OptionalExtension};

pub use cache_lifetimes::{CacheLifetimeEra, NewCacheLifetime};
pub use curated::{
    CuratedModelEra, NewCuratedModel, CURATED_MODELS_FILE, CURATED_MODEL_NOT_SERVED,
};
pub use fact_overrides::{FactOverrideEra, NewFactOverride};
pub use quality::{ModelMarkEra, QualityGrade, QualityGradeEra};

/// A lease-guarded, migrated catalog store.
pub struct CatalogStore {
    // Fields drop in declaration order. Close readers before the writer so its
    // final close can checkpoint the WAL for backups of the closed database.
    readers: readers::ReadPool,
    inner: SqliteStore,
    /// The database file this store opened, exactly as the storage descriptor
    /// named it. Kept so files that must sit beside the database (the backup
    /// enrollment descriptor) are placed by the path actually opened, not by a
    /// path re-derived from the environment that could disagree with it.
    path: std::path::PathBuf,
    instance_id: u64,
    catalog_changes: tokio::sync::Notify,
    rates_version: AtomicU64,
    store_mode: bool,
}

static NEXT_STORE_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// What went wrong, at the domain's level rather than the driver's.
#[derive(Debug)]
pub enum CatalogError {
    Store(StoreError),
    /// A write was rejected because a newer writer owns the database.
    ///
    /// Distinct from a generic backend error because it has a specific correct
    /// response: this process is superseded and must stop writing, not retry.
    Fenced,
    /// The poll has fact changes, but its timestamp is not after the newest
    /// era already recorded for that source.
    PollBoundaryNotIncreasing {
        observed_at: Timestamp,
        latest: Timestamp,
    },
    /// A read or write violated a domain invariant the schema could not express.
    Invariant(String),
    /// A value in the database did not round-trip back to the domain.
    ///
    /// Not a corrupt-database claim: it is what a schema change or a hand-edit
    /// looks like from in here, and both need to be loud rather than defaulted.
    Decode {
        column: &'static str,
        value: String,
    },
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogError::Store(e) => write!(f, "store: {e}"),
            CatalogError::Fenced => {
                write!(f, "write rejected: a newer writer owns this database")
            }
            CatalogError::PollBoundaryNotIncreasing {
                observed_at,
                latest,
            } => write!(
                f,
                "poll clock is at {} ms, not after the newest written boundary at {} ms",
                observed_at.0, latest.0
            ),
            CatalogError::Invariant(msg) => write!(f, "invariant violated: {msg}"),
            CatalogError::Decode { column, value } => {
                write!(f, "column {column} holds unreadable value {value:?}")
            }
        }
    }
}

impl std::error::Error for CatalogError {}

impl From<StoreError> for CatalogError {
    fn from(e: StoreError) -> Self {
        // A fenced write has a specific correct response — stop writing — so it
        // is lifted out of the generic backend error rather than left for a
        // caller to detect by matching on a string.
        match e {
            StoreError::Fenced { .. } => CatalogError::Fenced,
            other => CatalogError::Store(other),
        }
    }
}

/// One recorded poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub id: i64,
    pub source: SourceId,
    pub observed_at: Timestamp,
    pub outcome: ObservationOutcome,
    pub normalized_hash: Option<String>,
    pub raw_hash: Option<String>,
    pub etag: Option<String>,
    pub duration_ms: Option<i64>,
    pub detail: Option<String>,
}

/// A poll about to be recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewObservation {
    pub source: SourceId,
    pub observed_at: Timestamp,
    pub outcome: ObservationOutcome,
    pub normalized_hash: Option<String>,
    pub raw_hash: Option<String>,
    pub etag: Option<String>,
    pub duration_ms: Option<i64>,
    pub detail: Option<String>,
}

/// An era about to be written.
///
/// The observation window is NOT a field a caller supplies. It is derived from
/// the store's own observation history at write time, because the only party
/// that can state fusiform's previous observation instant is fusiform, and a
/// window handed in by a caller is a claim the store cannot check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEra {
    pub source: SourceId,
    pub provider_id: String,
    pub model_id: String,
    pub fact_key: FactKey,
    pub value_json: String,
    pub boundary_at: Timestamp,
    pub boundary_kind: BoundaryKind,
    /// The observation that opened this era. Required for `Observed`, forbidden
    /// for `Seed`.
    pub observation_id: Option<i64>,
}

enum PollWrite<'a> {
    Observation(&'a NewObservation),
    Existing {
        observation_id: Option<i64>,
        now: Timestamp,
    },
    Correction {
        source: SourceId,
        expected: &'a [correct::PlannedCorrection],
        now: Timestamp,
    },
    Document {
        observation: &'a NewObservation,
        catalog: &'a fusiform_core::NormalizedCatalog,
        matching_hash: bool,
    },
}

struct EraWrite {
    count: usize,
    version: Option<i64>,
    observation_id: Option<i64>,
    plan: Option<ingest::IngestPlan>,
}

pub use fusiform_core::upstream_facts::{prefix, rate_key, FactKey};

fn is_rate_relevant_fact(key: &str) -> bool {
    key.starts_with(prefix::RATE)
        || matches!(
            key,
            "existence" | "model.canonical_id" | "model.family" | "model.open_weights"
        )
}

impl CatalogStore {
    /// Open the store from the descriptor delivered over the handshake.
    ///
    /// The descriptor is never self-derived. A module that keys its own storage
    /// path before connecting will eventually key it differently from the
    /// supervisor and quietly run two databases, which is exactly what one
    /// fleet module shipped: a nested `astrocyte/cortexkit/astrocyte/store.db`
    /// alongside the real one.
    pub fn open(descriptor: &StorageDescriptor) -> Result<Self, CatalogError> {
        Self::open_mode(descriptor, false)
    }

    pub fn open_for_store(descriptor: &StorageDescriptor) -> Result<Self, CatalogError> {
        Self::open_mode(descriptor, true)
    }

    pub fn is_store_mode(&self) -> bool {
        self.store_mode
    }

    fn open_mode(descriptor: &StorageDescriptor, store_mode: bool) -> Result<Self, CatalogError> {
        let inner = open_sqlite(descriptor)?;
        // Older media judgments did not record a task. Refuse rather than
        // inventing one while migrating their history to the per-kind schema.
        let legacy_media_grades: i64 = inner.with_conn_fenced(|conn| {
            let needs_kind: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('quality_grade_era') WHERE name = 'grade') \
                 AND NOT EXISTS(SELECT 1 FROM pragma_table_info('quality_grade_era') WHERE name = 'kind')",
                [],
                |row| row.get(0),
            )?;
            if needs_kind {
                conn.query_row(
                    "SELECT COUNT(*) FROM quality_grade_era WHERE grade IS NOT NULL",
                    [],
                    |row| row.get(0),
                )
            } else {
                Ok(0)
            }
        })?;
        if legacy_media_grades != 0 {
            return Err(CatalogError::Invariant(format!(
                "cannot migrate quality grades to schema v13: {legacy_media_grades} legacy media grade rows have no kind"
            )));
        }
        let outcome = if store_mode {
            inner.migrate(
                schema::NAMESPACE,
                &schema::migrations_without_embedded_data(),
            )?
        } else {
            inner.migrate(schema::NAMESPACE, schema::MIGRATIONS)?
        };

        // A store written by a NEWER binary is refused, rather than served from.
        //
        // The migrator deliberately does not refuse — that would brick a binary
        // rollback — so the decision is the module's, and it turns on whether
        // this binary can honestly answer questions about rows it did not write.
        // Fusiform cannot, and the reason is measurable rather than cautious:
        //
        // `boundary_kind` is read back as a raw string and passed to the wire,
        // which is safe on its own. But suppression is a WHERE clause over that
        // column — `boundary_kind = 'corrected'` is what withholds a fact whose
        // recorded value is known bad. Any suppression semantic a later version
        // adds is therefore INVISIBLE to this binary's filter, and the failure is
        // to serve a fact that a newer binary withholds: a confident answer, in
        // the direction nobody audits, from a catalog whose whole purpose is that
        // absent and unknown and withheld are different states.
        //
        // Refusing costs a rolled-back binary its startup and says exactly why.
        // Continuing costs a consumer a wrong price with no way to notice.
        if outcome.store_ahead() {
            return Err(CatalogError::Invariant(format!(
                "the store's schema chain is ahead of this binary: store at {}, \
                 this binary knows {}. A newer version wrote rows this build \
                 cannot interpret — suppression is a filter over `boundary_kind`, \
                 so a boundary kind added later would be served rather than \
                 withheld. Run the newer binary, or restore a store written by \
                 this one.",
                outcome.recorded, outcome.chain_max,
            )));
        }

        // Readers open only after migrations commit, and never acquire a writer
        // lease. The original handle remains the sole fenced writer.
        let path = match &descriptor.backend {
            StorageBackend::Sqlite { path } => path,
            other => return Err(StoreError::UnsupportedBackend(other.label().to_string()).into()),
        };
        let readers = readers::ReadPool::open(path)?;
        let store = Self {
            inner,
            readers,
            path: std::path::PathBuf::from(path),
            instance_id: NEXT_STORE_INSTANCE.fetch_add(1, Ordering::Relaxed),
            catalog_changes: tokio::sync::Notify::new(),
            rates_version: AtomicU64::new(0),
            store_mode,
        };
        store.record_rates_version(store.catalog_version()?);
        if store_mode && store.latest_store_release()?.is_none() {
            store.raw_conn(store_consumer::require_empty_history)?;
        }
        Ok(store)
    }

    /// The database file this store opened.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Process-local identity for caches tied to this open store, not its path.
    /// Reopening or replacing a database must not reuse cached reads even when
    /// its path and catalog version happen to match an earlier open.
    pub fn instance_id(&self) -> u64 {
        self.instance_id
    }

    /// The fence epoch of the held single-writer lease.
    pub fn epoch(&self) -> u64 {
        self.inner.epoch()
    }

    /// Record one poll and return its id.
    ///
    /// Fenced: a superseded writer that still holds an open connection during a
    /// lease handover must not append to the history its replacement is already
    /// writing.
    pub fn record_observation(&self, obs: &NewObservation) -> Result<i64, CatalogError> {
        let written = self.append_eras_inner(&[], Some(PollWrite::Observation(obs)))?;
        Ok(written.observation_id.expect("observation was inserted"))
    }

    /// The most recent observation that CONFIRMED current values.
    ///
    /// A failed poll is excluded, because it observed nothing and cannot bound
    /// anything.
    pub fn last_confirming_observation(
        &self,
        source: SourceId,
    ) -> Result<Option<Timestamp>, CatalogError> {
        self.last_confirming_observation_before(source, Timestamp(i64::MAX))
    }

    /// How many observations sit at or after an instant, newest-inclusive.
    ///
    /// Answers "what must I pass to `--polls` to see that". The status route
    /// needs it for the failure hint, which was a hardcoded 60 until
    /// 2026-08-17 — true when written, and false once the store had polled
    /// past it. Measured that day: the recorded failure was 196 polls back, so
    /// the hint sent an operator to run a costly query that would not show
    /// them the failure it was pointing at.
    ///
    /// A number that describes a moving relationship has to be computed from
    /// the relationship. This is cheap: the observation table is indexed by
    /// (source, instant) and a count over the tail is a covering-index scan of
    /// a few hundred rows.
    pub fn observations_since(&self, source: SourceId, at: Timestamp) -> Result<i64, CatalogError> {
        let n = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM observation WHERE source = ?1 AND observed_at_ms >= ?2",
                params![source.as_str(), at.0],
                |r| r.get::<_, i64>(0),
            )
        })?;
        Ok(n)
    }

    /// The earliest instant this store has any record of, or `None` when it
    /// has none at all.
    ///
    /// # What this is for
    ///
    /// A point-in-time read BEFORE this instant cannot be answered. Not
    /// "answered with nothing" — fusiform has no basis to say anything about a
    /// time it was not watching, and an empty result would state that the
    /// catalog was empty then.
    ///
    /// That distinction is the whole subject of this store: absence must never
    /// be indistinguishable from never-published. Reading at an instant before
    /// the record begins is the version of it that survived longest, because
    /// the response is well-formed and the number zero is a plausible count.
    ///
    /// Taken from `observation` rather than `era`, because an observation is
    /// the claim "fusiform looked at this instant" — which is precisely the
    /// coverage question. The seed records one at the snapshot's fetch time,
    /// so a freshly seeded store reports the moment its snapshot was taken and
    /// not the moment it was installed.
    pub fn record_begins_at(&self, source: SourceId) -> Result<Option<Timestamp>, CatalogError> {
        let ts = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT MIN(observed_at_ms) FROM observation WHERE source = ?1",
                params![source.as_str()],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()
        })?;
        Ok(ts.flatten().map(Timestamp))
    }

    /// The most recent confirming observation STRICTLY BEFORE an instant.
    ///
    /// This is what an era's `prior_observation_at` is, and the strictness is
    /// load-bearing. An era's boundary instant is the observation that DETECTED
    /// the change, and that observation is already recorded when the era is
    /// written — so asking for "the latest confirming observation" returns the
    /// boundary itself and collapses the window to zero width, claiming the
    /// change happened in an instant of no duration.
    ///
    /// The near edge is the last time fusiform saw the OLD value. That is the
    /// observation before the one that opened the era.
    pub fn last_confirming_observation_before(
        &self,
        source: SourceId,
        instant: Timestamp,
    ) -> Result<Option<Timestamp>, CatalogError> {
        self.raw_conn(|conn| last_confirming_before_in(conn, source, instant))
    }

    /// Append eras, deriving each observed boundary's window from the store's
    /// own history.
    ///
    /// All eras in one call commit together. A partially-applied change would
    /// leave the catalog describing a state the upstream never published — some
    /// facts moved, others not — and a consumer reading between the two writes
    /// would believe it, and nothing later would tell it otherwise.
    pub fn append_eras(&self, eras: &[NewEra]) -> Result<usize, CatalogError> {
        if eras.is_empty() {
            return Ok(0);
        }
        self.append_eras_inner(eras, None)
            .map(|written| written.count)
    }

    /// Commit a changed poll's eras and catalog version together. Publish the
    /// derived rates version only after the transaction succeeds.
    ///
    /// One transaction, so a version is never visible ahead of its eras: a
    /// consumer that read a version whose content was missing would hold a
    /// high-water mark for a catalog it never received, and refuse the read
    /// that carried it. The store derives the version (`max(now_ms,
    /// current + 1)`) so a restore cannot rewind it.
    pub fn append_poll_eras(
        &self,
        eras: &[NewEra],
        observation_id: i64,
        now: Timestamp,
    ) -> Result<(usize, i64), CatalogError> {
        let written = self.append_eras_inner(
            eras,
            Some(PollWrite::Existing {
                observation_id: Some(observation_id),
                now,
            }),
        )?;
        let issued = written
            .version
            .expect("appending poll eras always issues a catalog version");
        self.publish_poll_write(eras, issued);
        Ok((written.count, issued))
    }

    /// Diff a normalized body and commit its observation, eras and version
    /// together. A matching saved digest can still require a diff on startup:
    /// older writers could persist a digest without its eras.
    pub fn commit_polled_document(
        &self,
        observation: &NewObservation,
        catalog: &fusiform_core::NormalizedCatalog,
        matching_hash: bool,
    ) -> Result<(i64, ingest::IngestPlan, Option<i64>), CatalogError> {
        let written = self.append_eras_inner(
            &[],
            Some(PollWrite::Document {
                observation,
                catalog,
                matching_hash,
            }),
        )?;
        let plan = written.plan.expect("a polled document is diffed");
        if let Some(version) = written.version {
            self.publish_poll_write(&plan.eras, version);
        }
        Ok((
            written.observation_id.expect("observation was inserted"),
            plan,
            written.version,
        ))
    }

    fn publish_poll_write(&self, eras: &[NewEra], issued: i64) {
        if eras
            .iter()
            .any(|era| is_rate_relevant_fact(era.fact_key.as_str()))
        {
            self.record_rates_version(issued);
        }
        self.signal_catalog_changed();
    }

    /// Corrections change what dated reads serve, even when current rates stay
    /// unchanged. Invalidate page tokens in the same transaction as the eras.
    pub(crate) fn append_correction_eras(
        &self,
        source: SourceId,
        eras: &[NewEra],
        expected: &[correct::PlannedCorrection],
        now: Timestamp,
    ) -> Result<usize, CatalogError> {
        if eras.is_empty() {
            return Ok(0);
        }
        let written = self.append_eras_inner(
            eras,
            Some(PollWrite::Correction {
                source,
                expected,
                now,
            }),
        )?;
        self.signal_catalog_changed();
        Ok(written.count)
    }

    fn append_eras_inner(
        &self,
        eras: &[NewEra],
        poll: Option<PollWrite<'_>>,
    ) -> Result<EraWrite, CatalogError> {
        // A correction's declared extent must name the fact it is written on.
        //
        // The row says "this fact's recorded value was wrong"; the extent says
        // which fields a consumer should partition on. One belief, two
        // statements, and each is well formed alone — so a correction on
        // `rate.input` declaring an extent about `limit.context` passes every
        // other check and tells a consumer to partition the wrong charges.
        //
        // Containment rather than equality: a single defect can span several
        // facts, and each row may carry the whole defect's extent. What it may
        // not do is omit its own fact.
        for era in eras {
            let BoundaryKind::Corrected(correction) = &era.boundary_kind else {
                continue;
            };
            if correction.fields.is_empty() {
                return Err(CatalogError::Invariant(format!(
                    "{}/{} {}: a correction must name at least one field; an extent \
                     naming nothing cannot be partitioned on",
                    era.provider_id,
                    era.model_id,
                    era.fact_key.as_str()
                )));
            }
            // Fields with no single fact key (tier and charge-unit corrections)
            // cannot be checked this way and are accepted: a tiered fact key
            // carries a threshold the FieldId does not know.
            let unmappable = correction
                .fields
                .iter()
                .any(|f| FactKey::for_field(f.clone()).is_none());
            if unmappable {
                continue;
            }
            let names_own_fact = correction
                .fields
                .iter()
                .filter_map(|f| FactKey::for_field(f.clone()))
                .any(|k| k == era.fact_key);
            if !names_own_fact {
                return Err(CatalogError::Invariant(format!(
                    "{}/{} {}: the correction's extent names {:?}, which does not \
                     include the fact it is written on",
                    era.provider_id,
                    era.model_id,
                    era.fact_key.as_str(),
                    correction.fields
                )));
            }
        }

        // A correction's interval must be ordered and must end at or before the
        // instant it is recorded. An extent reaching into the future claims
        // fusiform knows a value it has not observed yet.
        for era in eras {
            let BoundaryKind::Corrected(correction) = &era.boundary_kind else {
                continue;
            };
            if correction.affected_from > correction.affected_until {
                return Err(CatalogError::Invariant(format!(
                    "{}/{} {}: correction interval runs backwards ({} to {})",
                    era.provider_id,
                    era.model_id,
                    era.fact_key.as_str(),
                    correction.affected_from.0,
                    correction.affected_until.0
                )));
            }
            if correction.affected_until > era.boundary_at {
                return Err(CatalogError::Invariant(format!(
                    "{}/{} {}: correction extends to {} but is recorded at {}; a \
                     correction cannot cover instants fusiform has not reached",
                    era.provider_id,
                    era.model_id,
                    era.fact_key.as_str(),
                    correction.affected_until.0,
                    era.boundary_at.0
                )));
            }
        }

        let written = self.inner.with_conn_fenced(|tx| {
            // Validate explicit observed eras before making any changes. Read
            // each distinct window here so it shares the writer transaction.
            let mut prior_by_key = BTreeMap::new();
            for era in eras
                .iter()
                .filter(|era| matches!(era.boundary_kind, BoundaryKind::Observed))
            {
                let key = (era.source, era.boundary_at.0);
                if let std::collections::btree_map::Entry::Vacant(slot) = prior_by_key.entry(key) {
                    let Some(prior) = last_confirming_before_in(tx, era.source, era.boundary_at)?
                    else {
                        return Ok(Err(CatalogError::Invariant(format!(
                            "{}/{} {}: an observed boundary needs a prior confirming observation; \
                             the first era for a source is a seed",
                            era.provider_id,
                            era.model_id,
                            era.fact_key.as_str()
                        ))));
                    };
                    slot.insert(prior);
                }
            }
            let insert_observation =
                |obs: &NewObservation, outcome: &ObservationOutcome| -> rusqlite::Result<i64> {
                    let (outcome, failure_class) = outcome_columns(outcome);
                    tx.execute(
                        "INSERT INTO observation \
                     (source, observed_at_ms, outcome, failure_class, detail, \
                      normalized_hash, raw_hash, etag, duration_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            obs.source.as_str(),
                            obs.observed_at.0,
                            outcome,
                            failure_class,
                            obs.detail,
                            obs.normalized_hash,
                            obs.raw_hash,
                            obs.etag,
                            obs.duration_ms
                        ],
                    )?;
                    Ok(tx.last_insert_rowid())
                };
            let mut document = None;
            let (observation_id, version_at) = match poll {
                None => (None, None),
                Some(PollWrite::Observation(obs)) => {
                    (Some(insert_observation(obs, &obs.outcome)?), None)
                }
                Some(PollWrite::Existing {
                    observation_id,
                    now,
                }) => (observation_id, Some(now)),
                Some(PollWrite::Correction {
                    source,
                    expected,
                    now,
                }) => {
                    if let Err(error) = correct::check_expected_eras_in(tx, source, expected, now)?
                    {
                        return Ok(Err(error));
                    }
                    (None, Some(now))
                }
                Some(PollWrite::Document {
                    observation,
                    catalog,
                    matching_hash,
                }) => {
                    let at = observation.observed_at;
                    let prior = last_confirming_before_in(tx, observation.source, at)?;
                    let boundary = if prior.is_some() {
                        BoundaryKind::Observed
                    } else {
                        BoundaryKind::Seed
                    };
                    let mut plan = ingest::plan_ingest_in(tx, catalog, at, boundary, None)?;
                    if !plan.is_empty() {
                        let latest: Option<i64> = tx.query_row(
                            "SELECT MAX(boundary_at_ms) FROM era WHERE source = ?1",
                            [observation.source.as_str()],
                            |r| r.get(0),
                        )?;
                        if let Some(latest) = latest.filter(|&latest| latest >= at.0) {
                            return Ok(Err(CatalogError::PollBoundaryNotIncreasing {
                                observed_at: at,
                                latest: Timestamp(latest),
                            }));
                        }
                    }
                    let unchanged = matching_hash && plan.is_empty();
                    let outcome = if unchanged {
                        ObservationOutcome::Unchanged
                    } else {
                        ObservationOutcome::Changed { snapshot_seq: 0 }
                    };
                    let id = insert_observation(observation, &outcome)?;
                    if prior.is_some() {
                        for era in &mut plan.eras {
                            era.observation_id = Some(id);
                        }
                    }
                    document = Some((plan, prior));
                    (Some(id), (!unchanged).then_some(at))
                }
            };
            let eras = document.as_ref().map_or(eras, |(plan, _)| &plan.eras);
            let mut count = 0usize;
            for era in eras {
                let (kind, correction) = boundary_columns(&era.boundary_kind);
                let prior_ms = match era.boundary_kind {
                    BoundaryKind::Observed => {
                        let prior = if let Some((_, prior)) = &document {
                            *prior
                        } else {
                            prior_by_key.get(&(era.source, era.boundary_at.0)).copied()
                        };
                        prior.map(|t| t.0)
                    }
                    _ => None,
                };
                tx.execute(
                    "INSERT INTO era \
                     (source, provider_id, model_id, fact_key, value_json, \
                      boundary_at_ms, boundary_kind, prior_observation_at_ms, \
                      observation_id, corrected_fields_json, affected_from_ms, \
                      affected_until_ms, correction_reason) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![
                        era.source.as_str(),
                        era.provider_id,
                        era.model_id,
                        era.fact_key.as_str(),
                        era.value_json,
                        era.boundary_at.0,
                        kind,
                        prior_ms,
                        era.observation_id,
                        correction.as_ref().map(|c| c.fields_json.clone()),
                        correction.as_ref().map(|c| c.affected_from),
                        correction.as_ref().map(|c| c.affected_until),
                        correction.as_ref().map(|c| c.reason.clone()),
                    ],
                )?;
                count += 1;
            }
            let version = version_at
                .map(|now| advance_catalog_version_in(tx, observation_id, now))
                .transpose()?;
            Ok(Ok(EraWrite {
                count,
                version,
                observation_id,
                plan: document.map(|(plan, _)| plan),
            }))
        })?;

        written
    }

    /// The value a fact held at an instant, with the boundary that established
    /// it.
    ///
    /// Returns `None` when the instant precedes every era for the fact — which
    /// is a real answer ("the catalog did not know yet"), not an absence of
    /// data.
    /// What the store RECORDED for a fact at an instant.
    ///
    /// Selects by `boundary_at <= at` and nothing else, so a correction written
    /// later is invisible to it by construction. That is the right answer to
    /// "what did the store say at T" and the WRONG answer to "what was true at
    /// T" — see [`Self::value_at`], which is the one a consumer wants.
    ///
    /// Public because the distinction is real: an auditor reconstructing what
    /// fusiform believed at a past instant needs the recorded value even when
    /// it is known bad. Named so no caller reaches it by accident.
    pub fn recorded_value_at(
        &self,
        source: SourceId,
        provider_id: &str,
        model_id: &str,
        fact: &FactKey,
        at: Timestamp,
    ) -> Result<Option<EraRow>, CatalogError> {
        let row = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT id, value_json, boundary_at_ms, boundary_kind, \
                        prior_observation_at_ms, observation_id, \
                        corrected_fields_json, affected_from_ms, affected_until_ms, \
                        correction_reason \
                 FROM era \
                 WHERE source = ?1 AND provider_id = ?2 AND model_id = ?3 \
                   AND fact_key = ?4 AND boundary_at_ms <= ?5 \
                 ORDER BY boundary_at_ms DESC LIMIT 1",
                params![source.as_str(), provider_id, model_id, fact.as_str(), at.0],
                |r| {
                    Ok(RawEraRow {
                        id: r.get(0)?,
                        value_json: r.get(1)?,
                        boundary_at_ms: r.get(2)?,
                        boundary_kind: r.get(3)?,
                        prior_observation_at_ms: r.get(4)?,
                        observation_id: r.get(5)?,
                        corrected_fields_json: r.get(6)?,
                        affected_from_ms: r.get(7)?,
                        affected_until_ms: r.get(8)?,
                        correction_reason: r.get(9)?,
                    })
                },
            )
            .optional()
        })?;

        row.map(decode_era_row).transpose()
    }

    /// What was TRUE for a fact at an instant, or a refusal.
    ///
    /// The difference from [`Self::recorded_value_at`] is corrections. A
    /// `Corrected` era says the value recorded across its
    /// `affected_from..affected_until` interval was wrong. Selecting by
    /// `boundary_at <= at` cannot see it — the correction is written AFTER the
    /// interval it describes — so a plain point-in-time read lands on the bad
    /// era and returns it with every appearance of confidence.
    ///
    /// So a read inside a corrected interval refuses. Returning the corrected
    /// value instead would be the other wrong answer: fusiform does not know
    /// what the upstream held at that instant, only that its own record was
    /// bad. Returning the recorded value is how a consumer re-prices against a
    /// rate that was never real.
    ///
    /// This is why [`Correction`] carries an interval rather than a timestamp.
    /// Without one a corrected era is indistinguishable from a later change,
    /// and the read has nothing to refuse on.
    pub fn value_at(
        &self,
        source: SourceId,
        provider_id: &str,
        model_id: &str,
        fact: &FactKey,
        at: Timestamp,
    ) -> Result<PointInTime, CatalogError> {
        // Corrections are found by their EXTENT, not their boundary: one
        // written at any later time invalidates reads inside the interval it
        // names.
        let corrections = self.corrections_covering(source, provider_id, model_id, fact, at)?;
        if !corrections.is_empty() {
            return Ok(PointInTime::Corrected { corrections });
        }
        let Some(row) = self.recorded_value_at(source, provider_id, model_id, fact, at)? else {
            return Ok(PointInTime::Unknown);
        };

        // Is `at` inside a LATER era's observation window?
        //
        // An era saying "observed at T, previously confirmed at P" states that
        // the change happened somewhere in (P, T]. If `at` falls in that
        // interval, the era answering this read may already have ended before
        // `at` — fusiform was not looking. Answering with the value alone is a
        // confident claim the store cannot support.
        //
        // The uncertainty was always computable from these rows and no read
        // reported it. Found by probing what a restore does to history: the
        // rewind produces one enormous window, and every read inside it
        // answered confidently.
        // The interval excludes both endpoints. `prior` is an instant fusiform
        // CONFIRMED the value, so it is certain.
        //
        // The upper exclusion is unreachable rather than merely correct, and
        // saying so keeps anyone from testing for it: the era answering this
        // read has `boundary_at <= at`, and the next era is selected with
        // `boundary_at > row.boundary_at`. If that next boundary equalled `at`,
        // the read would have landed on THAT era instead. So `at == boundary`
        // cannot occur here, and a mutation widening this to `<=` is an
        // equivalent mutant.
        let next = self.next_era_window(source, provider_id, model_id, fact, row.boundary_at)?;
        if let Some((prior, boundary)) = next {
            if prior.0 < at.0 && at.0 < boundary.0 {
                return Ok(PointInTime::KnownStale {
                    row,
                    superseded_after: prior,
                    superseded_by: boundary,
                });
            }
        }

        Ok(PointInTime::Known(row))
    }

    /// The window of the first era after an instant, when it has one.
    ///
    /// Only `Observed` boundaries carry a window; a seed or a correction states
    /// no interval, so there is nothing to be uncertain inside.
    fn next_era_window(
        &self,
        source: SourceId,
        provider_id: &str,
        model_id: &str,
        fact: &FactKey,
        after: Timestamp,
    ) -> Result<Option<(Timestamp, Timestamp)>, CatalogError> {
        let row = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT prior_observation_at_ms, boundary_at_ms FROM era \
                 WHERE source = ?1 AND provider_id = ?2 AND model_id = ?3 \
                   AND fact_key = ?4 AND boundary_at_ms > ?5 \
                   AND prior_observation_at_ms IS NOT NULL \
                 ORDER BY boundary_at_ms ASC LIMIT 1",
                params![
                    source.as_str(),
                    provider_id,
                    model_id,
                    fact.as_str(),
                    after.0
                ],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()
        })?;
        Ok(row.map(|(p, b)| (Timestamp(p), Timestamp(b))))
    }

    /// Every correction covering an instant, across all models and facts.
    ///
    /// One query rather than a lookup per fact. A bulk read resolves ~67,000
    /// facts, and asking about each one separately would be 67,000 queries to
    /// discover that corrections are rare — currently zero. This fetches the
    /// whole (small) set once and the caller filters in memory.
    pub fn all_corrections_covering(
        &self,
        source: SourceId,
        at: Timestamp,
    ) -> Result<CorrectionsByFact, CatalogError> {
        let rows = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(CORRECTIONS_COVERING_SQL)?;
            let mapped = stmt.query_map(params![source.as_str(), at.0], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    FactKey::from_stored(r.get::<_, String>(2)?),
                    CorrectionRecord {
                        corrected_at: Timestamp(r.get(3)?),
                        fields_json: r.get(4)?,
                        affected_from: Timestamp(r.get(5)?),
                        affected_until: Timestamp(r.get(6)?),
                        reason: r.get(7)?,
                    },
                ))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })?;

        let mut by_fact: BTreeMap<(String, String, FactKey), Vec<CorrectionRecord>> =
            BTreeMap::new();
        for (provider_id, model_id, fact_key, record) in rows {
            by_fact
                .entry((provider_id, model_id, fact_key))
                .or_default()
                .push(record);
        }
        Ok(by_fact)
    }

    /// Corrections whose extent covers an instant, oldest first.
    ///
    /// The interval is closed at both ends. `affected_from` is a lower bound
    /// that goes to the earliest plausible instant when the true start is
    /// unknown, so over-inclusion is the intended direction: an unnecessary
    /// refusal costs a consumer a question, a missed one costs them a wrong
    /// number they cannot detect.
    pub fn corrections_covering(
        &self,
        source: SourceId,
        provider_id: &str,
        model_id: &str,
        fact: &FactKey,
        at: Timestamp,
    ) -> Result<Vec<CorrectionRecord>, CatalogError> {
        let rows = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(CORRECTIONS_FOR_FACT_SQL)?;
            let mapped = stmt.query_map(
                params![source.as_str(), provider_id, model_id, fact.as_str(), at.0],
                |r| {
                    Ok(CorrectionRecord {
                        corrected_at: Timestamp(r.get(0)?),
                        fields_json: r.get(1)?,
                        affected_from: Timestamp(r.get(2)?),
                        affected_until: Timestamp(r.get(3)?),
                        reason: r.get(4)?,
                    })
                },
            )?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows)
    }

    /// Check the store's pages without taking the poll writer's lock or lease.
    ///
    /// Only the first SQLite result is retained. An error running the check is
    /// evidence that the check could not pass, not an implicit `ok`.
    pub fn quick_check(&self) -> String {
        self.readers
            .with_conn(|conn| {
                conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            })
            .unwrap_or_else(|error| error.to_string())
    }

    /// Run a read against a pooled read-only connection.
    ///
    /// Reads only. Writes go through `with_conn_fenced`, which checks that this
    /// process still holds the current writer lease before applying anything:
    /// when one instance replaces another, the outgoing one can briefly still
    /// have an open connection after losing the lease, and that check is what
    /// stops its late writes from landing on top of its replacement's.
    /// Reads see committed WAL snapshots, not the writer's uncommitted changes;
    /// reads needed within a write transaction must use its `tx` instead.
    pub(crate) fn raw_conn<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T>,
    ) -> Result<T, CatalogError> {
        Ok(self.readers.with_conn(f)?)
    }

    /// Run related reads synchronously on one pooled connection. The first
    /// query establishes the snapshot; nested reads reuse it until the callback
    /// returns. A concurrent writer can commit without waiting for this read.
    pub fn with_read_snapshot<T>(&self, read: impl FnOnce() -> T) -> Result<T, CatalogError> {
        Ok(self.readers.with_snapshot(read)?)
    }

    /// The ETag from the most recent observation that received one.
    ///
    /// Read from the store rather than held in memory, so it survives a
    /// restart. The poll loop still sends no validator on a process's first
    /// poll, until that process has read a full document, because a 304 means
    /// the bytes are unchanged, not that the running binary has read them.
    pub fn last_etag(&self, source: SourceId) -> Result<Option<String>, CatalogError> {
        let etag = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT etag FROM observation \
                 WHERE source = ?1 AND etag IS NOT NULL \
                 ORDER BY observed_at_ms DESC LIMIT 1",
                params![source.as_str()],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })?;
        Ok(etag)
    }

    /// The normalized digest from the most recent observation that computed one.
    ///
    /// Only observations that parsed a body carry a digest, so a 304 or a
    /// failure between two full fetches does not erase the comparison basis.
    pub fn last_normalized_hash(&self, source: SourceId) -> Result<Option<String>, CatalogError> {
        let hash = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT normalized_hash FROM observation \
                 WHERE source = ?1 AND normalized_hash IS NOT NULL \
                 ORDER BY observed_at_ms DESC LIMIT 1",
                params![source.as_str()],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })?;
        Ok(hash)
    }

    /// The raw-bytes hash recorded on one observation, if it has one.
    ///
    /// Exists for the failure path: a parse failure records which exact
    /// document could not be read, so a later fix can be checked against those
    /// bytes rather than against whatever the upstream is serving by then.
    /// Observations whose eras every one state the claim already in force, and
    /// which are therefore artifacts of fusiform's own doing.
    ///
    /// # Why this can be decided mechanically rather than by an operator
    ///
    /// The ingest diff writes an era only when the incoming claim DIFFERS from
    /// the one held. So under the current diff, a poll whose every era restates
    /// the held claim cannot occur — such eras exist only because an earlier
    /// diff compared bytes, which is exactly the 2026-08-16 defect.
    ///
    /// That makes the rule sound by construction rather than by judgement: the
    /// signature identifies precisely the polls a byte-comparing diff could
    /// produce and a claim-comparing one cannot.
    ///
    /// Returns nothing on a store with no such poll, which includes every fresh
    /// install — so this is a repair that finds its own subject rather than a
    /// constant that assumes one.
    ///
    /// # MEASURED, and it is why the threshold is `same > 0` rather than a ratio
    ///
    /// Live store, 2026-08-24:
    ///
    /// ```text
    /// obs 203   same=17454  total=17455    the provenance incident
    /// obs 194   same=0      total=340      an ordinary changed poll
    /// obs 134   same=0      total=2388     an ordinary changed poll
    /// ```
    ///
    /// Ordinary polls score EXACTLY ZERO, which is the property that makes this
    /// sound: a claim-comparing diff cannot write an era restating what is held,
    /// so a single such era is already evidence of the byte-comparing diff. A
    /// ratio would invite a threshold; the honest rule is presence.
    ///
    /// And the incident is not pure — 17,454 of its 17,455 eras are artifacts,
    /// and ONE IS A GENUINE UPSTREAM CHANGE that landed in the same tick. A
    /// thirty-minute poll catching a real change alongside the rewrite is the
    /// expected case rather than bad luck.
    ///
    /// That is why this identifies observations for INSPECTION rather than for
    /// automatic marking: marking observation 203 would exclude its one real era
    /// along with the noise, which is the same class of loss the mark exists to
    /// prevent. Per-era exclusion is the exact answer and is not built yet.
    pub fn artifact_observations(&self) -> Result<Vec<i64>, CatalogError> {
        let candidates: Vec<i64> = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT observation_id FROM era \
                 WHERE observation_id IS NOT NULL AND boundary_kind = 'observed'",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
        })?;

        let mut found = Vec::new();
        for id in candidates {
            let (same, total) = self.same_claim_era_fraction(id)?;
            // A single same-claim era is already proof: the current diff cannot
            // write one, so its presence dates the poll to the byte-comparing
            // diff. Requiring ALL of them misses the real incident, whose 17,455
            // eras include one genuine change.
            if total > 0 && same > 0 {
                found.push(id);
            }
        }
        Ok(found)
    }

    /// How many of a poll's eras state the same upstream claim as the era they
    /// superseded, against how many it wrote.
    ///
    /// # Why this compares claims rather than bytes
    ///
    /// The obvious version compares `value_json` textually and CANNOT SEE THE
    /// THING IT EXISTS FOR. Measured on the live store: the 2026-08-16 artifact
    /// poll scores 0 same-value eras out of 17,455, because its eras carry
    /// `unit_provenance` and the eras they superseded do not — identical claim,
    /// different text. A serialization-change artifact is by definition the case
    /// where the text differs and the meaning does not.
    ///
    /// So it strips producer annotations with the same function the ingest diff
    /// uses. One rule, one definition of what the upstream said.
    ///
    /// # What it is for
    ///
    /// Marking an observation as an artifact deletes real history from every
    /// derivation that honours the mark, and a mistaken mark is unfalsifiable
    /// afterwards. This lets the claim be checked before it is written: a ratio
    /// near 1.0 is the signature, and anything else is a poll that changed real
    /// values and must not be marked.
    pub fn same_claim_era_fraction(&self, observation_id: i64) -> Result<(i64, i64), CatalogError> {
        let pairs: Vec<(String, Option<String>)> = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT e.value_json, ( \
                     SELECT p.value_json FROM era p \
                     WHERE p.source = e.source AND p.provider_id = e.provider_id \
                       AND p.model_id = e.model_id AND p.fact_key = e.fact_key \
                       AND p.boundary_at_ms < e.boundary_at_ms \
                     ORDER BY p.boundary_at_ms DESC LIMIT 1 \
                   ) FROM era e WHERE e.observation_id = ?1",
            )?;
            let rows = stmt.query_map(params![observation_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })?;

        let total = pairs.len() as i64;
        let same = pairs
            .iter()
            .filter(|(current, previous)| {
                previous
                    .as_deref()
                    .is_some_and(|p| ingest::states_the_same_upstream_claim(p, current))
            })
            .count() as i64;
        Ok((same, total))
    }

    /// Record that a poll's eras describe fusiform changing its own
    /// representation rather than the upstream changing its data.
    ///
    /// `reason` names a finding rather than describing one, so an operator
    /// meets the evidence instead of a summary of it.
    ///
    /// Fenced like every other write: an artifact is a claim about this
    /// store's history and must not be written by a process that has lost its
    /// lease.
    pub fn mark_observation_artifact(
        &self,
        observation_id: i64,
        reason: &str,
        recorded_at: Timestamp,
    ) -> Result<(), CatalogError> {
        self.inner.with_conn_fenced(|conn| {
            conn.execute(
                "INSERT INTO observation_artifact_event \
                 (observation_id, action, reason, recorded_at_ms) \
                 VALUES (?1, 'marked', ?2, ?3)",
                params![observation_id, reason, recorded_at.0],
            )
        })?;
        Ok(())
    }

    /// The instant a fact's value last genuinely changed, ignoring eras written
    /// by polls marked as artifacts.
    ///
    /// # Why the exclusion is here rather than in the caller
    ///
    /// The whole point of the field is that a consumer cannot be expected to
    /// know which of this store's polls were fusiform's own doing. An exclusion
    /// a caller has to remember is an exclusion that will be forgotten by
    /// whoever inherits the caller, and the resulting number does not look
    /// wrong — it makes every abandoned row appear freshly maintained.
    ///
    /// Corrections only restate the value in force. Returns `None` when the
    /// fact has no non-correction era. A fact whose only era is
    /// the seed returns the seed instant, which is honest: the value has been
    /// true since at least then and has never been seen to move.
    pub fn last_changed_at(
        &self,
        source: SourceId,
        provider_id: &str,
        model_id: &str,
        fact_key: &FactKey,
    ) -> Result<Option<Timestamp>, CatalogError> {
        self.last_changed_at_or_before(source, provider_id, model_id, fact_key, Timestamp(i64::MAX))
    }

    /// Like `last_changed_at`, but only consider eras in force by the snapshot instant.
    pub fn last_changed_at_or_before(
        &self,
        source: SourceId,
        provider_id: &str,
        model_id: &str,
        fact_key: &FactKey,
        at: Timestamp,
    ) -> Result<Option<Timestamp>, CatalogError> {
        // Per ERA, not per observation — measured, and the measurement changed
        // this design.
        //
        // The provenance incident wrote 17,455 eras in one poll. 17,454 of them
        // restate the claim already in force; ONE is a genuine upstream change
        // that happened to land in the same thirty-minute tick. Excluding the
        // whole observation would delete that change from history, which is the
        // same loss the mark exists to prevent, pointed the other way.
        //
        // Real changes arrive continuously, so a marked poll containing one is
        // the expected case rather than bad luck.
        //
        // The comparison is `states_the_same_upstream_claim`, the same function
        // the ingest diff uses. Computed here rather than denormalized into a
        // table at mark time, so there is ONE definition of "restates the held
        // claim" and no stored set that can drift from it. Cheap because facts
        // carry few eras — 68,419 of them have exactly one, 140 have two.
        self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT e.boundary_at_ms, e.value_json, \
                        (e.observation_id IS NOT NULL AND e.observation_id IN \
                         (SELECT observation_id FROM observation_artifact_event ev \
                          WHERE ev.id = (SELECT MAX(id) \
                                         FROM observation_artifact_event \
                                         WHERE observation_id = ev.observation_id) \
                            AND ev.action = 'marked')) \
                 FROM era e \
                  WHERE e.source = ?1 AND e.provider_id = ?2 AND e.model_id = ?3 \
                    AND e.fact_key = ?4 AND e.boundary_at_ms <= ?5 AND e.boundary_kind <> 'corrected' \
                 ORDER BY e.boundary_at_ms DESC",
            )?;
            let mut rows = stmt.query(params![
                source.as_str(),
                provider_id,
                model_id,
                fact_key.as_str(),
                at.0
            ])?;
            let Some(mut row) = rows.next()? else {
                return Ok(None);
            };
            loop {
                let boundary = Timestamp(row.get(0)?);
                // The newest unmarked row resolves the query without scanning
                // its predecessors. A marked row needs one predecessor to
                // tell whether it restated the claim or recorded a real change.
                if !row.get::<_, bool>(2)? {
                    return Ok(Some(boundary));
                }
                let value: String = row.get(1)?;
                let Some(previous) = rows.next()? else {
                    return Ok(Some(boundary));
                };
                if !ingest::states_the_same_upstream_claim(serve::row_text(previous, 1)?, &value) {
                    return Ok(Some(boundary));
                }
                row = previous;
            }
        })
    }

    /// Every mark and retraction recorded against one poll, oldest first.
    ///
    /// The log rather than the derived state, because "was this ever marked,
    /// and did someone take it back" is a different question from "is it marked
    /// now" — and the first is the one an operator asks when a derivation
    /// changed under them.
    pub fn artifact_events(
        &self,
        observation_id: i64,
    ) -> Result<Vec<(String, String)>, CatalogError> {
        let rows = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT action, reason FROM observation_artifact_event \
                 WHERE observation_id = ?1 ORDER BY id ASC",
            )?;
            let mapped = stmt.query_map(params![observation_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()
        })?;
        Ok(rows)
    }

    /// Take back a mark, recording that it was taken back.
    ///
    /// An APPEND, not an update: the mark stays in the log and a retraction
    /// event follows it. An operator asking why `last_changed_at` moved needs
    /// to see that a claim was made and withdrawn — the claim merely being
    /// absent tells them nothing about who decided what.
    ///
    /// Returns whether there was a live mark to retract, so a caller can tell
    /// "I took it back" from "there was nothing there" rather than reporting
    /// success either way. That distinction is the same one the refusal
    /// contract draws between a caller defect and a coverage answer.
    pub fn retract_observation_artifact(
        &self,
        observation_id: i64,
        reason: &str,
        retracted_at: Timestamp,
    ) -> Result<bool, CatalogError> {
        if self.observation_artifact_reason(observation_id)?.is_none() {
            return Ok(false);
        }
        self.inner.with_conn_fenced(|conn| {
            conn.execute(
                "INSERT INTO observation_artifact_event \
                 (observation_id, action, reason, recorded_at_ms) \
                 VALUES (?1, 'retracted', ?2, ?3)",
                params![observation_id, reason, retracted_at.0],
            )
        })?;
        Ok(true)
    }

    /// The reason recorded when a poll was marked an artifact, or `None` if it
    /// was never marked.
    ///
    /// Exists so a caller can check what the STORE holds rather than what a
    /// response claimed. Those are two different artifacts derived from one
    /// intent, and a test asserting only the response proves what the code says
    /// about itself.
    pub fn observation_artifact_reason(&self, id: i64) -> Result<Option<String>, CatalogError> {
        let reason = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT reason FROM observation_artifact_event ev \
                 WHERE ev.observation_id = ?1 AND ev.action = 'marked' \
                   AND ev.id = (SELECT MAX(id) FROM observation_artifact_event \
                                WHERE observation_id = ?1)",
                params![id],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })?;
        Ok(reason)
    }

    /// When one observation was taken, or `None` if this store has no such id.
    ///
    /// Exists so an operator marking a poll can check the id names the poll they
    /// mean BEFORE committing. Observation ids are per-store and carry no
    /// meaning across installs, so a number alone is not something anyone can
    /// verify by reading it.
    pub fn observation_observed_at(&self, id: i64) -> Result<Option<Timestamp>, CatalogError> {
        let at = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT observed_at_ms FROM observation WHERE id = ?1",
                params![id],
                |r| r.get::<_, i64>(0),
            )
            .optional()
        })?;
        Ok(at.map(Timestamp))
    }

    pub fn raw_hash_of_observation(&self, id: i64) -> Result<Option<String>, CatalogError> {
        let hash = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT raw_hash FROM observation WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
        })?;
        Ok(hash.flatten())
    }

    /// The failure class recorded on one observation, if it failed.
    ///
    /// Exists so a test can check that the class a caller was told matches the
    /// class that was written down. Those are two different artifacts derived
    /// from one intent, and nothing else compares them.
    pub fn failure_class_of_observation(&self, id: i64) -> Result<Option<String>, CatalogError> {
        let class = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT failure_class FROM observation WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
        })?;
        Ok(class.flatten())
    }

    /// The current catalog version.
    pub fn catalog_version(&self) -> Result<i64, CatalogError> {
        let v = self.raw_conn(|conn| {
            conn.query_row(
                "SELECT version FROM catalog_version WHERE id = 1",
                [],
                |r| r.get::<_, i64>(0),
            )
        })?;
        Ok(v)
    }

    /// The catalog version issued at startup or for the last rate-relevant
    /// poll or signed release.
    /// Read this before `catalog_version` when serving the pair so a concurrent
    /// commit cannot make the rates version appear newer than the catalog.
    pub fn rates_version(&self) -> u64 {
        self.rates_version.load(Ordering::Acquire)
    }

    fn record_rates_version(&self, issued: i64) {
        self.rates_version
            .fetch_max(issued as u64, Ordering::Release);
    }

    /// A coalesced wake after a committed version advance, not a version payload.
    pub async fn catalog_changed(&self) {
        self.catalog_changes.notified().await;
    }

    pub(crate) fn signal_catalog_changed(&self) {
        self.catalog_changes.notify_one();
    }

    /// Append-only era watermark for invalidating derived reads. Corrections
    /// and standalone era appends need not advance the public catalog version.
    /// MAX on the integer primary key visits only the last entry, not all eras.
    pub fn latest_era_id(&self) -> Result<i64, CatalogError> {
        self.raw_conn(|conn| {
            conn.query_row("SELECT COALESCE(MAX(id), 0) FROM era", [], |r| r.get(0))
        })
    }

    /// Advance the catalog version to a restore-invariant value.
    ///
    /// The version is `max(now_ms, current + 1)`, not `current + 1`. Both terms
    /// are load-bearing and each covers what the other cannot:
    ///
    /// - **`now_ms`** survives a restore. A whole-db capture restores this
    ///   counter along with everything else, so a counter derived only from its
    ///   own previous value rewinds to whatever the backup held. Wall-clock time
    ///   does not rewind, so the first version issued after a restore already
    ///   exceeds every version issued before it.
    /// - **`current + 1`** survives the clock. An NTP correction can move the
    ///   system clock backwards, and a version derived only from the clock would
    ///   then repeat or regress.
    ///
    /// Why this matters more than it looks: a rewound version is silent at the
    /// producer and total at the consumer. Fusiform keeps issuing versions that
    /// look fine locally, every consumer correctly refuses every one of them
    /// until the counter re-crosses its old high-water, and nothing anywhere
    /// reports a fault. Every component behaves exactly as designed.
    ///
    /// The rewind cannot be prevented at the storage layer, which is why the
    /// value itself has to be immune. The fleet's backup module offers no
    /// restore policy, and a whole-database restore replaces the file — so any
    /// watermark kept inside the database is restored along with the counter it
    /// was meant to protect.
    ///
    /// The guard below still rejects a non-advancing write. It does NOT catch a
    /// restore — after a rewind to 5, writing 6 passes it — which is exactly why
    /// the floor above is the actual protection. It catches a bug in fusiform's
    /// own arithmetic, which is a different failure and worth catching.
    ///
    /// # What this derivation costs a consumer, measured with BROCA 2026-08-13
    ///
    /// The wall-clock floor protects the counter and, in doing so, makes a
    /// restore INVISIBLE at the version. After a restore the version goes UP
    /// while the content goes BACK, because the restored store holds fewer
    /// identities — models arrive daily, 6,310 live against 6,280 at a seed two
    /// days earlier.
    ///
    /// That is the one combination which passes a consumer's version check and
    /// lands in their content check. BROCA's stale guard passes exactly as this
    /// derivation guarantees; their completeness guard then fires, and because
    /// it compares against identities they HOLD and a refused refresh never
    /// installs, the refusal is PERMANENT. Proven by their test, not inferred.
    ///
    /// A POINT-IN-TIME READ IS BYTE-IDENTICAL TO THAT SIGNATURE. `catalog.get`
    /// with `at_ms` returns an older identity set carrying the CURRENT version,
    /// because `catalog_version` describes the store at read time rather than
    /// the snapshot resolved. Correct as documented, and it makes the two
    /// situations indistinguishable to a consumer by construction.
    ///
    /// # The refinement, and why this is not simply a bug
    ///
    /// A counter that shares its artifact's fate is a DEFECT when it must
    /// survive a restore and a FEATURE when it must REFLECT one. Same property,
    /// opposite value, decided by whether the counter protects the data or
    /// describes it. This one was built for the first job and is doing the
    /// second badly — a design used outside its purpose rather than a mistake,
    /// which is why it took two seats to see.
    ///
    /// # If this is ever changed to derive from content, do it in this order
    ///
    /// A content-derived version (the newest era boundary) makes a restore
    /// VISIBLE: the consumer refuses loudly and it SELF-HEALS on the next poll
    /// that writes an era — a jump, not a climb, so the consumer's high-water
    /// does not lengthen it.
    ///
    /// The wait is therefore the gap between era-writing polls. Measured on the
    /// live store over 30 gaps: average 62 minutes, p90 120, max 208, with 9 of
    /// 30 exceeding an hour.
    ///
    /// BROCA degrades after three consecutive refusals, which is 60 minutes.
    /// That threshold is correct ONLY because this derivation makes a refusal
    /// permanent — under a content-derived version, 30% of ordinary quiet
    /// periods would alarm a consumer that is recovering correctly, and an
    /// alarm that fires during its own recovery trains itself away.
    ///
    /// So: measure the worst case, tell BROCA, let them re-pick the threshold,
    /// THEN ship. Not the reverse. The dependency is invisible from inside
    /// either repository.
    /// # The content-derived alternative was examined and REJECTED, 2026-08-14
    ///
    /// The obvious fix for the restore hazard documented above is to derive the
    /// version from content — the highest version the restored rows ever issued,
    /// i.e. the newest era boundary — so a restore makes the version go BACK and
    /// a consumer refuses loudly on staleness instead of silently on
    /// completeness. It is not being built, and the reasoning is here so the
    /// next person to notice the hazard does not re-derive it.
    ///
    /// MEASURED FIRST: on the live store the two derivations produce the SAME
    /// NUMBER, and structurally so — the version advances only inside the
    /// eras-written branch of a changed poll, with `now_ms`, in the same
    /// transaction that writes eras at that instant. So `max(now_ms, current+1)`
    /// reduces to `now_ms` every time and equals the newest era boundary. They
    /// diverge in exactly one case: a restore.
    ///
    /// WHY IT LOSES ANYWAY, from BROCA reading their own guards. Both of their
    /// guards freeze their comparison basis on refusal, but the PRODUCER's half
    /// moves in only one case: a rewound version climbs back with wall clock, a
    /// shrunken identity set does not until the models are re-observed. So a
    /// content-derived version does not replace the permanent refusal — it
    /// inserts a bounded stale phase in FRONT of the same completeness refusal.
    /// They pinned that sequence as a test rather than asserting it.
    ///
    /// And the case it would uniquely fix does not exist: a restore that rewinds
    /// eras WITHOUT shrinking identities is already accepted today, because the
    /// identities are intact and the version is high.
    ///
    /// WHAT I GOT WRONG IN BOTH DIRECTIONS, worth keeping because the corrected
    /// version is the useful one: I offered BROCA "permanent completeness versus
    /// bounded staleness". The completeness refusal is ALSO bounded — the ingest
    /// diff visits every fact in the store as well as every fact in the
    /// document, so a model absent from the document gets a tombstone era rather
    /// than a deleted row, and a restored store's identity set is whole again
    /// after one changed poll, retired models included. The real comparison is
    /// bounded-versus-bounded, and the alternative adds a phase.
    ///
    /// THE CLAIM THAT WOULD REVERSE THIS, stated so it can be falsified: after a
    /// restore plus one changed poll, a `catalog.get` with `include_retired`
    /// returns an identity set no smaller than the pre-restore one. That is
    /// reasoned from the diff's design and has never been driven — no store has
    /// been restored and polled. If it is ever false, the completeness refusal
    /// really is permanent and this decision should be revisited.
    pub fn advance_catalog_version(
        &self,
        observation_id: i64,
        now: Timestamp,
    ) -> Result<i64, CatalogError> {
        let issued = self
            .inner
            .with_conn_fenced(|tx| advance_catalog_version_in(tx, Some(observation_id), now))?;
        self.signal_catalog_changed();
        Ok(issued)
    }

    /// Advance both versions once when a process starts serving.
    ///
    /// Compiled-in data (aliases, curated rows, overrides, planes) changes with
    /// the binary, not with a poll, and readers refetch only when the version
    /// moves. Advancing at every start means a deploy that changes only
    /// compiled data still reaches them, at the cost of one refetch per
    /// restart. Records no observation: nothing was polled.
    pub fn advance_catalog_version_at_start(&self, now: Timestamp) -> Result<i64, CatalogError> {
        let issued = self
            .inner
            .with_conn_fenced(|tx| advance_catalog_version_in(tx, None, now))?;
        self.record_rates_version(issued);
        self.signal_catalog_changed();
        Ok(issued)
    }
}

/// Shared transaction-level advancement keeps grade and poll versions identical.
/// Callers must hold the writer fence and commit the content in this transaction.
pub(crate) fn advance_catalog_version_in(
    tx: &rusqlite::Transaction<'_>,
    observation_id: Option<i64>,
    now: Timestamp,
) -> rusqlite::Result<i64> {
    let current: i64 = tx.query_row(
        "SELECT version FROM catalog_version WHERE id = 1",
        [],
        |r| r.get(0),
    )?;
    let next = now.0.max(current + 1);
    if next <= current {
        // Signalled through the driver's error channel so the whole
        // transaction rolls back; mapped to a domain error below.
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            Some(format!(
                "catalog version must advance: {current} -> {next} does not"
            )),
        ));
    }
    tx.execute(
        "UPDATE catalog_version \
                 SET version = ?1, observation_id = ?2, updated_at_ms = ?3 \
                 WHERE id = 1",
        params![next, observation_id, now.0],
    )?;
    Ok(next)
}

/// Corrections indexed by the fact they cover.
///
/// The key is the full identity a fact is addressed by: provider, model, and
/// fact key. A read resolving thousands of facts looks each one up here, so the
/// shape is a map rather than a list.
pub type CorrectionsByFact = BTreeMap<(String, String, FactKey), Vec<CorrectionRecord>>;

/// Corrections covering an instant, for every model and fact of one source.
///
/// Public so the query-plan test asserts on the SHIPPED query rather than a
/// copy of it. A test holding its own transcription of the SQL proves that the
/// copy is indexed.
pub const CORRECTIONS_COVERING_SQL: &str =
    "SELECT provider_id, model_id, fact_key, boundary_at_ms, \
        corrected_fields_json, affected_from_ms, affected_until_ms, \
        correction_reason \
 FROM era \
 WHERE source = ?1 AND boundary_kind = 'corrected' \
   AND affected_from_ms <= ?2 AND affected_until_ms >= ?2 \
 ORDER BY boundary_at_ms ASC";

/// Corrections covering an instant, for one fact.
pub const CORRECTIONS_FOR_FACT_SQL: &str =
    "SELECT boundary_at_ms, corrected_fields_json, affected_from_ms, \
        affected_until_ms, correction_reason \
 FROM era \
 WHERE source = ?1 AND provider_id = ?2 AND model_id = ?3 \
   AND fact_key = ?4 AND boundary_kind = 'corrected' \
   AND affected_from_ms <= ?5 AND affected_until_ms >= ?5 \
 ORDER BY boundary_at_ms ASC";

/// The answer to "what was true at T".
///
/// Three outcomes rather than an `Option`, because "the store has no record"
/// and "the store's record here is known bad" are different facts and a
/// consumer acts differently on each. Collapsing them into `None` would make a
/// corrected interval look like a gap, which reads as "nothing was published"
/// — a wrong answer that looks like a legitimate one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointInTime {
    /// The store holds a value for this instant and nothing has corrected it.
    Known(EraRow),
    /// The store holds a value, and a LATER era's observation window covers
    /// this instant — so whether the value was still in force is unknown.
    ///
    /// The era answering the read ended somewhere inside
    /// `(superseded_after, superseded_by]`, which brackets `at`. Fusiform did
    /// not look during that interval, so the honest answer is the value plus
    /// the bracket rather than the value alone.
    ///
    /// This is not an exotic state. It occurs whenever a point-in-time read
    /// lands inside any era's window, and a poll gap makes the window wide: a
    /// restore rewinding history produces one spanning the whole lost interval,
    /// and every read inside it answered confidently before this existed.
    ///
    /// Never reached by a read of the CURRENT catalog, which lands on the
    /// newest era and has no later window by construction. It is the historical
    /// audit — "what was the rate on the 9th" — that needs it, which is the
    /// question a wrong answer costs money on.
    KnownStale {
        row: EraRow,
        /// The last instant fusiform confirmed this value before the change.
        superseded_after: Timestamp,
        /// The instant it observed the change.
        superseded_by: Timestamp,
    },
    /// No era covers this instant. The fact was not yet recorded.
    Unknown,
    /// The record covering this instant is known bad, and here is why.
    ///
    /// Carries every correction covering the instant rather than the newest:
    /// two corrections to overlapping intervals are two separate statements
    /// about what went wrong, and picking one would hide the other.
    Corrected { corrections: Vec<CorrectionRecord> },
}

impl PointInTime {
    /// The value, when one can honestly be given.
    ///
    /// `None` for both `Unknown` and `Corrected`. Callers that need to tell
    /// them apart must match; this exists for the ones that only need a value
    /// or nothing, and it is deliberately unable to leak a corrected value.
    /// The era, when the answer is a known-good one.
    ///
    /// `None` for both `Unknown` and `Corrected`, so a corrected value cannot
    /// reach a caller that did not explicitly match for it.
    pub fn known(&self) -> Option<&EraRow> {
        match self {
            PointInTime::Known(row) | PointInTime::KnownStale { row, .. } => Some(row),
            PointInTime::Unknown | PointInTime::Corrected { .. } => None,
        }
    }

    pub fn value_json(&self) -> Option<&str> {
        match self {
            PointInTime::Known(row) | PointInTime::KnownStale { row, .. } => Some(&row.value_json),
            PointInTime::Unknown | PointInTime::Corrected { .. } => None,
        }
    }
}

/// A correction, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectionRecord {
    /// When fusiform recorded the correction — always after the bad interval.
    pub corrected_at: Timestamp,
    /// The `FieldId` list, as stored JSON.
    pub fields_json: String,
    pub affected_from: Timestamp,
    pub affected_until: Timestamp,
    pub reason: String,
}

/// An era as stored, decoded back into the domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EraRow {
    pub id: i64,
    pub value_json: String,
    pub boundary_at: Timestamp,
    pub boundary_kind: BoundaryKind,
    pub prior_observation_at: Option<Timestamp>,
    pub observation_id: Option<i64>,
}

impl EraRow {
    /// The interval within which this era's value became true.
    ///
    /// `None` when the boundary kind carries no window — a seed, an asserted
    /// effective date, or a correction. The absence is the honest answer: those
    /// boundaries are not observations and inventing a window for them would
    /// manufacture precision.
    pub fn observation_window(&self) -> Option<(Timestamp, Timestamp)> {
        self.prior_observation_at
            .map(|prior| (prior, self.boundary_at))
    }
}

struct RawEraRow {
    id: i64,
    value_json: String,
    boundary_at_ms: i64,
    boundary_kind: String,
    prior_observation_at_ms: Option<i64>,
    observation_id: Option<i64>,
    corrected_fields_json: Option<String>,
    affected_from_ms: Option<i64>,
    affected_until_ms: Option<i64>,
    correction_reason: Option<String>,
}

fn decode_era_row(raw: RawEraRow) -> Result<EraRow, CatalogError> {
    let kind = match raw.boundary_kind.as_str() {
        "observed" => BoundaryKind::Observed,
        "asserted" => BoundaryKind::Asserted,
        "seed" => BoundaryKind::Seed,
        "corrected" => {
            // Every extent column is required by the schema for this kind, so a
            // missing one means the row was written by something that bypassed
            // the constraint. Loud, because a correction without its extent
            // silently loses the ability to partition what it invalidated.
            let fields_json = raw.corrected_fields_json.ok_or_else(|| {
                CatalogError::Invariant("corrected era has no field list".to_string())
            })?;
            let fields: Vec<FieldId> =
                serde_json::from_str(&fields_json).map_err(|_| CatalogError::Decode {
                    column: "corrected_fields_json",
                    value: fields_json.clone(),
                })?;
            BoundaryKind::Corrected(Correction {
                fields,
                affected_from: Timestamp(raw.affected_from_ms.ok_or_else(|| {
                    CatalogError::Invariant("corrected era has no affected_from".to_string())
                })?),
                affected_until: Timestamp(raw.affected_until_ms.ok_or_else(|| {
                    CatalogError::Invariant("corrected era has no affected_until".to_string())
                })?),
                reason: raw.correction_reason.ok_or_else(|| {
                    CatalogError::Invariant("corrected era has no reason".to_string())
                })?,
            })
        }
        other => {
            return Err(CatalogError::Decode {
                column: "boundary_kind",
                value: other.to_string(),
            })
        }
    };

    Ok(EraRow {
        id: raw.id,
        value_json: raw.value_json,
        boundary_at: Timestamp(raw.boundary_at_ms),
        boundary_kind: kind,
        prior_observation_at: raw.prior_observation_at_ms.map(Timestamp),
        observation_id: raw.observation_id,
    })
}

struct CorrectionColumns {
    fields_json: String,
    affected_from: i64,
    affected_until: i64,
    reason: String,
}

fn boundary_columns(kind: &BoundaryKind) -> (&'static str, Option<CorrectionColumns>) {
    match kind {
        BoundaryKind::Observed => ("observed", None),
        BoundaryKind::Asserted => ("asserted", None),
        BoundaryKind::Seed => ("seed", None),
        BoundaryKind::Corrected(c) => (
            "corrected",
            Some(CorrectionColumns {
                fields_json: serde_json::to_string(&c.fields)
                    .expect("FieldId is a plain enum and always serializes"),
                affected_from: c.affected_from.0,
                affected_until: c.affected_until.0,
                reason: c.reason.clone(),
            }),
        ),
    }
}

fn last_confirming_before_in(
    conn: &rusqlite::Connection,
    source: SourceId,
    instant: Timestamp,
) -> rusqlite::Result<Option<Timestamp>> {
    let sql = format!(
        "SELECT observed_at_ms FROM observation \
         WHERE source = ?1 AND observed_at_ms < ?2 AND outcome IN ({}) \
         ORDER BY observed_at_ms DESC LIMIT 1",
        confirming_in_list()
    );
    conn.query_row(&sql, params![source.as_str(), instant.0], |r| r.get(0))
        .optional()
        .map(|ts| ts.map(Timestamp))
}

/// Render from the domain's closed set of confirming outcomes so the query
/// cannot drift from it. Every interpolated value is a compile-time constant.
fn confirming_in_list() -> String {
    fusiform_core::CONFIRMING_OUTCOMES
        .iter()
        .map(|o| format!("'{o}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The class a stored `failure_class` string names.
///
/// Beside its encoder deliberately. The two directions of one mapping are a
/// single belief, and splitting them across files is how a stored word and a
/// read word drift apart with nothing able to notice — the store would keep
/// writing "http_status" while a reader looking for "http" reported no class
/// and an operator concluded the failure had none.
///
/// Returns `None` for an unrecognised word rather than guessing: a wrong cause
/// sends an operator somewhere specific and wrong, which is worse than sending
/// them nowhere.
pub fn failure_class_word(class: FailureClass) -> &'static str {
    match class {
        FailureClass::Network => "network",
        FailureClass::HttpStatus => "http_status",
        FailureClass::Parse => "parse",
        FailureClass::Implausible => "implausible",
    }
}

pub fn failure_class_from_stored(word: &str) -> Option<FailureClass> {
    match word {
        "network" => Some(FailureClass::Network),
        "http_status" => Some(FailureClass::HttpStatus),
        "parse" => Some(FailureClass::Parse),
        "implausible" => Some(FailureClass::Implausible),
        _ => None,
    }
}

fn outcome_columns(outcome: &ObservationOutcome) -> (&'static str, Option<&'static str>) {
    // The outcome word comes from the domain's own spelling, so a new variant
    // cannot be stored under a name the queries do not know. Only the failure
    // class is decided here, because only a failure has one.
    let failure_class = match outcome {
        // Through the exported mapping rather than a local match, so the word
        // this writes and the word `failure_class_from_stored` reads are the
        // same function's output. Two matches would be one belief written
        // twice, with nothing able to notice them diverging.
        ObservationOutcome::Failed { class } => Some(failure_class_word(*class)),
        _ => None,
    };
    (outcome.wire_str(), failure_class)
}

/// One curated subscription price, as the store records it.
///
/// A separate type from the module's loaded row rather than a re-export: the
/// store's concern is what gets written, and the loader's concern includes
/// where the row came from in the file. Keeping them apart means a field added
/// for the file's benefit does not silently become a stored claim — the exact
/// mistake that put `unit_provenance` into stored rates and wrote 17,455
/// phantom eras.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPlanPrice {
    pub provider_id: String,
    pub tier: String,
    /// `None` with a `refusal_reason` is a positive claim: the tier was
    /// observed and the source publishes no price for it.
    pub minor_units: Option<i64>,
    pub exponent: Option<i32>,
    pub currency: Option<String>,
    /// Part of the money group: a refusal has no price and therefore no
    /// period. Minting one would be a value invented to fill a column.
    pub period: Option<String>,
    /// The vendor's effective date. Deliberately not paired with a "recorded
    /// at" column — see the schema.
    pub boundary_at_ms: i64,
    pub established_by: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
    pub source_ref: String,
    pub refusal_reason: Option<String>,
}

/// The claim a plan-price row currently makes: price, period, refusal.
///
/// Named rather than an inline tuple because it is the thing the ingest
/// COMPARES, and its membership is the diff's definition of "changed". Reading
/// the bare tuple at the comparison site says nothing about why those five
/// fields and not the provenance ones beside them.
type HeldPlanPrice = (
    Option<i64>,
    Option<i32>,
    Option<String>,
    Option<String>,
    Option<String>,
);

impl CatalogStore {
    /// Append curated plan prices, skipping rows already in force unchanged.
    ///
    /// # Why this diffs rather than inserting
    ///
    /// The curated file is compiled in and re-applied on every startup, so a
    /// blind insert would write the whole file again on each boot — the table
    /// would grow without bound and every row's history would read as a
    /// reprice on each restart. That is the phantom-era failure from August
    /// wearing different clothes: a write path that records fusiform's own
    /// repetition as though it were the world changing.
    ///
    /// The comparison is on the CLAIM — price, period, refusal — and not on
    /// provenance. Re-reading a page and confirming the same number moves
    /// `established_at_ms` and `review_by_ms` without the price having changed,
    /// and recording that as a new era would make a confirmation
    /// indistinguishable from a reprice. A review is not a change.
    ///
    /// That leaves a real gap, stated rather than hidden: a confirmed-unchanged
    /// price does not refresh its review date in the STORE. The file carries
    /// the new date, the gate reads the file, and the store keeps the era that
    /// says what the price was. Which is correct — the store's subject is the
    /// price's history, and the review date is a fact about the file.
    ///
    /// # A key the file stops carrying is TOMBSTONED, not left alone
    ///
    /// This diff visits the FILE'S rows. Left there, a key present in the store
    /// and absent from the file would never be visited, so its last row would
    /// stay in force — and the review gate reads the FILE, so nothing would
    /// check its date either.
    ///
    /// That is the worst failure this plane has. Its premise is that curated
    /// data has no fetch, no diff and no possible contradiction, so a review
    /// date is the ONLY liveness signal it can carry. A row that escapes the
    /// gate is a price that can be wrong forever with nothing able to notice —
    /// and deleting a cell is the obvious way to retire a tier.
    ///
    /// Same fix as the catalog's, which tombstones a model the upstream stops
    /// publishing rather than letting the old facts stand. Solved there in
    /// August and not applied here until QTA generalised it: IN AN APPEND-ONLY
    /// STORE, DECLINING TO WRITE LEAVES THE OLD CLAIM SERVING RATHER THAN
    /// LEAVING SILENCE.
    ///
    /// A tombstone is the one row here that cannot go stale, because its claim
    /// is about fusiform's own curation rather than about a price.
    pub fn append_plan_prices(&self, rows: &[NewPlanPrice]) -> Result<usize, CatalogError> {
        if rows.is_empty() {
            return Ok(0);
        }

        // The file's vintage, used for the tombstones below. The file states
        // these prices as effective at this instant, so the ABSENCE of a key is
        // a statement as of the same instant — and taking it from the rows
        // rather than the clock keeps re-application a no-op.
        let vintage = rows.iter().map(|r| r.boundary_at_ms).max().unwrap_or(0);
        let review = rows.iter().map(|r| r.review_by_ms).max().unwrap_or(0);
        let curated: std::collections::BTreeSet<(&str, &str)> = rows
            .iter()
            .map(|r| (r.provider_id.as_str(), r.tier.as_str()))
            .collect();

        let written = self.inner.with_conn_fenced(|tx| {
            let mut written = 0usize;
            for row in rows {
                // The row in force for this key: newest id at or before the
                // boundary. Matches the read path, so a row this diff considers
                // unchanged is the same one a reader would be served.
                let held: Option<HeldPlanPrice> = tx
                    .query_row(
                        "SELECT minor_units, exponent, currency, period, refusal_reason \
                         FROM plan_price_era \
                         WHERE provider_id = ?1 AND tier = ?2 AND boundary_at_ms <= ?3 \
                         ORDER BY id DESC LIMIT 1",
                        params![row.provider_id, row.tier, row.boundary_at_ms],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                    )
                    .optional()?;

                if let Some((units, exponent, currency, period, refusal)) = held {
                    let unchanged = units == row.minor_units
                        && exponent == row.exponent
                        && currency == row.currency
                        && period == row.period
                        && refusal == row.refusal_reason;
                    if unchanged {
                        continue;
                    }
                }

                tx.execute(
                    "INSERT \
                     INTO plan_price_era \
                     (provider_id, tier, minor_units, exponent, currency, period, \
                      boundary_kind, boundary_at_ms, established_by, \
                      established_at_ms, review_by_ms, source_ref, refusal_reason) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'asserted', ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        row.provider_id,
                        row.tier,
                        row.minor_units,
                        row.exponent,
                        row.currency,
                        row.period,
                        row.boundary_at_ms,
                        row.established_by,
                        row.established_at_ms,
                        row.review_by_ms,
                        row.source_ref,
                        row.refusal_reason,
                    ],
                )?;
                written += 1;
            }

            // Every key the store holds, so one absent from the file can be
            // named. Read inside the fenced transaction, or a key written by a
            // concurrent apply could be tombstoned on the strength of a stale
            // read.
            let mut stmt = tx.prepare("SELECT DISTINCT provider_id, tier FROM plan_price_era")?;
            let held: Vec<(String, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);

            for (provider_id, tier) in held {
                if curated.contains(&(provider_id.as_str(), tier.as_str())) {
                    continue;
                }

                // Already tombstoned? Then this is an ordinary boot and there
                // is nothing to say. Without this the table would gain a
                // tombstone on every restart — the phantom-era failure again,
                // in the code that exists to prevent the stale-row one.
                let current: Option<Option<String>> = tx
                    .query_row(
                        "SELECT refusal_reason FROM plan_price_era \
                         WHERE provider_id = ?1 AND tier = ?2 \
                         ORDER BY id DESC LIMIT 1",
                        params![provider_id, tier],
                        |r| r.get(0),
                    )
                    .optional()?;
                if matches!(&current, Some(Some(reason)) if reason == UNCURATED) {
                    continue;
                }

                tx.execute(
                    "INSERT \
                     INTO plan_price_era \
                     (provider_id, tier, boundary_kind, boundary_at_ms, \
                      established_by, established_at_ms, review_by_ms, \
                      source_ref, refusal_reason) \
                     VALUES (?1, ?2, 'asserted', ?3, 'fusiform', ?3, ?4, ?5, ?6)",
                    params![provider_id, tier, vintage, review, CURATED_FILE, UNCURATED,],
                )?;
                written += 1;
            }

            Ok(written)
        })?;

        Ok(written)
    }
}

/// The reason a tombstone carries.
///
/// It is read on a current answer with no price history beside it, so it must
/// not point at a price "above": in a listing, the row above is a different
/// tier. The already-tombstoned check matches this exact text, so a constant
/// keeps the writer and the check from drifting apart. Rewording it writes one
/// new tombstone per retired key on the next boot, carrying the new wording,
/// and after that re-application is a no-op again.
pub const UNCURATED: &str = "no longer carried by fusiform's curated plan-price file, so fusiform \
     serves no current price for this tier";

/// What a tombstone cites, since there is no vendor page behind an absence.
///
/// The file itself is the source: the claim is "fusiform stopped carrying
/// this", which the file's own contents establish.
pub const CURATED_FILE: &str = "crates/fusiform-module/data/plan-prices.json";

impl CatalogStore {
    /// The plan price in force for every `(provider, tier)` at `at_ms`.
    ///
    /// Supersession is by insertion order, matching the ingest's diff: the row
    /// in force is the newest id among those at or before the instant. A
    /// correction shares its predecessor's boundary, so ordering by boundary
    /// alone would be free to serve either one.
    ///
    /// The returned price boundary is the first eligible era in the unbroken
    /// run of equal claims. Metadata-only revisions can change which row is
    /// eligible without changing when the price claim took effect.
    pub fn plan_prices_at(
        &self,
        at_ms: i64,
        provider_id: Option<&str>,
    ) -> Result<Vec<NewPlanPrice>, CatalogError> {
        let rows = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT provider_id, tier, minor_units, exponent, currency, period, \
                        (SELECT origin.boundary_at_ms FROM plan_price_era origin \
                         WHERE origin.provider_id = p.provider_id AND origin.tier = p.tier \
                           AND origin.id <= p.id AND origin.boundary_at_ms <= ?1 \
                           AND origin.id > COALESCE(( \
                               SELECT MAX(previous.id) FROM plan_price_era previous \
                               WHERE previous.provider_id = p.provider_id AND previous.tier = p.tier \
                                 AND previous.id < p.id AND previous.boundary_at_ms <= ?1 \
                                 AND NOT (previous.minor_units IS p.minor_units \
                                     AND previous.exponent IS p.exponent \
                                     AND previous.currency IS p.currency AND previous.period IS p.period \
                                     AND previous.refusal_reason IS p.refusal_reason) \
                           ), 0) \
                         ORDER BY origin.id LIMIT 1), established_by, established_at_ms, \
                        review_by_ms, source_ref, refusal_reason \
                 FROM plan_price_era p \
                 WHERE boundary_at_ms <= ?1 \
                   AND (?2 IS NULL OR provider_id = ?2) \
                   AND id = ( \
                       SELECT id FROM plan_price_era q \
                       WHERE q.provider_id = p.provider_id AND q.tier = p.tier \
                         AND q.boundary_at_ms <= ?1 \
                       ORDER BY q.id DESC LIMIT 1 \
                   ) \
                 ORDER BY provider_id, tier",
            )?;
            let rows = stmt
                .query_map(params![at_ms, provider_id], |r| {
                    Ok(NewPlanPrice {
                        provider_id: r.get(0)?,
                        tier: r.get(1)?,
                        minor_units: r.get(2)?,
                        exponent: r.get(3)?,
                        currency: r.get(4)?,
                        period: r.get(5)?,
                        boundary_at_ms: r.get(6)?,
                        established_by: r.get(7)?,
                        established_at_ms: r.get(8)?,
                        review_by_ms: r.get(9)?,
                        source_ref: r.get(10)?,
                        refusal_reason: r.get(11)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;

        Ok(rows)
    }
}
