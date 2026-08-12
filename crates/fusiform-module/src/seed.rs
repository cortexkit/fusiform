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
