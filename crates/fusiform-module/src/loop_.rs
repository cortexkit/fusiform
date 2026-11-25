//! The poll loop: one cadence tick, start to finish.
//!
//! Every tick does the same five things — fetch, normalize, diff, record,
//! advance — and the ORDER of the last two is the only interesting decision in
//! this file.
//!
//! A changed poll commits its observation, eras and catalog version together.
//! Otherwise a retry could trust the new digest or validator even though the
//! facts it identifies never reached the store.
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

use fusiform_core::normalize::normalize_models_dev_with;
use fusiform_core::{FailureClass, ObservationOutcome, SourceId, Timestamp};
use fusiform_store::ingest::catalog_digest_with_conflicts;
use fusiform_store::{CatalogError, CatalogStore, NewObservation};

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

    let recorded_before = ctx.signals.polls_recorded();
    let result = fetch_and_apply(ctx, now_ms).await;
    // The single poll loop owns both stamps. An error after persisting the
    // observation must not count as a missed observation.
    if result.is_err() && ctx.signals.polls_recorded() == recorded_before {
        ctx.signals.poll_unpersisted();
    }
    result
}

async fn fetch_and_apply(ctx: &PollContext, now_ms: i64) -> Result<TickReport, TickError> {
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
    let document_read = ctx.document_read.load(Ordering::Acquire);
    let etag = if document_read {
        ctx.store
            .last_etag(ctx.endpoint.source)
            .map_err(TickError::Store)?
    } else {
        None
    };

    let outcome = ctx.fetcher.poll(&ctx.endpoint, etag.as_deref()).await;
    let store = ctx.store.clone();
    let signals = ctx.signals.clone();
    let source = ctx.endpoint.source;
    let report = tokio::task::spawn_blocking(move || {
        apply_with_count_reader(
            &store,
            &signals,
            source,
            outcome,
            now_ms,
            document_read,
            |store, source| store.catalog_counts(source, None),
        )
    })
    .await
    .map_err(TickError::ApplyTask)??;
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
/// written. Without a process context, validate full bodies against the eras
/// rather than assuming a persisted digest describes them.
pub fn apply(
    store: &CatalogStore,
    signals: &Signals,
    source: SourceId,
    outcome: FetchOutcome,
    now_ms: i64,
) -> Result<TickReport, TickError> {
    apply_with_count_reader(
        store,
        signals,
        source,
        outcome,
        now_ms,
        false,
        |store, source| store.catalog_counts(source, None),
    )
}

fn apply_with_count_reader(
    store: &CatalogStore,
    signals: &Signals,
    source: SourceId,
    outcome: FetchOutcome,
    now_ms: i64,
    trust_saved_hash: bool,
    mut read_counts: impl FnMut(&CatalogStore, SourceId) -> Result<(usize, usize), CatalogError>,
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
            // Pass separately sourced input/output audio evidence so only
            // documented unconditional token prices become audio rate facts.
            let outcome_parse = normalize_models_dev_with(&bytes, &crate::input_audio::evidence());

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

            let (digest, conflicts) = catalog_digest_with_conflicts(&catalog);
            for conflict in conflicts {
                eprintln!(
                    "fusiform: omitted conflicting upstream fact {:?}/{:?} {:?}: {:?}",
                    conflict.provider,
                    conflict.model,
                    conflict.fact.as_str(),
                    conflict.values
                );
            }
            let previous = ctx
                .store
                .last_normalized_hash(ctx.source)
                .map_err(TickError::Store)?;

            // A 200 whose facts match what we already hold. The bytes may
            // differ — a reformat, a reordered key — and none of that is a
            // change to anything fusiform serves.
            let matching_hash = previous.as_deref() == Some(digest.as_str());
            // The first full body also reconciles stores left by an older
            // writer: even an `unchanged` row could repeat an unapplied digest.
            if trust_saved_hash && matching_hash {
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

            let (observation_id, plan, next_version) = match ctx.store.commit_polled_document(
                &NewObservation {
                    source: ctx.source,
                    observed_at: Timestamp(now_ms),
                    outcome: ObservationOutcome::Changed { snapshot_seq: 0 },
                    normalized_hash: Some(digest),
                    raw_hash: Some(raw_hash.clone()),
                    etag,
                    duration_ms: Some(duration.as_millis() as i64),
                    detail: None,
                },
                &catalog,
            ) {
                Ok(written) => written,
                Err(error @ fusiform_store::CatalogError::PollBoundaryNotIncreasing { .. }) => {
                    return record_failure(
                        ctx,
                        FailureClass::Implausible,
                        error.to_string(),
                        now_ms,
                        duration,
                        Some(raw_hash),
                    );
                }
                Err(error @ fusiform_store::CatalogError::PollOvertakenByCorrection { .. }) => {
                    return record_failure(
                        ctx,
                        FailureClass::Transient,
                        error.to_string(),
                        now_ms,
                        duration,
                        Some(raw_hash),
                    );
                }
                Err(error) => return Err(TickError::Store(error)),
            };
            ctx.signals.observed(now_ms);

            let Some(next_version) = next_version else {
                return Ok(TickReport {
                    outcome: TickOutcome::Unchanged,
                    observation_id,
                    eras_written: 0,
                    composition: EraComposition::default(),
                });
            };
            // This count only refreshes health telemetry; its failure must not
            // turn an already-appended ingest into a failed poll.
            match read_counts(ctx.store, ctx.source) {
                Ok((models, priced)) => ctx.signals.set_model_counts(models, priced),
                Err(e) => {
                    ctx.signals.mark_model_counts_unknown();
                    eprintln!(
                        "fusiform: could not refresh cached model counts after a changed poll; \
                         health will report null until a later catalog count succeeds: {e}"
                    );
                }
            }
            ctx.signals.wrote(now_ms);

            Ok(TickReport {
                outcome: TickOutcome::Changed {
                    new_version: next_version,
                },
                observation_id,
                eras_written: plan.eras.len(),
                composition: EraComposition {
                    models_arrived: plan.new_models,
                    models_withdrawn: plan.disappeared_models,
                    facts_changed: plan.changed_facts,
                },
            })
        }
    }
}

