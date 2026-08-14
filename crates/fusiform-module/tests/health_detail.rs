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

/// A stale catalog is not blamed on a failure that already healed.
///
/// # Why the durable class needs a gate here
///
/// The class used to imply a live failure, because it cleared on every success.
/// Now it is durable, so its presence says only that something failed ONCE —
/// possibly yesterday, possibly healed thirty seconds later.
///
/// Reading it as a gate makes health attribute today's staleness to that old
/// failure: "catalog is 120 minutes old (upstream unreachable)" when the
/// upstream is answering fine and the real fault is the loop or the store. That
/// sends an operator to the one place that is definitely working, which is the
/// exact defect the class was added to prevent, inverted.
///
/// The streak is what answers "is something failing now"; the class only says
/// what kind.
#[test]
fn a_healed_failure_does_not_explain_todays_staleness() {
    let signals = Signals::new();
    signals.store_opened();

    // A failure that happened and healed: the class is stamped, the streak is
    // clear. This is the ordinary state of any module that has ever failed.
    signals.failed(FailureClass::Network);
    signals.observed(0);

    // The catalog is stale for an unrelated reason, with the loop still ALIVE —
    // otherwise the stopped-loop arm fires first and correctly says so, which
    // is a different (and better) message than the one under test here. Found
    // by running: my first fixture forgot to stamp an attempt and got
    // "the poll loop has not attempted a fetch in 121 minutes", a true
    // diagnosis of a condition I had accidentally created.
    let now = health::STALE_AFTER_MS + 60_000;
    signals.adopt_last_observation(0);
    signals.attempted(now);
    let report = health::report(&signals, now);
    let detail = report.detail.expect("a stale catalog reports why");

    assert!(
        detail.contains("no recent poll failure"),
        "a stale catalog with a HEALED failure must not be blamed on the \
         upstream: the class is durable now, so its presence no longer means \
         polls are failing. Got: {detail}"
    );
    assert!(
        !detail.contains("upstream unreachable"),
        "the healed network failure must not be presented as the cause of \
         today's staleness. Got: {detail}"
    );
}
