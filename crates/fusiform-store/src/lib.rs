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

pub mod correct;
pub mod ingest;
pub mod schema;
pub mod serve;

use std::collections::BTreeMap;

use cortexkit_store::{open_sqlite, SqliteStore, StorageDescriptor, StoreError};
use fusiform_core::{
    BoundaryKind, Correction, FailureClass, FieldId, ObservationOutcome, SourceId, Timestamp,
};
use rusqlite::{params, OptionalExtension};

/// A lease-guarded, migrated catalog store.
pub struct CatalogStore {
    inner: SqliteStore,
}

/// What went wrong, at the domain's level rather than the driver's.
#[derive(Debug)]
pub enum CatalogError {
    Store(StoreError),
    /// A write was rejected because a newer writer owns the database.
    ///
    /// Distinct from a generic backend error because it has a specific correct
    /// response: this process is superseded and must stop writing, not retry.
    Fenced,
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

/// What an era is about, in the served contract's vocabulary.
///
/// A newtype rather than a bare string so a caller cannot invent a key at a
/// call site: the vocabulary is closed at the type level even though the column
/// is text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FactKey(String);

impl FactKey {
    /// The fact a [`FieldId`] names.
    ///
    /// A correction declares WHICH fields were wrong and is written on a
    /// specific fact's row. Those are two statements of one belief, and without
    /// a mapping between them nothing stops a row that corrects `rate.input`
    /// from declaring an extent about limits — incoherent, and invisible,
    /// because each half is individually well formed.
    ///
    /// Exhaustive by construction: adding a `FieldId` variant without a fact
    /// key here fails to compile.
    pub fn for_field(field: fusiform_core::FieldId) -> Option<Self> {
        use fusiform_core::{CapabilityId, FieldId, LimitId};
        Some(match field {
            FieldId::Rate { class } => Self::rate(class),
            FieldId::Existence => Self::existence(),
            FieldId::Limit { limit } => Self::limit(match limit {
                LimitId::Context => "context",
                LimitId::Output => "output",
            }),
            FieldId::Capability { capability } => Self::capability(match capability {
                CapabilityId::Reasoning => "reasoning",
                CapabilityId::ToolCall => "tool_call",
                CapabilityId::Attachment => "attachment",
                CapabilityId::InputModalities => "input_modalities",
                CapabilityId::OutputModalities => "output_modalities",
            }),
            // A tiered rate's fact key carries a threshold that the FieldId
            // does not know, so these two cannot be mapped to a single key.
            // `None` rather than a guess: the alternative is returning the
            // untiered key, which would make a correction about a tier read as
            // a correction about the base rate.
            FieldId::TierThreshold | FieldId::TierRate | FieldId::ChargeUnit => return None,
        })
    }
}

/// The namespace prefixes fact keys are grouped under.
///
/// Named constants rather than string literals at each construction site,
/// because a consumer selecting a plane does it by prefix — and a prefix that
/// no longer matches any key returns an EMPTY result rather than an error. That
/// is the failure shape this repository keeps finding: a wrong answer that
/// reads as a legitimate one. A caller filtering on `"rate."` after the
/// namespace moved gets zero rates and no indication anything is wrong.
pub mod prefix {
    /// Pricing facts, including tiered rates like `rate.input.above_context.200000`.
    pub const RATE: &str = "rate.";
    /// Declared capabilities.
    pub const CAPABILITY: &str = "capability.";
    /// Declared limits.
    pub const LIMIT: &str = "limit.";
}

impl FactKey {
    /// The key for a rate, per token class — never one key for "rates".
    pub fn rate(class: fusiform_core::TokenClass) -> Self {
        Self(format!("{}{}", prefix::RATE, token_class_str(class)))
    }

    /// The key for a rate that applies above a context threshold.
    ///
    /// The threshold is part of the KEY rather than part of the value, so a
    /// provider adding a tier opens an era on the new tier alone instead of
    /// rewriting the base rate's history. It also means a point-in-time read
    /// asks for exactly the rate it needs — no consumer has to fetch every rate
    /// for a model and filter.
    pub fn rate_above_context(class: fusiform_core::TokenClass, tokens: u64) -> Self {
        Self(format!(
            "{}{}.above_context.{tokens}",
            prefix::RATE,
            token_class_str(class)
        ))
    }

    /// Rebuild a key from its stored text.
    ///
    /// Total by construction: the column is text and this crate is the only
    /// writer, so an unrecognised key means someone edited the database by
    /// hand. Preserving it verbatim keeps that visible instead of turning a
    /// hand-edit into a parse error on an unrelated read.
    pub fn from_stored(raw: String) -> Self {
        Self(raw)
    }

    /// The key for a capability field.
    pub fn capability(name: &str) -> Self {
        Self(format!("{}{name}", prefix::CAPABILITY))
    }

    /// The key for a declared limit.
    pub fn limit(name: &str) -> Self {
        Self(format!("{}{name}", prefix::LIMIT))
    }

