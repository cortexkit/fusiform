//! The embedded bootstrap snapshot.
//!
//! The carriage bar from the charter: a fresh install with no network comes up
//! able to answer. Not "comes up and reports healthy while serving nothing" —
//! an empty catalog is a wrong answer that looks like a legitimate one, and a
//! consumer caching it has no way to tell.
//!
//! # What a seed is allowed to claim
//!
//! The snapshot is compiled in, so it describes the upstream as of whenever
//! someone last ran `scripts/refresh-seed.sh`. Two facts follow, and the code
//! below exists to keep them straight:
//!
//! **The eras carry `boundary_kind: Seed`.** That says *this is where the store
//! started*, never *this is when the upstream changed*. A seed boundary must
//! not feed skew arithmetic and must not generate a change notification,
//! because nothing changed — a store came into existence.
//!
//! **The observation carries `ObservationOutcome::Seeded`, at the instant the
//! snapshot was FETCHED.** This is the part that is easy to get wrong. The
//! snapshot really was fetched from the upstream at a known instant, by the
//! refresh script; recording that is honest, and recording the install instant
//! instead would claim fusiform looked at the upstream when the operator ran
//! the installer.
//!
//! Recording it matters beyond honesty. The first real fetch that DISAGREES
//! with the snapshot needs a left edge for its observation window. Without a
//! seeded observation there is none, and that fetch has to be written as
//! another `Seed` boundary — which claims the store came into existence twice,
//! loses the link to the observation that detected the change, and reports no
//! window where a real one exists. That is what this module's absence produced,
//! found by a probe before the seed was built.

use fusiform_core::normalize::{normalize_models_dev, NormalizedCatalog};
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_store::ingest::{catalog_digest, plan_ingest};
use fusiform_store::{CatalogError, CatalogStore, NewObservation};
use serde::Deserialize;

/// The snapshot, compiled in.
const SNAPSHOT: &[u8] = include_bytes!("../data/models-dev-seed.json");

/// Its provenance, compiled in alongside.
const SNAPSHOT_META: &str = include_str!("../data/models-dev-seed.meta.json");

/// What the refresh script recorded about the snapshot.
#[derive(Debug, Clone, Deserialize)]
pub struct SeedMeta {
    /// When the snapshot was fetched from the upstream, in epoch milliseconds.
    ///
    /// Load-bearing rather than documentary: it becomes the instant of the
    /// recorded `Seeded` observation, and therefore the left edge of the first
    /// disagreeing fetch's observation window.
    pub fetched_at_ms: i64,
    pub sha256: String,
    pub model_count: usize,
    pub provider_count: usize,
    pub source_url: String,
}

/// What happened when the store was seeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedOutcome {
    /// The store was empty and the snapshot was loaded.
    Seeded {
        eras_written: usize,
        model_count: usize,
        fetched_at: Timestamp,
        /// The catalog version the seed issued, equal to `fetched_at`.
        ///
        /// Reported rather than left implicit because it is what a consumer
        /// compares against, and because a seeded store serving version 0 was a
        /// real defect found only by reading this number off a live module.
        version: i64,
    },
    /// The store already held facts, so the snapshot was not applied.
    ///
    /// Not an error and not a no-op worth hiding: an operator restarting a
    /// module wants to know the seed did not overwrite anything.
    AlreadyPopulated { existing_eras: i64 },
}

/// Why seeding failed.
#[derive(Debug)]
pub enum SeedError {
    /// The compiled-in metadata did not parse.
    Meta(String),
    /// The compiled-in snapshot did not normalize.
    ///
    /// A build-time defect surfacing at runtime: the refresh script parses the
    /// document before writing it, so this means the normalizer changed in a
    /// way the snapshot does not satisfy.
    Snapshot(String),
    Store(CatalogError),
}

impl std::fmt::Display for SeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeedError::Meta(m) => write!(f, "seed metadata is unreadable: {m}"),
            SeedError::Snapshot(m) => write!(f, "embedded snapshot does not normalize: {m}"),
            SeedError::Store(e) => write!(f, "seeding the store failed: {e}"),
        }
    }
}

impl std::error::Error for SeedError {}

/// Read the compiled-in provenance.
pub fn meta() -> Result<SeedMeta, SeedError> {
    serde_json::from_str(SNAPSHOT_META).map_err(|e| SeedError::Meta(e.to_string()))
}

