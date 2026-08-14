//! The mechanical signals the health path reads.
//!
//! Every value here is an atomic stamped by the code that does the work, and
//! read by nothing but [`crate::health`]. That shape is the whole point: a
//! health path that asks a component how it is doing gets an opinion, and a
//! wedged component's opinion is that it is fine. A counter that only advances
//! when a poll actually completes cannot say a poll completed.
//!
//! Three rules this module exists to enforce, from the fleet's health-path
//! contract:
//!
//! - **Mechanical signals, never self-opinion.** Nothing writes "I am healthy".
//!   The loop stamps what it DID; health arithmetic decides what that means.
//! - **No blocking lock, no disk, no subprocess on the health path.** A health
//!   reply that touches the store queues behind whatever is degrading the
//!   store, which is exactly the condition being probed. Atomics cannot queue.
//! - **Degraded is not dispatch-impaired.** Fusiform whose fetches are failing
//!   is degraded — it is serving history that is getting older. Fusiform that
//!   cannot answer a read is impaired. Collapsing the two either hides a real
//!   outage or pages someone for a stale catalog.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use fusiform_core::FailureClass;

/// No failure in the current streak.
const CLASS_NONE: u64 = 0;

/// Encode a failure class for atomic storage.
///
/// Exhaustive rather than defaulted: a new class must choose its encoding here
/// instead of silently sharing one with an existing class, which would make two
/// different outages read identically in health.
const fn encode_class(class: FailureClass) -> u64 {
    match class {
        FailureClass::Network => 1,
        FailureClass::HttpStatus => 2,
        FailureClass::Parse => 3,
        FailureClass::Implausible => 4,
    }
}

const fn decode_class(value: u64) -> Option<FailureClass> {
    match value {
        1 => Some(FailureClass::Network),
        2 => Some(FailureClass::HttpStatus),
        3 => Some(FailureClass::Parse),
        4 => Some(FailureClass::Implausible),
        // CLASS_NONE, and any value this build does not know. An unrecognised
        // encoding reports "no class" rather than guessing one, because a wrong
        // cause sends an operator somewhere specific and wrong.
        _ => None,
    }
}

/// Counters stamped by the poll loop, read by the health path.
///
/// `Default` is written out rather than derived, because the derived one is
/// WRONG here and silently so. `AtomicI64::default()` is zero, and zero is a
/// real instant — the unix epoch — so a derived `Signals` claims fusiform last
/// wrote in 1970 rather than never. A probe caught this reporting
/// `last_write_age_ms` as the entire age of the clock where production, built
/// with `new()`, correctly reports null.
///
/// The two constructors disagreeing is the defect rather than either value
/// being wrong on its own: every test used `Default` and production used
/// `new()`, so the sentinel under test was never the sentinel that ships.
#[derive(Debug)]
pub struct Signals {
    /// Advances once per poll that reached a VERDICT — an observation row, or
    /// a recorded failure.
    ///
    /// Not attempts, despite what this was called for most of its life. A tick
    /// that fails inside the store increments nothing here, and a probe caught
    /// it reading 1 after eleven attempts.
    polls_recorded: AtomicU64,
    /// How many ticks STARTED.
    ///
    /// Independent of the counter above by construction: stamped at the top of
    /// a tick, before the store is touched. That independence is what makes
    /// the pair a usable identity — see [`Signals::attempt_ledger`].
    attempts: AtomicU64,
    /// When the loop last ATTEMPTED a poll, in unix ms, whatever the outcome.
    ///
    /// The counters above answer "how many", and health is called fresh on
    /// every probe with no memory of the last one — so a counter alone can
    /// never say the loop STOPPED advancing it. That needs an instant.
    ///
    /// Distinct from `last_observation_ms` on purpose, and the difference is
    /// the whole point: an attempt that failed advances this and not that. A
    /// loop that is alive and failing keeps this current; a loop that has
    /// stopped leaves both behind, and only this one can tell them apart.
    last_attempt_ms: AtomicI64,
    /// When the last poll that OBSERVED something finished, in unix ms.
    ///
    /// Observed, not succeeded: a 304 observed that the content is unchanged
    /// and counts. A failed poll does not, which is what makes this the age of
    /// fusiform's knowledge rather than the age of its last request.
    last_observation_ms: AtomicI64,
    /// Consecutive failed polls. Reset on any observation.
    consecutive_failures: AtomicU64,
    /// How many polls have EVER failed, adopted from the store at startup and
    /// incremented on each failure. Distinct from the streak above, which
    /// resets on success and cannot answer "has this ever happened".
    failures_ever: AtomicU64,
    /// When the most recent failure was, adopted from the store at startup.
    last_failure_ms: AtomicI64,
    /// What the most recent failure WAS, encoded as a small integer.
    ///
    /// A count of failures says a module is failing; it does not say whether
    /// the upstream is unreachable, answering with an error status, or serving
    /// a body that will not parse. Those have different operators and different
    /// fixes, and the class is already computed at the failure site — recording
    /// only the count throws it away at the one place it is free to keep.
    ///
    /// Recording a cause and surfacing a cause are different features, and the
    /// first alone produces a perfect record of an outage nobody knows about:
    /// a sibling module ran three days with one of its data sources failing
    /// every poll, the reason sitting in a store row the whole time, while the
    /// health surface its operator actually watched said only that something
    /// was wrong.
    ///
    /// An integer rather than a lock-protected enum because this is read on the
    /// health path, which may not block.
    last_failure_class: AtomicU64,
    /// When the store last accepted a write, in unix ms.
    last_write_ms: AtomicI64,
    /// Whether the store has been opened. Read as a dispatch-capability signal:
    /// a module that never opened its store cannot answer a read.
    store_open: AtomicU64,
}

