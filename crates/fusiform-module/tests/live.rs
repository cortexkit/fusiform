//! The whole loop against the real upstream.
//!
//! Everything else in this repository tests against captured bytes. This is the
//! only test that exercises the parts a capture cannot reach: whether the
//! conditional GET actually works, whether the ETag the upstream returns is the
//! one it accepts back, and whether a live document still normalizes.
//!
//! Opt-in via `FUSIFORM_LIVE=1`, because a test that reaches the network fails
//! for reasons unrelated to the code and must not gate a build.

use std::sync::Arc;

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::SourceId;
use fusiform_module::fetch::{FetchOutcome, Fetcher, SourceEndpoint};
use fusiform_module::loop_::{tick, PollContext, TickOutcome};
use fusiform_module::signals::Signals;
use fusiform_store::CatalogStore;

fn enabled() -> bool {
    std::env::var("FUSIFORM_LIVE").is_ok()
}

fn context(dir: &tempfile::TempDir) -> PollContext {
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    })
    .expect("store opens");

    PollContext {
        store: Arc::new(store),
        fetcher: Fetcher::new().expect("http client builds"),
        endpoint: SourceEndpoint::models_dev(),
        signals: Arc::new(Signals::new()),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Three ticks against the live upstream: seed, then two conditional polls.
///
/// The second and third ticks are the point. If the conditional request works,
/// they cost zero body bytes and record as NotModified; if the ETag round trip
/// is broken in either direction, they silently fall back to full fetches and
/// this test says so.
#[tokio::test]
async fn a_live_poll_cycle_seeds_then_goes_conditional() {
    if !enabled() {
        eprintln!("skipped: set FUSIFORM_LIVE=1 to reach the network");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(&dir);

    // Tick 1: nothing known, so a full fetch and a seed.
    let first = tick(&ctx, now_ms()).await.expect("first tick");
    let TickOutcome::Changed { new_version } = first.outcome else {
        panic!(
            "a first tick against an empty store must be a change, got {:?}",
            first.outcome
        );
    };
    assert_eq!(new_version, 1);
    assert!(
        first.eras_written > 10_000,
        "the live document should seed tens of thousands of eras, got {}",
        first.eras_written
    );
    eprintln!(
        "live seed: {} eras, version {new_version}",
        first.eras_written
    );

    // The ETag must have been stored, or every later poll is unconditional.
    let etag = ctx
        .store
        .last_etag(SourceId::ModelsDev)
        .unwrap()
        .expect("the upstream sends an ETag and it must be recorded");
    eprintln!("stored etag: {etag}");

    // Tick 2: conditional. The upstream should answer 304.
    let second = tick(&ctx, now_ms()).await.expect("second tick");
    assert_eq!(
        second.outcome,
        TickOutcome::NotModified,
        "the conditional GET did not work: the upstream returned a body for an \
         ETag it had just issued, so every poll will pull the full document"
    );
    assert_eq!(second.eras_written, 0);

    // Tick 3: still conditional, proving the ETag survives a 304 rather than
    // being cleared by one.
    let third = tick(&ctx, now_ms()).await.expect("third tick");
    assert_eq!(
        third.outcome,
        TickOutcome::NotModified,
        "the ETag did not survive a 304, so polls alternate between \
         conditional and full"
    );

    // Four observations' worth of history: three ticks, none of which failed.
    assert_eq!(ctx.signals.poll_attempts(), 3);
    assert_eq!(ctx.signals.consecutive_failures(), 0);
    assert_eq!(
        ctx.store.catalog_version().unwrap(),
        1,
        "two unchanged polls must not advance the version"
    );
}

/// The live document normalizes and the ETag it returns is well-formed.
///
/// Separated from the loop test so a normalization regression and a conditional
/// -GET regression are distinguishable from the test name alone.
#[tokio::test]
async fn the_live_document_normalizes_today() {
    if !enabled() {
        eprintln!("skipped: set FUSIFORM_LIVE=1 to reach the network");
        return;
    }
    let fetcher = Fetcher::new().unwrap();
    let outcome = fetcher.poll(&SourceEndpoint::models_dev(), None).await;

    let FetchOutcome::Body { bytes, etag, .. } = outcome else {
        panic!("an unconditional poll must return a body, got {outcome:?}");
    };

    let parsed = fusiform_core::normalize_models_dev(&bytes)
        .expect("the live document must normalize with the shipped parser");
    eprintln!(
        "live: {} bytes, {} models, {} findings",
        bytes.len(),
        parsed.catalog.model_count(),
        parsed.findings.len()
    );

    assert!(parsed.catalog.model_count() > 1_000);
    assert!(
        etag.is_some(),
        "the conditional-GET design depends on this upstream sending an ETag"
    );

    // Every finding must be an unattributable charge unit. Anything else is a
    // shape the parser does not understand appearing in production.
    for finding in &parsed.findings {
        assert!(
            matches!(
                finding,
                fusiform_core::NormalizeError::UnattributableChargeUnit { .. }
            ),
            "unexpected finding on live data: {finding}"
        );
    }
}
