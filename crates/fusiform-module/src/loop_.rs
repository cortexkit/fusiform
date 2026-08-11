//! The poll loop: one cadence tick, start to finish.
//!
//! Every tick does the same five things — fetch, normalize, diff, record,
//! advance — and the ORDER of the last two is the only interesting decision in
//! this file.
//!
//! # Why the observation is written first
//!
//! A tick writes its observation row before its eras, in that order, and the
//! two are separate transactions. The alternative — one transaction for both —
//! is tempting and wrong: an era's observation window is derived from the
//! store's observation history, so the observation must already be visible
//! when the eras are appended.
//!
//! The cost is a crash window between them, and the shape of that window is
//! what makes it acceptable: an observation with no eras reads as "we looked
//! and nothing had changed", which is a survivable lie that the next tick
//! corrects (it will diff against the same unchanged state and write the eras
//! then). The reverse — eras with no observation — would be unrecoverable,
//! because the window they were computed against would not exist.
//!
//! # Why a failed tick writes anything at all
//!
//! A failed poll observed nothing, so it writes no eras and cannot narrow any
//! window. It still writes an observation row, because the record of what
//! fusiform TRIED is what lets an operator tell "the upstream has not changed
//! in six hours" from "we have not successfully asked in six hours". Those look
//! identical from the era table alone.

use std::sync::Arc;

use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, FailureClass, ObservationOutcome, SourceId, Timestamp};
use fusiform_store::ingest::{catalog_digest, plan_ingest};
use fusiform_store::{CatalogStore, NewObservation};

use crate::fetch::{hash_bytes, FetchOutcome, Fetcher, SourceEndpoint};
use crate::signals::Signals;

/// The fixed poll interval.
///
/// Not configurable, deliberately. The interval is the width of every
/// observation window fusiform records, so it is a property of the data rather
/// than a tuning knob — two deployments with different intervals would produce
/// history of different precision under one schema, and nothing downstream
/// could tell which it was reading.
pub const POLL_INTERVAL_MS: i64 = 30 * 60 * 1_000;

/// What one tick did, for logging and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickReport {
    pub outcome: TickOutcome,
    pub observation_id: i64,
    pub eras_written: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickOutcome {
    /// The catalog moved.
    Changed { new_version: i64 },
    /// A 200 whose facts matched what the store already held.
    Unchanged,
    /// The upstream answered the conditional request with 304.
    NotModified,
    /// Nothing was observed.
    Failed { class: FailureClass },
}

/// Everything a tick needs.
pub struct PollContext {
    pub store: Arc<CatalogStore>,
    pub fetcher: Fetcher,
    pub endpoint: SourceEndpoint,
    pub signals: Arc<Signals>,
}

/// Run one poll cycle: fetch, then apply.
pub async fn tick(ctx: &PollContext, now_ms: i64) -> Result<TickReport, TickError> {
    // The ETag from the last observation that carried one. Read per tick rather
    // than cached in memory: after a restart the in-memory value would be gone
    // and every first poll after a restart would pull 3.6 MB it did not need.
    let etag = ctx
        .store
        .last_etag(ctx.endpoint.source)
        .map_err(TickError::Store)?;

    let outcome = ctx.fetcher.poll(&ctx.endpoint, etag.as_deref()).await;
    apply(
        &ctx.store,
        &ctx.signals,
        ctx.endpoint.source,
        outcome,
        now_ms,
    )
}

/// The pieces `apply` writes through. A borrow-only view, so the apply half
/// cannot accidentally hold the fetcher or the endpoint and grow a dependency
/// on the network it was split away from.
struct ApplyCtx<'a> {
    store: &'a CatalogStore,
    signals: &'a Signals,
    source: SourceId,
}