/// A value meaning "never happened", chosen so ordinary arithmetic on it is
/// obviously wrong rather than subtly plausible.
const NEVER: i64 = i64::MIN;

impl Default for Signals {
    fn default() -> Self {
        Self::new()
    }
}

impl Signals {
    pub fn new() -> Self {
        Self {
            polls_recorded: AtomicU64::new(0),
            attempts: AtomicU64::new(0),
            last_attempt_ms: AtomicI64::new(NEVER),
            last_observation_ms: AtomicI64::new(NEVER),
            consecutive_failures: AtomicU64::new(0),
            failures_ever: AtomicU64::new(0),
            last_failure_ms: AtomicI64::new(NEVER),
            last_failure_class: AtomicU64::new(CLASS_NONE),
            last_write_ms: AtomicI64::new(NEVER),
            store_open: AtomicU64::new(0),
        }
    }

    /// Stamp a completed poll that observed something.
    pub fn observed(&self, at_ms: i64) {
        self.polls_recorded.fetch_add(1, Ordering::Relaxed);
        self.last_attempt_ms.store(at_ms, Ordering::Relaxed);
        self.last_observation_ms.store(at_ms, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
        // Cleared with the streak it describes. A class outliving its streak
        // would have health explaining a failure that is no longer happening.
        self.last_failure_class.store(CLASS_NONE, Ordering::Relaxed);
    }

    /// Adopt everything the store knows that a restart would otherwise erase.
    ///
    /// Called once when the store opens, before the poll loop starts. It lives
    /// here rather than in `main` so a test can drive the real startup path:
    /// when this logic sat in the binary, a test could only reproduce it, and a
    /// mutation deleting the call from `main` reddened nothing. A signal whose
    /// only writer is unreachable from the test suite can be silently removed —
    /// the same defect this module hit with the attempt heartbeat.
    ///
    /// Errors are logged rather than returned. The store opened; a read failing
    /// here is not fatal because the loop will write its own values shortly, but
    /// it must not silently pass as a fresh install.
    pub fn adopt_from_store(
        &self,
        store: &fusiform_store::CatalogStore,
        source: fusiform_core::SourceId,
    ) {
        // The catalog's real age. Without it the staleness clock restarts with
        // the process, and a module whose upstream has been unreachable for
        // hours reports healthy the moment it restarts — which is precisely
        // when an operator is looking, because a restart is what they try when
        // something seems wrong.
        match store.last_confirming_observation(source) {
            Ok(Some(at)) => self.adopt_last_observation(at.0),
            // A genuinely fresh install: "no observation yet" is the true
            // answer, and health treats it as Ok rather than stale.
            Ok(None) => {}
            // Names what health will now SAY, not just what failed here.
            //
            // The read failing leaves the staleness clock empty, so health
            // reports observation_age_ms as null and calls it Ok — which is the
            // exact false-fresh-install reading this adoption exists to
            // prevent. An operator who sees only "could not read" has no reason
            // to distrust the health line that follows it.
            Err(e) => eprintln!(
                "fusiform: could not read the last observation at startup, so health \
                 will report observation_age_ms as null and look like a fresh \
                 install until the next poll: {e}"
            ),
        }

        // The instant the catalog last changed, for the same reason. This one
        // was missed when the observation clock was fixed: two adjacent metrics
        // of the same shape, one adopted across a restart and one not, so
        // production reported `last_write_age_ms: null` — never written —
        // beside a store holding 68,000 eras.
        match store.newest_era_boundary(source) {
            Ok(Some(at)) => self.adopt_last_write(at.0),
            // No eras at all, so null is then the true answer.
            Ok(None) => {}
            Err(e) => eprintln!(
                "fusiform: could not read the newest era at startup, so health will \
                 report last_write_age_ms as null — which reads as 'never written' \
                 rather than 'not adopted': {e}"
            ),
        }

        // And the failure HISTORY, which no atomic can supply.
        //
        // `process_consecutive_failures` is a current-state gauge: it resets on
        // the first success, so an operator reading health at 09:00 after a
        // failure at 02:00 that recovered by 03:00 sees zero —
        // indistinguishable from nothing ever having gone wrong. Correct for
        // "is fusiform failing now", useless for "has fusiform ever failed",
        // and only the second question survives the operator not being present
        // while the event holds.
        //
        // Read from the store rather than counted here, because a
        // process-scoped total would have the same gap one level up: it would
        // reset on restart, which is exactly when someone is looking.
        //
        // BROCA found the identical shape in their own refusal gauge tonight,
        // after I promoted it to sole observer of an event neither of us can
        // construct. The general form is the rare-event argument turned on an
        // instrument rather than an experiment: AN INSTRUMENT THAT ONLY READS
        // DURING THE EVENT IS ONLY AS GOOD AS THE ODDS SOMEONE IS LOOKING AT
        // THE RIGHT MOMENT.
        match store.failure_history(source) {
            Ok((count, at, class)) => {
                // The class of the last failure, even after the streak cleared.
                //
                // `last_failure_class` is stamped by the streak and clears with
                // it, so a healed failure reported a count and an age with a
                // NULL class — measured live: "failures_ever: 1,
                // last_failure_age_ms: 84628131, last_failure_class: null".
                // Something went wrong, 23 hours ago, kind unknown.
                //
                // Adopted only when the current streak is empty, so a live
                // failure's class always wins over history.
                if self.consecutive_failures() == 0 {
                    if let Some(c) = class {
                        self.last_failure_class
                            .store(encode_class(c), Ordering::Relaxed);
                    }
                }
                self.failures_ever.store(count as u64, Ordering::Relaxed);
                if let Some(at) = at {
                    self.last_failure_ms.store(at.0, Ordering::Relaxed);
                }
            }
            Err(e) => eprintln!(
                "fusiform: could not read the failure history at startup, so health \
                 will report failures_ever as 0 — which reads as 'never failed' \
                 rather than 'not adopted': {e}"
            ),
        }
    }

    /// Adopt the last observation instant recorded in the store.
    ///
    /// Called once when the store opens, before the poll loop starts. Without
    /// it the staleness clock restarts with the process, and a module whose
    /// upstream has been unreachable for hours reports healthy the moment it is
    /// restarted — which is exactly when an operator is looking, because a
    /// restart is what they try when something seems wrong.
    ///
    /// This does NOT make the health path touch the store. The read happens at
    /// open time, on the startup path, and only the resulting instant is
    /// stamped into an atomic; the health path still reads atomics alone.
    ///
    /// Deliberately does not touch `poll_attempts`: that counter is a heartbeat
    /// for THIS process's loop, and priming it would claim polls this process
    /// never made. Nor does it touch `consecutive_failures`, because a restart
    /// genuinely does clear the streak — the new process has failed nothing yet
    /// and the staleness clock is the signal that survives.
    pub fn adopt_last_observation(&self, at_ms: i64) {
        self.last_observation_ms.store(at_ms, Ordering::Relaxed);
    }

    /// Adopt the instant the catalog last changed, from the store.
    ///
    /// Same reasoning as `adopt_last_observation` and found the same way: the
    /// write clock is an atomic, so a restart empties it, and a module holding
    /// 68,000 eras reported `last_write_age_ms: null` — "fusiform has never
    /// written anything" — minutes after a placement.
    ///
    /// Null is a strong claim, not a missing value. It is the correct answer
    /// for a genuinely fresh install and a false one for a restart, and those
    /// two were indistinguishable in the metric an operator reads.
    ///
    /// The instant is the newest ERA BOUNDARY rather than a recorded write
    /// time, because that is what an operator means by "when did the catalog
    /// last change" — and it is a property of history rather than a number
    /// someone had to remember to record, so it is correct for eras written
    /// before this existed.
    pub fn adopt_last_write(&self, at_ms: i64) {
        self.last_write_ms.store(at_ms, Ordering::Relaxed);
    }

    /// Stamp a completed poll that observed nothing.
    pub fn failed(&self, class: FailureClass) {
        self.polls_recorded.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        // The durable count advances with the streak and NEVER resets: a
        // recovered failure must stay readable after it has healed.
        self.failures_ever.fetch_add(1, Ordering::Relaxed);
        self.last_failure_class
            .store(encode_class(class), Ordering::Relaxed);
    }

    /// Stamp that the loop attempted a poll, whatever came of it.
    ///
    /// Called at the TOP of a tick rather than after it, so a fetch that hangs
    /// for its full timeout still counts as the loop being alive. Stamping
    /// after would make a slow upstream look like a dead loop.
    pub fn attempted(&self, at_ms: i64) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        self.last_attempt_ms.store(at_ms, Ordering::Relaxed);
    }

