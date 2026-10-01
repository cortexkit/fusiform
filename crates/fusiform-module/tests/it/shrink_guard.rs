//! Refusing an upstream document that would delete most of the catalog.
//!
//! Fusiform's ingest treats "absent from this document" as a withdrawal, so a
//! well-formed response describing far fewer models tombstones every one it
//! omits. That is correct for a real withdrawal and catastrophic for a bad
//! response — a truncated CDN cache entry, a provider index caught mid-rebuild,
//! an API returning `{}` on an internal error.
//!
//! Nothing else catches it: the bytes parse, the hash differs, and the
//! tombstones are exactly what the diff asks for. Found by a probe that asked
//! what one such response does, then measured: it took a seeded store from 14
//! models to 1, and `{}` normalizes cleanly as a zero-model catalog.

use std::sync::Arc;
use std::time::Duration;

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{FailureClass, SourceId};
use fusiform_module::fetch::FetchOutcome;
use fusiform_module::loop_::{apply, MAX_SHRINK_FRACTION, SHRINK_GUARD_MIN_MODELS};
use fusiform_module::signals::Signals;
use fusiform_store::CatalogStore;

struct Fixture {
    store: Arc<CatalogStore>,
    signals: Arc<Signals>,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
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
    let signals = Arc::new(Signals::new());
    signals.store_opened();
    Fixture {
        store: Arc::new(store),
        signals,
        _dir: dir,
    }
}