pub use fusiform_core::shrink_guard::{MAX_SHRINK_FRACTION, SHRINK_GUARD_MIN_MODELS};

fn implausible_shrink(
    ctx: &ApplyCtx<'_>,
    catalog: &fusiform_core::NormalizedCatalog,
) -> Result<Option<String>, fusiform_store::CatalogError> {
    let held = ctx.store.present_model_count(ctx.source)?;
    Ok(fusiform_core::shrink_guard::implausible_shrink(
        held,
        catalog.model_count(),
    ))
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

    let observation = NewObservation {
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
    };
    let id = ctx
        .store
        .record_observation(&observation)
        .map_err(|store| TickError::FailureNotRecorded {
            class,
            detail: observation.detail.unwrap_or_default(),
            store,
        })?;
    ctx.signals.poll_recorded();
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
    ApplyTask(tokio::task::JoinError),
    FailureNotRecorded {
        class: FailureClass,
        detail: String,
        store: CatalogError,
    },
}

impl std::fmt::Display for TickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TickError::Store(e) => write!(f, "store: {e}"),
            TickError::ApplyTask(e) => write!(f, "poll apply task did not complete: {e}"),
            TickError::FailureNotRecorded {
                class,
                detail,
                store,
            } => write!(
                f,
                "poll failed ({class:?}): {detail}; store could not record the failure: {store}"
            ),
        }
    }
}

impl std::error::Error for TickError {}

/// The source fusiform polls.
pub fn default_source() -> SourceId {
    SourceId::ModelsDev
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
    use fusiform_core::SourceId;
    use fusiform_store::{CatalogError, CatalogStore};

    use super::{apply_with_count_reader, FetchOutcome, TickOutcome};
    use crate::{health, signals::Signals};

    #[test]
    fn a_failed_count_refresh_does_not_fail_a_changed_ingest() {
        let dir = tempfile::tempdir().unwrap();
        let store = CatalogStore::open(&StorageDescriptor {
            module_id: "fusiform".to_string(),
            storage_namespace: "default".to_string(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.path().join("store.db").to_string_lossy().to_string(),
            },
        })
        .unwrap();
        let signals = Signals::new();
        signals.attempted(1_000);
        signals.set_model_counts(50, 30);

        let report = apply_with_count_reader(
            &store,
            &signals,
            SourceId::ModelsDev,
            FetchOutcome::Body {
                bytes: include_bytes!("../../fusiform-core/fixtures/models-dev-excerpt.json")
                    .to_vec(),
                etag: None,
                duration: Duration::from_millis(10),
            },
            1_000,
            true,
            |store, source| {
                assert!(
                    store.era_count(source).unwrap() > 0,
                    "the injected count read runs after the eras are appended"
                );
                Err(CatalogError::Invariant(
                    "injected catalog count read failure".to_string(),
                ))
            },
        )
        .unwrap();

        assert!(matches!(report.outcome, TickOutcome::Changed { .. }));
        assert!(report.eras_written > 0);
        assert!(store.era_count(SourceId::ModelsDev).unwrap() > 0);

        let metrics = health::report(&signals, 1_000).metrics.unwrap();
        assert_eq!(metrics["process_poll_attempts"], 1);
        assert_eq!(metrics["process_polls_recorded"], 1);
        assert_eq!(metrics["process_polls_unrecorded"], 0);
        assert!(metrics["model_count"].is_null());
        assert!(metrics["models_priced"].is_null());
    }
}
