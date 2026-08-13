//! What health SAYS, not just what status it returns.
//!
//! A status word tells an operator that something is wrong; the detail tells
//! them where to look. Those are different features, and only the second has a
//! wrong answer — which is why these assertions are on the message rather than
//! on the enum.

use fusiform_core::FailureClass;
use fusiform_module::health;
use fusiform_module::signals::Signals;

/// A stale catalog names why it is stale, when the reason is known.
///
/// The staleness arm is checked BEFORE the failure-streak arm, so without this
/// an operator gets "catalog is 120 minutes old" for a case where the real
/// answer — the upstream has been refusing every poll — is already recorded one
/// field away. Staleness is the consequence; the failure class is what an
/// operator acts on.
///
/// Same defect as the poll loop's "tick failed" line, in the surface read first.
#[test]
fn a_stale_catalog_names_its_cause() {
    let signals = Signals::new();
    signals.store_opened();
    signals.adopt_last_observation(0);

    // Five hours later, with every poll failing on the network.
    let now = 5 * 60 * 60 * 1_000;
    for _ in 0..6 {
        signals.attempted(now);
        signals.failed(FailureClass::Network);
    }

    let report = health::report(&signals, now);
    let detail = report.detail.expect("a degraded report explains itself");

    // The PROSE rendering, not the wire token. `describe_failure` turns a class
    // into something an operator can act on -- "upstream unreachable" rather
    // than "network" -- and the machine spelling lives in the
    // `last_failure_class` metric beside it. Asserting the token here would
    // have been asserting the wrong layer's vocabulary, which is what the first
    // version of this test did.
    assert!(
        detail.contains("upstream unreachable"),
        "a stale catalog must name the failure cause, got {detail:?}"
    );

    // And the machine-readable class is on the metric, so a monitor can branch
    // on it without parsing prose.
    let metrics = report.metrics.expect("degraded reports carry metrics");
    assert_eq!(
        metrics["last_failure_class"], "network",
        "the class must also be readable as a token"
    );
    assert!(
        detail.contains("300 minutes"),
        "and must still state the age, got {detail:?}"
    );
}

/// Stale with NO recorded failure points at the loop rather than the upstream.
///
/// The absence of a cause is itself the diagnostic: if polls are not failing,
/// the upstream is not the problem, and sending an operator to check the
/// network wastes the first move of an incident.
#[test]
fn a_stale_catalog_with_no_failures_says_the_upstream_is_not_refusing() {
    let signals = Signals::new();
    signals.store_opened();
    signals.adopt_last_observation(0);

    // Five hours old, and the loop is still attempting — so this is not the
    // stopped-loop case either.
    let now = 5 * 60 * 60 * 1_000;
    signals.attempted(now);

    let report = health::report(&signals, now);
    let detail = report.detail.expect("a degraded report explains itself");

    assert!(
        detail.contains("no recent poll failure"),
        "must say the upstream is not refusing, got {detail:?}"
    );
    assert!(
        !detail.contains("network") && !detail.contains("parse"),
        "must not name a failure class it does not have, got {detail:?}"
    );
}