    /// Ticks that started, and ticks that reached a verdict.
    ///
    /// A CONSERVATION IDENTITY, and its value comes entirely from the two
    /// numbers having independent sources: attempts are stamped at the top of a
    /// tick before anything can fail, verdicts are counted when one is
    /// recorded. A total derived from its own parts always balances and proves
    /// nothing.
    ///
    /// They diverge when a tick STARTED and recorded nothing, which happens
    /// only when the store itself failed — every fetch result is an outcome
    /// rather than an error. That state has no other signal today: no
    /// observation row, no failure streak, nothing an operator can see.
    pub fn attempt_ledger(&self) -> (u64, u64) {
        (
            self.attempts.load(Ordering::Relaxed),
            self.polls_recorded.load(Ordering::Relaxed),
        )
    }

    /// How long since the loop last attempted a poll. `None` if it never has.
    pub fn attempt_age_ms(&self, now_ms: i64) -> Option<i64> {
        match self.last_attempt_ms.load(Ordering::Relaxed) {
            NEVER => None,
            at => Some(now_ms - at),
        }
    }

    /// The class of the most recent failure, if the current streak has one.
    ///
    /// Reset by any observation, so it describes the streak health is currently
    /// reporting rather than something that happened last week.
    pub fn last_failure_class(&self) -> Option<FailureClass> {
        decode_class(self.last_failure_class.load(Ordering::Relaxed))
    }

