//! Health, derived mechanically.
//!
//! This module contains no I/O and takes no lock. It reads atomics and does
//! arithmetic, because a health reply that queues behind a degraded resource is
//! useless exactly when it is needed.
//!
//! # What the statuses mean here
//!
//! The distinction fusiform has to get right is between being STALE and being
//! BROKEN, because they have different responses and only one is urgent.
//!
//! - **Ok** — fusiform observed the upstream recently enough that its catalog
//!   is current, and it can answer reads.
//! - **Degraded** — fusiform can answer reads, but its knowledge is aging: the
//!   upstream is unreachable, or polls keep failing. Consumers still get
//!   correct answers about a world that may have moved. This is not an outage;
//!   escalating it as one trains an operator to ignore it.
//! - **Failing** — fusiform cannot do its job at all: no store, so no read can
//!   be answered. This is dispatch impairment and is the only state that should
//!   wake anyone.
//!
//! Fusiform is unusual among modules in that stale data is its main failure
//! mode and is *not* an outage — a catalog nobody can read is far worse than
//! one that is six hours old.

use subc_protocol::session::{HealthReport, HealthStatus};

use fusiform_core::FailureClass;

use crate::signals::Signals;

/// How old fusiform's knowledge may get before it reports degraded.
///
/// Four cadence intervals at the fixed 30-minute poll. One missed poll is
/// noise — a transient DNS failure, an upstream deploy — and reporting on it
/// makes the signal worthless. Four consecutive misses is a pattern.
pub const STALE_AFTER_MS: i64 = 4 * 30 * 60 * 1_000;

/// Consecutive failures before the failure streak alone means degraded.
///
/// Independent of the staleness clock because they catch different things: a
/// slow upstream fails polls without the catalog necessarily being wrong yet,
/// and this notices the pattern before the clock does.
pub const FAILURE_STREAK_DEGRADED: u64 = 4;

/// How long the poll loop may be silent before it is presumed dead.
///
/// Two cadences plus margin. The bound has to clear the worst LEGITIMATE gap
/// between attempts, which is one interval plus a fetch running to its full
/// 90-second timeout — about 31.5 minutes. Two cadences is 60, and the extra 10
/// covers scheduler slop without approaching `STALE_AFTER_MS`.
///
/// Deliberately BELOW the staleness threshold, so a stopped loop surfaces as
/// itself rather than later as its symptom. An operator told "the catalog is
/// two hours old" goes looking at the upstream; an operator told "the loop is
/// not running" restarts the module.
pub const LOOP_SILENT_AFTER_MS: i64 = 70 * 60 * 1_000;