    /// The key for whether the model exists in the source at all. A retirement
    /// is a tombstone value under this key, never a deleted row.
    pub fn existence() -> Self {
        Self("existence".to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn token_class_str(class: fusiform_core::TokenClass) -> &'static str {
    use fusiform_core::TokenClass::*;
    match class {
        Input => "input",
        Output => "output",
        CacheRead => "cache_read",
        CacheWrite => "cache_write",
        Reasoning => "reasoning",
    }
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
        let inner = open_sqlite(descriptor)?;
        inner.migrate(schema::NAMESPACE, schema::MIGRATIONS)?;
        Ok(Self { inner })
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
        let (outcome, failure_class) = outcome_columns(&obs.outcome);
        let id = self.inner.with_conn_fenced(|tx| {
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
                    obs.duration_ms,
                ],
            )?;
            Ok(tx.last_insert_rowid())
        })?;
        Ok(id)
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
        let n = self.inner.with_conn(|conn| {
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
        let ts = self.inner.with_conn(|conn| {
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
        let sql = format!(
            "SELECT observed_at_ms FROM observation \
             WHERE source = ?1 \
               AND observed_at_ms < ?2 \
               AND outcome IN ({}) \
             ORDER BY observed_at_ms DESC LIMIT 1",
            confirming_in_list()
        );
        let ts = self.inner.with_conn(|conn| {
            conn.query_row(&sql, params![source.as_str(), instant.0], |r| {
                r.get::<_, i64>(0)
            })
            .optional()
        })?;
        Ok(ts.map(Timestamp))
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

        // Resolve the window's near edge per (source, boundary instant), before
        // the write transaction. Keyed on the boundary as well as the source
        // because the edge is "the last confirming observation before THIS
        // boundary" — a batch carrying eras at two different boundaries has two
        // different near edges, and sharing one would attach the wrong window to
        // whichever era did not own it.
        let mut prior_by_key: BTreeMap<(SourceId, i64), Option<Timestamp>> = BTreeMap::new();
        for era in eras {
            let key = (era.source, era.boundary_at.0);
            if let std::collections::btree_map::Entry::Vacant(slot) = prior_by_key.entry(key) {
                slot.insert(self.last_confirming_observation_before(era.source, era.boundary_at)?);
            }
        }

        // An observed boundary with no prior confirming observation cannot state
        // a window, and the schema will reject it. Failing here names the era
        // rather than surfacing a constraint violation from the driver.
        for era in eras {
            if matches!(era.boundary_kind, BoundaryKind::Observed)
                && prior_by_key
                    .get(&(era.source, era.boundary_at.0))
                    .copied()
                    .flatten()
                    .is_none()
            {
                return Err(CatalogError::Invariant(format!(
                    "{}/{} {}: an observed boundary needs a prior confirming observation; \
                     the first era for a source is a seed",
                    era.provider_id,
                    era.model_id,
                    era.fact_key.as_str()
                )));
            }
        }

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
            let mut count = 0usize;
            for era in eras {
                let prior = prior_by_key
                    .get(&(era.source, era.boundary_at.0))
                    .copied()
                    .flatten();
                let (kind, correction) = boundary_columns(&era.boundary_kind);
                // Only an observed boundary carries a window. Every other kind
                // writes NULL, which the schema enforces independently.
                let prior_ms = match era.boundary_kind {
                    BoundaryKind::Observed => prior.map(|t| t.0),
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
            Ok(count)
        })?;

        Ok(written)
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
        let row = self.inner.with_conn(|conn| {
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

    /// Run a read against the connection.
    ///
    /// Reads only. Writes go through `with_conn_fenced`, which checks that this
    /// process still holds the current writer lease before applying anything:
    /// when one instance replaces another, the outgoing one can briefly still
    /// have an open connection after losing the lease, and that check is what
    /// stops its late writes from landing on top of its replacement's.
    pub(crate) fn raw_conn<T>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T>,
    ) -> Result<T, CatalogError> {
        Ok(self.inner.with_conn(f)?)
    }

    /// The ETag from the most recent observation that received one.
    ///
    /// Read from the store on every tick rather than held in memory: an
    /// in-process cache is empty after a restart, so the first poll of every
    /// new process would be unconditional and pull the whole document when a
    /// 304 would have done.
    pub fn last_etag(&self, source: SourceId) -> Result<Option<String>, CatalogError> {
        let etag = self.inner.with_conn(|conn| {
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
        let hash = self.inner.with_conn(|conn| {
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
    pub fn raw_hash_of_observation(&self, id: i64) -> Result<Option<String>, CatalogError> {
        let hash = self.inner.with_conn(|conn| {
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
        let class = self.inner.with_conn(|conn| {
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
        let v = self.inner.with_conn(|conn| {
            conn.query_row(
                "SELECT version FROM catalog_version WHERE id = 1",
                [],
                |r| r.get::<_, i64>(0),
            )
        })?;
        Ok(v)
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
        let issued = self.inner.with_conn_fenced(|tx| {
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
        })?;
        Ok(issued)
    }
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

/// The confirming outcomes as a SQL literal list.
///
/// Rendered from the domain's `CONFIRMING_OUTCOMES` rather than written out, so
/// adding an outcome cannot leave the query behind. Safe to interpolate: every
/// element is a compile-time constant from a closed enum, never user input.
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
