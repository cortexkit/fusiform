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

use std::sync::atomic::{AtomicBool, Ordering};
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
    /// How the eras divide: models arriving, models leaving, and facts that
    /// moved on a model already known.
    ///
    /// A row count alone is misleading in a specific direction, measured over
    /// 11 hours of live polling: 72% of era churn was models arriving and
    /// leaving rather than facts changing, because an arriving model writes one
    /// era per fact it has. An operator reading "45 eras written" would take
    /// that as 45 facts moving; on one real poll it was 8 facts and 5 arrivals.
    ///
    /// The distinction is not cosmetic. A model arriving is the upstream
    /// publishing something new; a fact changing on a known model is the
    /// upstream revising something, which is the event a consumer's cache and
    /// a ledger care about.
    pub composition: EraComposition,
}

/// How a tick's eras divide by cause.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EraComposition {
    pub models_arrived: usize,
    pub models_withdrawn: usize,
    /// Facts that moved on a model already known — the count an operator
    /// usually means by "what changed".
    pub facts_changed: usize,
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
    /// Whether THIS process has read a full document yet. Until it has, a poll
    /// sends no validator; see `tick`. Private so the only way to set it is a
    /// poll that actually received a body.
    document_read: AtomicBool,
}

impl PollContext {
    /// A context for a process that has not polled yet.
    pub fn new(
        store: Arc<CatalogStore>,
        fetcher: Fetcher,
        endpoint: SourceEndpoint,
        signals: Arc<Signals>,
    ) -> Self {
        Self {
            store,
            fetcher,
            endpoint,
            signals,
            document_read: AtomicBool::new(false),
        }
    }
}

/// Run one poll cycle: fetch, then apply.
pub async fn tick(ctx: &PollContext, now_ms: i64) -> Result<TickReport, TickError> {
    // Stamp the attempt FIRST, and stamp it HERE rather than in the caller.
    //
    // First, because a fetch that hangs for its full 90-second timeout must
    // still show the loop as alive: stamping after would make a slow upstream
    // indistinguishable from a dead loop, which is the exact distinction this
    // signal exists to draw.
    //
    // Here rather than in the caller, because a mutation proved the caller
    // version untestable. The stamp lived in the poll loop in `main.rs`, no
    // test exercises that loop — every test calls `tick` directly — and
    // deleting the call reddened nothing. A signal whose only writer is
    // unreachable from the test suite is a signal that can be silently removed.
    ctx.signals.attempted(now_ms);

    // No validator until this process has read a full document.
    //
    // A 304 says the upstream's BYTES have not changed since the stored ETag.
    // It does not say THIS binary has read them, and after a deploy it has not:
    // a build that extracts a new fact would otherwise get a 304 on every poll
    // until the upstream happened to move, and serve nothing new in the
    // meantime. Measured on 2026-09-25: a build adding tier prices was placed
    // at 20:15:25Z, its first poll sent the previous binary's ETag, received
    // 304, and served no tier price at all. The ingest diff makes the full read
    // safe: an unchanged document writes no era.
    //
    // The cost is one full download (about 4 MB) per process start, and only
    // until the first body arrives: a failed first poll leaves the next one
    // unconditional too.
    let etag = if ctx.document_read.load(Ordering::Acquire) {
        ctx.store
            .last_etag(ctx.endpoint.source)
            .map_err(TickError::Store)?
    } else {
        None
    };

    let outcome = ctx.fetcher.poll(&ctx.endpoint, etag.as_deref()).await;
    let report = apply(
        &ctx.store,
        &ctx.signals,
        ctx.endpoint.source,
        outcome,
        now_ms,
    )?;
    if matches!(
        report.outcome,
        TickOutcome::Changed { .. } | TickOutcome::Unchanged
    ) {
        ctx.document_read.store(true, Ordering::Release);
    }
    Ok(report)
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
                composition: EraComposition::default(),
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
                    composition: EraComposition::default(),
                });
            }

            // A document that describes far less than the last one is refused
            // before anything is written.
            //
            // Fusiform's ingest treats "absent from this document" as a
            // withdrawal, so a valid-but-collapsed response — a truncated CDN
            // cache entry, a provider index caught mid-rebuild, an API
            // returning `{}` on an internal error — tombstones every model it
            // omits. Measured against a real seed: one such response took a
            // 14-model store to 1, and at production scale it would tombstone
            // 6,270 models, report success, and leave every consumer reading an
            // empty catalog.
            //
            // Nothing else catches it. The bytes are well formed, the parse
            // succeeds, the hash differs, and the tombstones are exactly what
            // the diff asks for.
            if let Some(reason) = implausible_shrink(ctx, &catalog).map_err(TickError::Store)? {
                return record_failure(
                    ctx,
                    FailureClass::Implausible,
                    reason,
                    now_ms,
                    duration,
                    Some(raw_hash),
                );
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
                composition: EraComposition {
                    models_arrived: plan.new_models,
                    models_withdrawn: plan.disappeared_models,
                    facts_changed: plan.changed_facts,
                },
            })
        }
    }
}

/// The fraction of the catalog a single poll may drop before fusiform refuses.
///
/// Grounded in two measurements rather than chosen. Across two live fetches six
/// hours apart, ZERO models disappeared out of 6,253 — the real rate is not
/// merely low, it was nil over that interval. And the largest single provider
/// carries 9.9% of the catalog, so a whole provider vanishing at once, the
/// biggest plausible legitimate drop, stays under this.
///
/// A legitimate mass withdrawal beyond that is possible and would be refused.
/// That is the intended direction: it presents as a Degraded module with a
/// named reason and an operator decision, while the alternative presents as a
/// successful poll that emptied the catalog. One is a delay, the other is
/// silent data loss with every component reporting success.
pub const MAX_SHRINK_FRACTION: f64 = 0.25;