/// Build a health report from mechanical signals alone.
///
/// `now_ms` is passed in rather than read from the clock inside, so the
/// arithmetic is testable at an exact instant. A health path that reads the
/// clock itself can only be tested by sleeping.
pub fn report(signals: &Signals, now_ms: i64) -> HealthReport {
    // Dispatch impairment first: without a store, nothing else is meaningful.
    // A module reporting "degraded, catalog is stale" while unable to answer
    // any read at all would be describing the less important of its two
    // problems.
    if !signals.store_is_open() {
        return HealthReport {
            status: HealthStatus::Failing,
            detail: Some("store is not open; no read can be answered".to_string()),
            metrics: Some(metrics(signals, now_ms)),
        };
    }

    let age = signals.observation_age_ms(now_ms);
    let failures = signals.consecutive_failures();

    // A loop that has STOPPED is checked before staleness, because it is the
    // cause of the staleness that follows and an operator sent to look at the
    // upstream would find nothing wrong with it.
    //
    // This gap exists because health is stateless — called fresh on every probe
    // with no memory of the last one — so the attempt COUNTER can never say it
    // stopped advancing. Only an instant can. Before this, a dead loop reported
    // Ok for two hours while an alive-but-failing loop reported Degraded in
    // thirty minutes: the worse condition was the quieter one, because failures
    // announce themselves and absence does not.
    //
    // The two are worth distinguishing beyond tidiness. A failing upstream may
    // recover on its own; a loop that is not running never will.
    if let Some(attempt_age) = signals.attempt_age_ms(now_ms) {
        if attempt_age > LOOP_SILENT_AFTER_MS {
            return HealthReport {
                status: HealthStatus::Degraded,
                detail: Some(format!(
                    "the poll loop has not attempted a fetch in {} minutes \
                     (cadence is {}); it is not running and will not recover on \
                     its own — restart the module",
                    attempt_age / 60_000,
                    crate::loop_::POLL_INTERVAL_MS / 60_000
                )),
                metrics: Some(metrics(signals, now_ms)),
            };
        }
    }

    // Never having observed is not an error. A module that just started has an
    // empty history and a seed; calling that unhealthy would make every fresh
    // install page someone, and the correct response is to wait one cadence.
    let (status, detail) = match age {
        None if failures >= FAILURE_STREAK_DEGRADED => (
            HealthStatus::Degraded,
            Some(format!(
                "no successful observation yet; {failures} consecutive fetch failures ({})",
                describe_failure(signals)
            )),
        ),
        None => (
            HealthStatus::Ok,
            Some("no observation yet; started recently".to_string()),
        ),
        Some(age) if age > STALE_AFTER_MS => (
            HealthStatus::Degraded,
            Some(format!(
                "catalog is {} minutes old; serving correct history of a world that may have moved",
                age / 60_000
            )),
        ),
        Some(_) if failures >= FAILURE_STREAK_DEGRADED => (
            HealthStatus::Degraded,
            Some(format!(
                "{failures} consecutive fetch failures ({}); catalog is still current but not refreshing",
                describe_failure(signals)
            )),
        ),
        Some(age) => (
            HealthStatus::Ok,
            Some(format!("catalog observed {} minutes ago", age / 60_000)),
        ),
    };

    HealthReport {
        status,
        detail,
        metrics: Some(metrics(signals, now_ms)),
    }
}

/// The raw signals, so an operator can see what the status was derived from.
///
/// Emitted alongside every report rather than only on failure: a status without
/// its inputs is an assertion, and the first thing anyone debugging a wrong
/// status needs is the numbers it was computed from.
/// Name what is failing, for the health detail line.
///
/// A count says a module is failing; it does not say whether the upstream is
/// unreachable, answering with an error status, or serving a body that will not
/// parse. Those have different fixes and different owners, and an operator
/// reading "4 consecutive failures" has to open the store to learn which.
fn describe_failure(signals: &Signals) -> &'static str {
    match signals.last_failure_class() {
        Some(FailureClass::Network) => "upstream unreachable",
        Some(FailureClass::HttpStatus) => "upstream returned an error status",
        Some(FailureClass::Parse) => "upstream body did not parse",
        // Deliberately not phrased as a fusiform failure. The document is
        // well formed and fusiform read it correctly; it was refused because
        // believing it would have deleted most of the catalog. An operator
        // needs to look at the upstream, not at this module.
        Some(FailureClass::Implausible) => "upstream document dropped too much of the catalog",
        // The streak is nonzero but no class is recorded, which this build
        // cannot produce. Reported as unknown rather than guessed: a wrong
        // cause sends an operator somewhere specific and wrong.
        None => "cause not recorded",
    }
}