/// Normalize the compiled-in snapshot.
pub fn catalog() -> Result<NormalizedCatalog, SeedError> {
    normalize_models_dev(SNAPSHOT)
        .map(|outcome| outcome.catalog)
        .map_err(|e| SeedError::Snapshot(e.to_string()))
}

/// Load the snapshot into an empty store.
///
/// Does nothing if the store already holds eras. The check is on the store's
/// own content rather than on a flag or a marker file: a flag can disagree with
/// the database it describes, and the question being asked — "does this store
/// already know anything" — is answerable directly.
pub fn seed_if_empty(store: &CatalogStore) -> Result<SeedOutcome, SeedError> {
    // "Is this a fresh install?" answered by "have I written any models.dev
    // eras?", which is a fact about the WRITE PIPELINE standing in for a fact
    // about the INSTALL.
    //
    // They coincide, and the reason is a property rather than luck: the seed
    // writes its eras under `SourceId::ModelsDev`, so the rows it checks are
    // exactly the rows it would create. Nothing else writes models.dev eras
    // before the first poll.
    //
    // The property is worth stating because it would break QUIETLY. Add a
    // second source, seed it, and this gate reads zero models.dev eras on a
    // store that is not fresh at all — then re-seeds over a populated catalog
    // with a snapshot that may be months old. The check would still be correct
    // about what it asks and wrong about what it is being asked.
    //
    // ENGRAM lost nine hours to that shape on 2026-08-13: a gate asking "is my
    // newest head registered?" used to answer "is there history to backfill?",
    // where a head is unregistered by construction while its generation is in
    // flight, so the check never fired.
    let existing = store
        .era_count(SourceId::ModelsDev)
        .map_err(SeedError::Store)?;
    if existing > 0 {
        return Ok(SeedOutcome::AlreadyPopulated {
            existing_eras: existing,
        });
    }

    let meta = meta()?;
    let catalog = catalog()?;
    let fetched_at = Timestamp(meta.fetched_at_ms);

    // The observation is written FIRST, so the eras appended below can derive
    // their windows from a history that already contains it — the same ordering
    // the poll loop uses, for the same reason.
    //
    // Its hash is the normalized digest of the snapshot, so the first real
    // fetch of an unchanged document compares equal and correctly records
    // `Unchanged` rather than rewriting every fact.
    let observation_id: i64 = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: fetched_at,
            outcome: ObservationOutcome::Seeded,
            normalized_hash: Some(catalog_digest(&catalog)),
            // No raw hash: that field is a drift signal between successive
            // FETCHES, and a seed has no predecessor to drift from. Recording
            // the snapshot's sha256 here would put a value in a column whose
            // meaning is "the bytes of the last fetch", inviting a diff against
            // the next real fetch that would always show a change.
            raw_hash: None,
            // No ETag: the snapshot was fetched by a script whose response
            // headers are gone. Claiming one would make the first real poll
            // conditional on an ETag the upstream never issued to this store.
            etag: None,
            duration_ms: None,
            detail: Some(format!(
                "embedded snapshot, sha256 {}, {} models",
                meta.sha256, meta.model_count
            )),
        })
        .map_err(SeedError::Store)?;

    // The eras themselves are Seed boundaries with no observation link. The
    // schema enforces that pairing: a seed era naming an observation would read
    // as if the upstream had been observed to hold the seeded value at that
    // instant, which is the claim the boundary kind exists to avoid.
    let plan = plan_ingest(store, &catalog, fetched_at, BoundaryKind::Seed, None)
        .map_err(SeedError::Store)?;

    let eras_written = store.append_eras(&plan.eras).map_err(SeedError::Store)?;

    // The version advances to the snapshot's FETCH INSTANT, not to now.
    //
    // Without this the store served version 0 while holding a complete
    // catalog — measured on the first live deployment, where `ck models status`
    // reported 6,280 models, 68,026 eras and `catalog version 0`. The version
    // is what a consumer holds as a high-water mark and refuses at or below, so
    // a seeded catalog was indistinguishable from a store that knows nothing,
    // and stayed that way until the first CHANGED poll — an unchanged poll
    // advances nothing, so on a quiet upstream that is hours or days.
    //
    // The failure that makes it serious is the reinstall: a fresh install
    // seeded from a NEWER snapshot also served 0, so every consumer holding a
    // real version refused it — correctly by their own rule, and wrongly in
    // fact, because the refused catalog was the newer one. Silent at the
    // producer, total at the consumer: exactly the failure
    // `advance_catalog_version` is written to prevent, reached through the one
    // path that never called it.
    //
    // Fetch instant rather than now, because the version then states the
    // VINTAGE OF THE CONTENT rather than the age of the install. Two machines
    // installed a week apart from the same snapshot agree, and an install from
    // a stale snapshot is correctly ordered below a consumer's newer catalog
    // instead of claiming to supersede it.
    //
    // Monotonicity is safe here: this runs only on a store with no eras, so
    // the version it advances from is always 0, and the store's own guard
    // rejects a non-advancing write regardless.
    let version = store
        .advance_catalog_version(observation_id, fetched_at)
        .map_err(SeedError::Store)?;

    Ok(SeedOutcome::Seeded {
        eras_written,
        model_count: catalog.model_count(),
        fetched_at,
        version,
    })
}