/// Below this many models, the fraction is meaningless and any drop is allowed.
///
/// A store holding four models cannot express a 25% threshold usefully, and a
/// small catalog is either a test fixture or a brand-new install — neither is a
/// state worth defending against a shrink.
pub const SHRINK_GUARD_MIN_MODELS: usize = 100;

/// Whether a document drops so much of the catalog that fusiform refuses it.
///
/// Compares against what the store currently holds as PRESENT, not against the
/// last document's size: the store is what a consumer would lose.
fn implausible_shrink(
    ctx: &ApplyCtx<'_>,
    catalog: &fusiform_core::NormalizedCatalog,
) -> Result<Option<String>, fusiform_store::CatalogError> {
    let held = ctx.store.present_model_count(ctx.source)?;
    if held < SHRINK_GUARD_MIN_MODELS {
        return Ok(None);
    }

    let incoming = catalog.model_count();
    if incoming >= held {
        // GROWTH IS DELIBERATELY UNGUARDED, and this line is where a reader
        // would otherwise have to guess whether that was reasoned or missed.
        //
        // The two directions fail differently, and the difference is the same
        // loud-versus-silent one that decides most of this crate's design:
        //
        //   - A spurious SHRINK writes tombstones. A consumer stops offering a
        //     model that really exists, nothing errors, and nobody looks. That
        //     is the failure the guard was built for, from a measured event: a
        //     collapsed response took a seeded store from 14 models to 1.
        //
        //   - A spurious GROWTH writes arrivals. A consumer routes to a model
        //     the provider does not have and gets a 404 on the first request —
        //     immediate, attributable, and self-correcting on the next poll
        //     that omits it.
        //
        // So the direction that needs a guard is the one that cannot announce
        // itself. Refusing growth would trade a loud failure for a refused
        // poll, and a real batch arrival is ordinary: 51 models landed in a
        // single poll on 2026-08-14, and models.dev adds roughly 45 a day.
        //
        // WHAT WOULD CHANGE THIS: growth is not harmless in the store, only at
        // the consumer. A duplicated provider index would append tens of
        // thousands of arrival eras that append-only history can never remove,
        // only tombstone. No such event has been observed, so building for it
        // would be designing against an imagined failure — but if one is ever
        // seen, this is the line that needs the counterpart, and the argument
        // above is what it has to beat.
        //
        // WHAT ORDINARY LOOKS LIKE, measured on the live store 2026-08-15 so a
        // future threshold is picked against data rather than taste. The
        // largest single-poll arrival to date: 211 models against ~6,370 held,
        // which is 3.3% — and 204 of them were one provider (`edenai`) growing
        // from 16 models to 220 in a day. A real provider onboarding its
        // catalogue is therefore a THREE PERCENT jump, so any growth guard has
        // to sit well above that, while the failure it would catch (an index
        // duplicated into itself) is nearer a hundred percent. That gap is
        // wide, which is a further reason the guard is not urgent: there is no
        // ambiguous middle to adjudicate.
        return Ok(None);
    }

    let dropped = held - incoming;
    let fraction = dropped as f64 / held as f64;
    if fraction <= MAX_SHRINK_FRACTION {
        return Ok(None);
    }

    Ok(Some(format!(
        "upstream document describes {incoming} models, {dropped} fewer than the \
         {held} currently held ({:.1}% drop, limit {:.0}%); refusing to tombstone \
         them on one response",
        fraction * 100.0,
        MAX_SHRINK_FRACTION * 100.0
    )))
}

/// Record a poll that observed nothing.
///
/// One function for every failure path, so the class stored in the observation
/// row and the class returned to the caller cannot disagree — they are the same
/// value. Two call sites each constructing both independently is the shape that
/// lets a mutation change one and leave the other, with nothing failing.
///
/// # The in-memory signal is stamped FIRST, and the order is the point
///
/// `signals.failed` is process-local and exists to be readable when the store
/// is not. Stamping it after the write made that false: the `?` on the write
/// returned early, so a store outage suppressed the record of a fetch outage
/// happening at the same time.
///
/// Measured with a probe. Network down and disk broken together, health
/// reported `consecutive_failures: 0` and `last_failure_class: null` for three
/// hours — the fetch failures were invisible, and the one signal designed to
/// survive a store outage did not, because it was written behind the store.
///
/// This is ASTRO's fourth variant, found in their capacity loop and checked for
/// here on their report: **a failure path that depends on the failing subsystem
/// cannot report on it.** The reporter and the reported must be independent,
/// and "process-local" is not independence if the code path reaches it through
/// the thing that failed.
///
/// The two records now say different things when they disagree, which is
/// correct rather than a hazard: the signal says a fetch failed (true, it did),
/// and the missing row says the store could not record it (also true, and
/// reported separately by the attempt ledger in `health`).
fn record_failure(
    ctx: &ApplyCtx<'_>,
    class: FailureClass,
    detail: String,
    now_ms: i64,
    duration: std::time::Duration,
    raw_hash: Option<String>,
) -> Result<TickReport, TickError> {
    ctx.signals.failed(class);

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
    Ok(TickReport {
        outcome: TickOutcome::Failed { class },
        observation_id: id,
        eras_written: 0,
        composition: EraComposition::default(),
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