fn metrics(signals: &Signals, now_ms: i64) -> serde_json::Value {
    serde_json::json!({
        "poll_attempts": signals.poll_attempts(),
        "consecutive_failures": signals.consecutive_failures(),
        // Present so a machine reading health can branch on the cause without
        // parsing the detail sentence.
        "last_failure_class": signals.last_failure_class().map(|c| match c {
            FailureClass::Network => "network",
            FailureClass::HttpStatus => "http_status",
            FailureClass::Parse => "parse",
            FailureClass::Implausible => "implausible",
        }),
        "observation_age_ms": signals.observation_age_ms(now_ms),
        // How long since the loop last ATTEMPTED, distinct from how long since
        // it last observed. A failing loop keeps this current while
        // observation_age_ms grows; a stopped loop leaves both behind.
        "attempt_age_ms": signals.attempt_age_ms(now_ms),
        "last_write_age_ms": signals.last_write_age_ms(now_ms),
        "store_open": signals.store_is_open(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_store() -> Signals {
        let s = Signals::new();
        s.store_opened();
        s
    }

    #[test]
    fn no_store_is_failing_regardless_of_everything_else() {
        let s = Signals::new();
        // Even with a perfectly fresh observation: a catalog nobody can read is
        // worse than a stale one.
        s.observed(1_000);
        let r = report(&s, 1_100);
        assert_eq!(r.status, HealthStatus::Failing);
        assert!(r.detail.unwrap().contains("store"));
    }

    #[test]
    fn a_fresh_install_with_no_observation_yet_is_ok() {
        let s = open_store();
        let r = report(&s, 1_000_000);
        assert_eq!(
            r.status,
            HealthStatus::Ok,
            "a module that started a minute ago must not page anyone"
        );
    }

    #[test]
    fn a_stale_catalog_is_degraded_not_failing() {
        let s = open_store();
        s.observed(0);

        // Five hours later, with the loop STILL POLLING — every attempt
        // failing, so the catalog ages while the loop stays alive. Keeping the
        // attempts current is what makes this a staleness case rather than a
        // stopped-loop case, and the distinction is the point: without the
        // attempts, five hours of silence is a dead loop and health should say
        // so instead.
        let now = 5 * 60 * 60 * 1_000;
        let mut at = 0;
        while at + 30 * 60_000 <= now {
            at += 30 * 60_000;
            s.attempted(at);
            s.failed(FailureClass::Network);
        }

        let r = report(&s, now);
        assert_eq!(
            r.status,
            HealthStatus::Degraded,
            "stale data is fusiform's normal failure mode and is not an outage"
        );
        assert!(r.detail.unwrap().contains("300 minutes old"));
    }

    #[test]
    fn one_missed_poll_is_not_a_status_change() {
        let s = open_store();
        s.observed(0);
        s.failed(FailureClass::Network);
        // Thirty minutes on: one cadence interval, one failure.
        let r = report(&s, 30 * 60 * 1_000);
        assert_eq!(
            r.status,
            HealthStatus::Ok,
            "a single transient failure must not move the status, or the signal \
             becomes noise an operator learns to ignore"
        );
    }

    #[test]
    fn a_failure_streak_is_degraded_before_the_staleness_clock_fires() {
        let s = open_store();
        s.observed(0);
        for _ in 0..FAILURE_STREAK_DEGRADED {
            s.failed(FailureClass::Network);
        }
        // Only one interval has passed, so the age alone would still be Ok.
        let r = report(&s, 30 * 60 * 1_000);
        assert_eq!(r.status, HealthStatus::Degraded);
        assert!(r.detail.unwrap().contains("consecutive fetch failures"));
    }

    #[test]
    fn the_report_always_carries_the_numbers_it_was_derived_from() {
        let s = open_store();
        s.observed(1_000);
        s.wrote(1_100);
        let r = report(&s, 2_000);
        let m = r.metrics.expect("metrics must always be present");
        assert_eq!(m["poll_attempts"], 1);
        assert_eq!(m["observation_age_ms"], 1_000);
        assert_eq!(m["last_write_age_ms"], 900);
        assert_eq!(m["store_open"], true);
    }

    #[test]
    fn health_never_reports_its_own_opinion() {
        // The structural property: every input to `report` is a counter or a
        // timestamp stamped by the code that did the work. There is no field a
        // component can set to assert it is fine, so a wedged component cannot
        // claim health — it can only fail to advance a counter, which this
        // reads as degraded.
        let s = open_store();
        s.observed(0);
        let healthy = report(&s, 1_000).status;
        assert_eq!(healthy, HealthStatus::Ok);

        // Time passing alone flips it, with nothing else changing.
        let stale = report(&s, STALE_AFTER_MS + 1).status;
        assert_eq!(stale, HealthStatus::Degraded);
    }
}