/// Everything a tick decides, given what the fetch returned.
///
/// Split from [`tick`] so the decisions can be tested against a real store
/// without a network: every branch below turns on what arrived, and a test that
/// has to stand up an HTTP server to reach the parse-failure branch will not be
/// written. The fetch half has no decisions in it — it classifies a response and
/// returns.
pub fn apply(
    store: &CatalogStore,
    signals: &Signals,
    source: SourceId,
    outcome: FetchOutcome,
    now_ms: i64,
) -> Result<TickReport, TickError> {
    let ctx = ApplyCtx {
        store,
        signals,
        source,
    };
    let ctx = &ctx;
    match outcome {
        FetchOutcome::Failed {
            class,
            detail,
            duration,
        } => record_failure(ctx, class, detail, now_ms, duration, None),

        FetchOutcome::NotModified { etag, duration } => {
            // A real observation: the upstream confirmed its content is
            // unchanged. This is a valid window edge and refreshes the age of
            // fusiform's knowledge.
            let id = ctx
                .store
                .record_observation(&NewObservation {
                    source: ctx.source,
                    observed_at: Timestamp(now_ms),
                    outcome: ObservationOutcome::NotModified,
                    normalized_hash: None,
                    raw_hash: None,
                    etag,
                    duration_ms: Some(duration.as_millis() as i64),
                    detail: None,
                })
                .map_err(TickError::Store)?;
            ctx.signals.observed(now_ms);
            Ok(TickReport {
                outcome: TickOutcome::NotModified,
                observation_id: id,
                eras_written: 0,
            })
        }

        FetchOutcome::Body {
            bytes,
            etag,
            duration,
        } => {
            let raw_hash = hash_bytes(&bytes);
            let outcome_parse = normalize_models_dev(&bytes);

            let catalog = match outcome_parse {
                Ok(outcome) => outcome.catalog,
                Err(e) => {
                    // The bytes arrived and could not be read. This is a parse
                    // failure rather than a network one, and the distinction is
                    // load-bearing: a network failure will probably fix itself,
                    // while a parse failure means the upstream published a
                    // shape this version does not understand and will keep
                    // failing until someone looks.
                    //
                    // The raw hash is worth keeping even though nothing was
                    // ingested: it identifies exactly which document failed, so
                    // a later fix can be checked against those bytes rather
                    // than against whatever the upstream is serving by then.
                    return record_failure(
                        ctx,
                        FailureClass::Parse,
                        e.to_string(),
                        now_ms,
                        duration,
                        Some(raw_hash),
                    );
                }
            };

            let digest = catalog_digest(&catalog);
            let previous = ctx
                .store
                .last_normalized_hash(ctx.source)
                .map_err(TickError::Store)?;

            // A 200 whose facts match what we already hold. The bytes may
            // differ — a reformat, a reordered key — and none of that is a
            // change to anything fusiform serves.
            if previous.as_deref() == Some(digest.as_str()) {
                let id = ctx
                    .store
                    .record_observation(&NewObservation {
                        source: ctx.source,
                        observed_at: Timestamp(now_ms),
                        outcome: ObservationOutcome::Unchanged,
                        normalized_hash: Some(digest),
                        raw_hash: Some(raw_hash),
                        etag,
                        duration_ms: Some(duration.as_millis() as i64),
                        detail: None,
                    })
                    .map_err(TickError::Store)?;
                ctx.signals.observed(now_ms);
                return Ok(TickReport {
                    outcome: TickOutcome::Unchanged,
                    observation_id: id,
                    eras_written: 0,
                });
            }

            // Something moved. The observation is written first so the eras
            // appended below can derive their windows from a history that
            // already contains this tick.
            let observation_id = ctx
                .store
                .record_observation(&NewObservation {
                    source: ctx.source,
                    observed_at: Timestamp(now_ms),
                    outcome: ObservationOutcome::Changed { snapshot_seq: 0 },
                    normalized_hash: Some(digest),
                    raw_hash: Some(raw_hash),
                    etag,
                    duration_ms: Some(duration.as_millis() as i64),
                    detail: None,
                })
                .map_err(TickError::Store)?;
            ctx.signals.observed(now_ms);

            // The first ingest into an empty store is a SEED, not an
            // observation: there is no prior observation to bound it against,
            // so claiming an observed boundary would be claiming a window that
            // does not exist.
            let boundary_kind = if ctx
                .store
                .last_confirming_observation_before(ctx.source, Timestamp(now_ms))
                .map_err(TickError::Store)?
                .is_some()
            {
                BoundaryKind::Observed
            } else {
                BoundaryKind::Seed
            };
            let era_observation = match boundary_kind {
                BoundaryKind::Seed => None,
                _ => Some(observation_id),
            };

            let plan = plan_ingest(
                ctx.store,
                &catalog,
                Timestamp(now_ms),
                boundary_kind,
                era_observation,
            )
            .map_err(TickError::Store)?;

            let eras_written = ctx
                .store
                .append_eras(&plan.eras)
                .map_err(TickError::Store)?;
            ctx.signals.wrote(now_ms);

            // The version advances only after the eras are durable. A version
            // published ahead of its content would have consumers refuse the
            // push that carried it, permanently: they would hold a high-water
            // mark for a catalog they never received.
            //
            // The store derives the value rather than taking one from here: it
            // is `max(now_ms, current + 1)` so a restore cannot rewind it, and
            // that derivation belongs next to the counter it protects.
            let next_version = ctx
                .store
                .advance_catalog_version(observation_id, Timestamp(now_ms))
                .map_err(TickError::Store)?;

            Ok(TickReport {
                outcome: TickOutcome::Changed {
                    new_version: next_version,
                },
                observation_id,
                eras_written,
            })
        }
    }
}

/// Record a poll that observed nothing.
///
/// One function for every failure path, so the class stored in the observation
/// row and the class returned to the caller cannot disagree — they are the same
/// value. Two call sites each constructing both independently is the shape that
/// lets a mutation change one and leave the other, with nothing failing.
fn record_failure(
    ctx: &ApplyCtx<'_>,
    class: FailureClass,
    detail: String,
    now_ms: i64,
    duration: std::time::Duration,
    raw_hash: Option<String>,
) -> Result<TickReport, TickError> {
    let id = ctx
        .store
        .record_observation(&NewObservation {
            source: ctx.source,
            observed_at: Timestamp(now_ms),
            outcome: ObservationOutcome::Failed { class },
            normalized_hash: None,
            raw_hash,
            // Never carried forward from a previous observation: this row must
            // not look like it learned an ETag it never received, and the next
            // poll must not send a conditional request claiming to hold a
            // document this one could not read.
            etag: None,
            duration_ms: Some(duration.as_millis() as i64),
            detail: Some(detail),
        })
        .map_err(TickError::Store)?;
    // The same class that went into the observation row, so health and the
    // stored history cannot disagree about what failed.
    ctx.signals.failed(class);
    Ok(TickReport {
        outcome: TickOutcome::Failed { class },
        observation_id: id,
        eras_written: 0,
    })
}

#[derive(Debug)]
pub enum TickError {
    Store(fusiform_store::CatalogError),
}

impl std::fmt::Display for TickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TickError::Store(e) => write!(f, "store: {e}"),
        }
    }
}

impl std::error::Error for TickError {}

/// The source fusiform polls.
pub fn default_source() -> SourceId {
    SourceId::ModelsDev
}