    /// Stamp a durable write.
    pub fn wrote(&self, at_ms: i64) {
        self.last_write_ms.store(at_ms, Ordering::Relaxed);
    }

    /// Record that the store is open and usable.
    pub fn store_opened(&self) {
        self.store_open.store(1, Ordering::Relaxed);
    }

    /// How many polls reached a verdict, successful or failed.
    pub fn polls_recorded(&self) -> u64 {
        self.polls_recorded.load(Ordering::Relaxed)
    }

    /// How many polls have ever failed, and how long ago the last one was.
    ///
    /// Survives recovery AND restart, which the streak does not. Reading
    /// `(0, None)` means no poll has ever failed; `(n, Some(age))` with a zero
    /// streak means it happened and healed — go and read `ck models status`.
    pub fn failure_history(&self, now_ms: i64) -> (u64, Option<i64>) {
        let at = self.last_failure_ms.load(Ordering::Relaxed);
        let age = (at != NEVER).then(|| now_ms.saturating_sub(at));
        (self.failures_ever.load(Ordering::Relaxed), age)
    }

    pub fn consecutive_failures(&self) -> u64 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    pub fn store_is_open(&self) -> bool {
        self.store_open.load(Ordering::Relaxed) == 1
    }

    /// How long since fusiform last observed the upstream, in ms.
    ///
    /// `None` when it never has — which is a different state from "a long time
    /// ago", and the difference decides whether a fresh install is broken or
    /// simply new.
    pub fn observation_age_ms(&self, now_ms: i64) -> Option<i64> {
        match self.last_observation_ms.load(Ordering::Relaxed) {
            NEVER => None,
            at => Some(now_ms.saturating_sub(at)),
        }
    }

    pub fn last_write_age_ms(&self, now_ms: i64) -> Option<i64> {
        match self.last_write_ms.load(Ordering::Relaxed) {
            NEVER => None,
            at => Some(now_ms.saturating_sub(at)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_observed_is_not_observed_long_ago() {
        let s = Signals::new();
        // The distinction that decides whether a fresh install looks broken.
        assert_eq!(s.observation_age_ms(1_000_000), None);
        s.observed(999_000);
        assert_eq!(s.observation_age_ms(1_000_000), Some(1_000));
    }

    #[test]
    fn a_failed_poll_advances_the_heartbeat_without_refreshing_knowledge() {
        let s = Signals::new();
        s.observed(1_000);
        assert_eq!(s.polls_recorded(), 1);

        s.failed(FailureClass::Network);
        s.failed(FailureClass::Network);
        // The loop is demonstrably running: three attempts.
        assert_eq!(s.polls_recorded(), 3);
        // But knowledge is still as old as the last real observation, which is
        // the distinction that keeps a failing fetcher from looking healthy.
        assert_eq!(s.observation_age_ms(5_000), Some(4_000));
        assert_eq!(s.consecutive_failures(), 2);
    }

    #[test]
    fn an_observation_clears_the_failure_streak() {
        let s = Signals::new();
        s.failed(FailureClass::Network);
        s.failed(FailureClass::Network);
        s.failed(FailureClass::Network);
        assert_eq!(s.consecutive_failures(), 3);
        s.observed(10_000);
        assert_eq!(s.consecutive_failures(), 0);
    }
}