/// A synthetic models.dev document with `n` models under one provider.
///
/// Generated rather than cut from a fixture because the guard's threshold is a
/// fraction of the catalog, and the excerpt has 14 models — below the minimum
/// where the fraction means anything. Testing a percentage rule needs a
/// population it can express.
fn document(n: usize) -> Vec<u8> {
    let mut models = String::new();
    for i in 0..n {
        if i > 0 {
            models.push(',');
        }
        models.push_str(&format!(
            r#""m{i}":{{"id":"m{i}","name":"Model {i}","limit":{{"context":100000,"output":8000}},"cost":{{"input":1,"output":2}}}}"#
        ));
    }
    format!(r#"{{"p":{{"id":"p","name":"P","models":{{{models}}}}}}}"#).into_bytes()
}

fn body(bytes: Vec<u8>) -> FetchOutcome {
    FetchOutcome::Body {
        bytes,
        etag: None,
        duration: Duration::from_millis(10),
    }
}

fn seed(f: &Fixture, n: usize, at: i64) {
    let report = apply(
        &f.store,
        &f.signals,
        SourceId::ModelsDev,
        body(document(n)),
        at,
    )
    .expect("the seed poll succeeds");
    assert!(report.eras_written > 0, "the seed must write eras");
    assert_eq!(f.store.present_model_count(SourceId::ModelsDev).unwrap(), n);
}

/// A document that drops most of the catalog is refused, and nothing is written.
#[test]
fn a_collapsed_document_is_refused_and_writes_nothing() {
    let f = fixture();
    seed(&f, 400, 1_000);
    let eras_before = f.store.era_count(SourceId::ModelsDev).unwrap();

    // The upstream now describes one model. Everything else would be tombstoned.
    let report = apply(
        &f.store,
        &f.signals,
        SourceId::ModelsDev,
        body(document(1)),
        2_000,
    )
    .expect("a refusal is not a tick error");

    match report.outcome {
        fusiform_module::loop_::TickOutcome::Failed { class } => {
            assert_eq!(
                class,
                FailureClass::Implausible,
                "a well-formed document refused for its content is not a parse failure"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(report.eras_written, 0);

    // Nothing was tombstoned.
    assert_eq!(
        f.store.era_count(SourceId::ModelsDev).unwrap(),
        eras_before,
        "a refused document must write no eras at all"
    );
    assert_eq!(
        f.store.present_model_count(SourceId::ModelsDev).unwrap(),
        400,
        "the catalog must survive intact"
    );
}

/// An empty document is refused too.
///
/// `{}` normalizes cleanly as a zero-model catalog, which is the most extreme
/// version of the same failure and the easiest for an upstream to produce.
#[test]
fn an_empty_document_is_refused() {
    let f = fixture();
    seed(&f, 400, 1_000);

    let report = apply(
        &f.store,
        &f.signals,
        SourceId::ModelsDev,
        body(b"{}".to_vec()),
        2_000,
    )
    .unwrap();
    assert!(matches!(
        report.outcome,
        fusiform_module::loop_::TickOutcome::Failed {
            class: FailureClass::Implausible
        }
    ));
    assert_eq!(
        f.store.present_model_count(SourceId::ModelsDev).unwrap(),
        400
    );
}

/// The refusal is recorded with its reason, not just counted.
#[test]
fn the_refusal_records_what_it_refused() {
    let f = fixture();
    seed(&f, 400, 1_000);
    apply(
        &f.store,
        &f.signals,
        SourceId::ModelsDev,
        body(document(1)),
        2_000,
    )
    .unwrap();

    let polls = f.store.recent_observations(SourceId::ModelsDev, 5).unwrap();
    let refusal = polls.first().expect("the refusal is recorded");
    assert_eq!(refusal.outcome, "failed");
    assert_eq!(refusal.failure_class.as_deref(), Some("implausible"));
    let detail = refusal.detail.as_deref().unwrap_or("");
    assert!(
        detail.contains("400") && detail.contains('%'),
        "the reason must name what was held and how much would go: {detail:?}"
    );

    // The raw hash is kept, so the exact document can be identified later.
    assert!(
        refusal.raw_hash.is_some(),
        "a refused document must stay identifiable"
    );
}

/// A drop inside the limit is a normal poll.
///
/// The guard must not turn every mass withdrawal into an outage. Measured: the
/// largest single provider carries 9.9% of the live catalog, so a whole
/// provider vanishing at once stays well inside the limit.
#[test]
fn a_drop_within_the_limit_is_applied_normally() {
    let f = fixture();
    seed(&f, 400, 1_000);

    // A 10% drop: larger than the biggest provider on models.dev today.
    let report = apply(
        &f.store,
        &f.signals,
        SourceId::ModelsDev,
        body(document(360)),
        2_000,
    )
    .unwrap();

    assert!(
        matches!(
            report.outcome,
            fusiform_module::loop_::TickOutcome::Changed { .. }
        ),
        "a plausible withdrawal must be applied, got {:?}",
        report.outcome
    );
    assert_eq!(
        f.store.present_model_count(SourceId::ModelsDev).unwrap(),
        360,
        "the withdrawn models must actually be tombstoned"
    );
}

/// The threshold is a boundary, and both sides of it are checked.
///
/// A test that only exercises an obvious collapse would pass with the limit set
/// anywhere between 2% and 99%.
#[test]
fn the_limit_is_where_it_says_it_is() {
    let held = 400usize;

    // Just inside: a drop of exactly the limit is allowed.
    let f = fixture();
    seed(&f, held, 1_000);
    let allowed = held - (held as f64 * MAX_SHRINK_FRACTION) as usize;
    let report = apply(
        &f.store,
        &f.signals,
        SourceId::ModelsDev,
        body(document(allowed)),
        2_000,
    )
    .unwrap();
    assert!(
        matches!(
            report.outcome,
            fusiform_module::loop_::TickOutcome::Changed { .. }
        ),
        "a drop of exactly the limit must be allowed, got {:?}",
        report.outcome
    );

    // Just outside: one model further is refused.
    let g = fixture();
    seed(&g, held, 1_000);
    let refused = allowed - 1;
    let report = apply(
        &g.store,
        &g.signals,
        SourceId::ModelsDev,
        body(document(refused)),
        2_000,
    )
    .unwrap();
    assert!(
        matches!(
            report.outcome,
            fusiform_module::loop_::TickOutcome::Failed {
                class: FailureClass::Implausible
            }
        ),
        "one model past the limit must be refused, got {:?}",
        report.outcome
    );
}

/// A small store is not guarded.
///
/// Below the minimum the fraction cannot be expressed usefully, and a small
/// catalog is a fixture or a fresh install rather than a state worth defending.
#[test]
fn a_small_store_is_not_guarded() {
    let f = fixture();
    let small = SHRINK_GUARD_MIN_MODELS - 1;
    seed(&f, small, 1_000);

    let report = apply(
        &f.store,
        &f.signals,
        SourceId::ModelsDev,
        body(document(1)),
        2_000,
    )
    .unwrap();
    assert!(
        matches!(
            report.outcome,
            fusiform_module::loop_::TickOutcome::Changed { .. }
        ),
        "a store below the guard minimum applies the document, got {:?}",
        report.outcome
    );
}

/// A GROWING catalog is never refused.
///
/// The guard is about loss. Measured: 821 models appeared over 18 days, so
/// growth is the normal direction and a guard that fired on it would refuse
/// every ordinary poll.
#[test]
fn growth_is_never_refused() {
    let f = fixture();
    seed(&f, 400, 1_000);
    let report = apply(
        &f.store,
        &f.signals,
        SourceId::ModelsDev,
        body(document(900)),
        2_000,
    )
    .unwrap();
    assert!(matches!(
        report.outcome,
        fusiform_module::loop_::TickOutcome::Changed { .. }
    ));
    assert_eq!(
        f.store.present_model_count(SourceId::ModelsDev).unwrap(),
        900
    );
}

/// A refusal degrades health rather than passing silently.
#[test]
fn a_refusal_is_visible_in_health() {
    use subc_protocol::session::HealthStatus;

    let f = fixture();
    seed(&f, 400, 1_000);

    // Enough refusals to cross the streak threshold.
    for i in 0..6 {
        apply(
            &f.store,
            &f.signals,
            SourceId::ModelsDev,
            body(document(1)),
            2_000 + i * 1_000,
        )
        .unwrap();
    }

    let report = fusiform_module::health::report(&f.signals, 10_000);
    assert_eq!(report.status, HealthStatus::Degraded);
    let detail = report.detail.unwrap_or_default();
    assert!(
        detail.contains("dropped too much"),
        "health must name the cause: {detail:?}"
    );
    assert_eq!(
        report
            .metrics
            .as_ref()
            .and_then(|m| m.get("last_failure_class"))
            .and_then(|v| v.as_str()),
        Some("implausible"),
        "a machine reading health must be able to branch on it"
    );

    // The catalog is still intact and still served.
    assert_eq!(
        f.store.present_model_count(SourceId::ModelsDev).unwrap(),
        400,
        "refusing is only worth doing if the last good catalog survives"
    );
}

/// A recovered upstream is applied normally.
///
/// The guard must not wedge: once the upstream returns a plausible document the
/// module resumes, including applying a large drop that arrives incrementally.
#[test]
fn the_module_recovers_when_the_upstream_does() {
    let f = fixture();
    seed(&f, 400, 1_000);
    apply(
        &f.store,
        &f.signals,
        SourceId::ModelsDev,
        body(document(1)),
        2_000,
    )
    .unwrap();

    let report = apply(
        &f.store,
        &f.signals,
        SourceId::ModelsDev,
        body(document(400)),
        3_000,
    )
    .unwrap();
    // The document matches what is held, so this is an unchanged poll.
    assert_eq!(
        report.outcome,
        fusiform_module::loop_::TickOutcome::Unchanged,
        "the upstream came back with the same catalog"
    );

    let health = fusiform_module::health::report(&f.signals, 3_500);
    assert_eq!(health.status, subc_protocol::session::HealthStatus::Ok);
}

/// A genuine mass withdrawal can still be recorded, one poll at a time.
///
/// This is the cost of the guard, stated as a test rather than left implicit: a
/// real 90% withdrawal is refused, and the operator's path is a sequence of
/// documents each inside the limit. It is deliberately awkward, because the
/// alternative is silent.
#[test]
fn a_genuine_mass_withdrawal_needs_more_than_one_poll() {
    let f = fixture();
    seed(&f, 400, 1_000);

    let mut at = 2_000;
    let mut held = 400usize;
    let mut polls = 0;
    while held > 40 {
        let next = std::cmp::max(40, held - (held as f64 * MAX_SHRINK_FRACTION) as usize);
        let report = apply(
            &f.store,
            &f.signals,
            SourceId::ModelsDev,
            body(document(next)),
            at,
        )
        .unwrap();
        assert!(
            matches!(
                report.outcome,
                fusiform_module::loop_::TickOutcome::Changed { .. }
            ),
            "step {polls}: {held} -> {next} should be inside the limit, got {:?}",
            report.outcome
        );
        held = next;
        at += 1_000;
        polls += 1;
        assert!(polls < 20, "the walk-down must terminate");
    }

    assert_eq!(
        f.store.present_model_count(SourceId::ModelsDev).unwrap(),
        40
    );
    eprintln!("a 90% withdrawal took {polls} polls");
}

/// The threshold VALUE is pinned to the measurements that justify it.
///
/// Every other test here computes its expectation from `MAX_SHRINK_FRACTION`,
/// so the guard is proven self-consistent and the constant is free to be
/// anything. Mutation showed exactly that: raising it to 90% broke nothing,
/// while a document dropping 89% of the catalog — the failure this guard
/// exists for — would sail through.
///
/// So this asserts bounds derived from live measurements rather than from the
/// constant:
///
/// - **Above 9.9%**, because the largest single provider on models.dev carries
///   618 of 6,254 models. A whole provider disappearing is the biggest
///   plausible legitimate loss, and a guard that refuses it turns an ordinary
///   upstream event into an outage.
/// - **Below 50%**, because a document describing less than half the catalog
///   is the failure being prevented. Any threshold permitting that makes the
///   guard decorative.
///
/// Both numbers come from `/docs/upstream-models-dev-measured.md` and the two
/// live captures behind it, not from taste.
///
/// **A mutant inside the range survives this test, and that is correct.**
/// Setting the constant to 0.49 breaks nothing here, because the measurements
/// justify a RANGE and not a point — 25% is a choice within it, and asserting
/// the exact value would be dressing a judgment as a measurement. For scale:
/// the five largest providers together carry 29.2% of the catalog, so 25%
/// refuses even a simultaneous top-five loss, which is the kind of event no
/// upstream produces in one 30-minute poll.
/// Enforced at COMPILE time rather than in a test body. Both sides are
/// constants, so a test would only fail when someone happens to run it; this
/// fails the build the moment the constant leaves the measured range.
const LARGEST_PROVIDER_SHARE: f64 = 618.0 / 6254.0; // 9.88%, measured
const _: () = assert!(
    MAX_SHRINK_FRACTION > LARGEST_PROVIDER_SHARE,
    "the shrink limit must admit the largest provider vanishing, or a real \
     upstream event becomes an outage"
);
const _: () = assert!(
    MAX_SHRINK_FRACTION < 0.5,
    "the shrink limit must refuse a document describing less than half the \
     catalog, which is the failure this guard exists for"
);

/// And the guard actually behaves that way against a catalog-shaped population.
///
/// The bounds above constrain a number; this checks the number produces the
/// intended behaviour at the two measured scales, so the constraint cannot be
/// satisfied by a constant the code does not really use.
#[test]
fn a_provider_sized_loss_passes_and_a_half_catalog_loss_does_not() {
    // 400 models standing in for the catalog, scaled from the live shape.
    let provider_sized = 400 - (400.0 * (618.0 / 6254.0)) as usize; // lose ~9.9%

    let f = fixture();
    seed(&f, 400, 1_000);
    let report = apply(
        &f.store,
        &f.signals,
        SourceId::ModelsDev,
        body(document(provider_sized)),
        2_000,
    )
    .unwrap();
    assert!(
        matches!(
            report.outcome,
            fusiform_module::loop_::TickOutcome::Changed { .. }
        ),
        "losing the largest provider must be applied, got {:?}",
        report.outcome
    );

    let g = fixture();
    seed(&g, 400, 1_000);
    let report = apply(
        &g.store,
        &g.signals,
        SourceId::ModelsDev,
        body(document(199)),
        2_000,
    )
    .unwrap();
    assert!(
        matches!(
            report.outcome,
            fusiform_module::loop_::TickOutcome::Failed {
                class: FailureClass::Implausible
            }
        ),
        "a document describing less than half the catalog must be refused, got {:?}",
        report.outcome
    );
}