/// How old the bootstrap snapshot may be before it must be refreshed.
///
/// # Why an age rather than a currency check
///
/// Whether the seed still matches upstream is not checkable offline, and a
/// gate that needs the network is a gate that fails on a plane. The AGE is
/// checkable, deterministic, and the thing that actually predicts drift.
///
/// # Where the number comes from
///
/// MEASURED, not chosen. On 2026-09-19 the seed was five weeks old and 341 of
/// 5,669 models present in both it and live upstream had a different input
/// rate — 6.0%, or roughly 1.2% per week. Two weeks puts a fresh install
/// within about 2.5% of upstream on its worst fact, which is the window it
/// occupies before its first poll lands.
///
/// That window matters more than it sounds, because it is exactly where a TEST
/// RIG lives: a rig that builds, asserts and exits may never poll at all, so a
/// fixture captured from it is a snapshot of the seed file rather than of the
/// catalog. One such fixture sent two seats hunting a synthesis path that did
/// not exist.
pub const SEED_MAX_AGE_MS: i64 = 14 * 24 * 60 * 60 * 1000;

#[cfg(test)]
mod freshness_tests {
    use super::*;

    /// The embedded snapshot is not older than `SEED_MAX_AGE_MS`.
    ///
    /// The second TIME-TRIGGERED gate here, and it fires the same way the plan
    /// price review does: a date passes with nothing touched. The scheduled CI
    /// run is what makes it work — a push-only gate is silent for exactly the
    /// quiet stretch in which a seed goes stale.
    ///
    /// It cannot tell you the seed is CORRECT, only that it is recent. Stated
    /// in the failure rather than implied, because a gate that seems to promise
    /// currency would stop anyone from checking.
    #[test]
    fn the_embedded_seed_is_not_stale() {
        let meta = meta().expect("the seed metadata parses");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_millis() as i64;

        let age_days = (now - meta.fetched_at_ms) / (24 * 60 * 60 * 1000);
        let max_days = SEED_MAX_AGE_MS / (24 * 60 * 60 * 1000);

        assert!(
            now - meta.fetched_at_ms <= SEED_MAX_AGE_MS,
            "\n\nNOT A BUILD FAILURE. Nothing changed; the bootstrap snapshot aged.\n\n\
             The embedded seed was captured {age_days} days ago and the limit is \
             {max_days}. A fresh install — every E2E rig, every new machine — \
             serves these values until its first poll lands, and a rig that \
             builds, asserts and exits may never poll at all.\n\n\
             Measured drift when this last lapsed: 6.0% of models had a \
             different input rate after five weeks.\n\n\
             Fix: ./scripts/refresh-seed.sh, then review the diff and commit \
             both files together.\n\n\
             What this gate CANNOT tell you: whether the seed matches upstream. \
             That is not checkable offline. It knows only that nobody has \
             refreshed it recently.\n"
        );
    }

    /// The gate can fail, and the control proves it is not vacuous.
    ///
    /// Without this the test above passes every day until the first lapse, and
    /// the first person to meet a real failure meets an untested message.
    #[test]
    fn a_stale_snapshot_would_be_caught() {
        let meta = meta().expect("parses");
        let now = meta.fetched_at_ms + SEED_MAX_AGE_MS + 1;
        assert!(
            now - meta.fetched_at_ms > SEED_MAX_AGE_MS,
            "one millisecond past the limit must be over it"
        );

        // CONTROL: exactly at the limit is NOT stale, or the boundary is off by
        // one and every seed fails a day early.
        let at_limit = meta.fetched_at_ms + SEED_MAX_AGE_MS;
        assert!(
            at_limit - meta.fetched_at_ms <= SEED_MAX_AGE_MS,
            "the limit itself is within budget"
        );
    }
}
