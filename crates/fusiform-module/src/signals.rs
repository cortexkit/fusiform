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

/// Counters stamped by the poll loop, read by the health path.
#[derive(Debug, Default)]
pub struct Signals {
    /// Advances once per completed poll attempt, whatever the outcome.
    ///
    /// A monotonic heartbeat rather than a timestamp of "last activity",
    /// because the question health asks is whether the loop is running at all,
    /// and a loop that is stuck answers that by not advancing this.
    poll_attempts: AtomicU64,
    /// When the last poll that OBSERVED something finished, in unix ms.
    ///
    /// Observed, not succeeded: a 304 observed that the content is unchanged
    /// and counts. A failed poll does not, which is what makes this the age of
    /// fusiform's knowledge rather than the age of its last request.
    last_observation_ms: AtomicI64,
    /// Consecutive failed polls. Reset on any observation.
    consecutive_failures: AtomicU64,
    /// When the store last accepted a write, in unix ms.
    last_write_ms: AtomicI64,
    /// Whether the store has been opened. Read as a dispatch-capability signal:
    /// a module that never opened its store cannot answer a read.
    store_open: AtomicU64,
}

/// A value meaning "never happened", chosen so ordinary arithmetic on it is
/// obviously wrong rather than subtly plausible.
const NEVER: i64 = i64::MIN;

impl Signals {
    pub fn new() -> Self {
        Self {
            poll_attempts: AtomicU64::new(0),
            last_observation_ms: AtomicI64::new(NEVER),
            consecutive_failures: AtomicU64::new(0),
            last_write_ms: AtomicI64::new(NEVER),
            store_open: AtomicU64::new(0),
        }
    }

    /// Stamp a completed poll that observed something.
    pub fn observed(&self, at_ms: i64) {
        self.poll_attempts.fetch_add(1, Ordering::Relaxed);
        self.last_observation_ms.store(at_ms, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
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

    /// Stamp a completed poll that observed nothing.
    pub fn failed(&self) {
        self.poll_attempts.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Stamp a durable write.
    pub fn wrote(&self, at_ms: i64) {
        self.last_write_ms.store(at_ms, Ordering::Relaxed);
    }

    /// Record that the store is open and usable.
    pub fn store_opened(&self) {
        self.store_open.store(1, Ordering::Relaxed);
    }

    pub fn poll_attempts(&self) -> u64 {
        self.poll_attempts.load(Ordering::Relaxed)
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
        assert_eq!(s.poll_attempts(), 1);

        s.failed();
        s.failed();
        // The loop is demonstrably running: three attempts.
        assert_eq!(s.poll_attempts(), 3);
        // But knowledge is still as old as the last real observation, which is
        // the distinction that keeps a failing fetcher from looking healthy.
        assert_eq!(s.observation_age_ms(5_000), Some(4_000));
        assert_eq!(s.consecutive_failures(), 2);
    }

    #[test]
    fn an_observation_clears_the_failure_streak() {
        let s = Signals::new();
        s.failed();
        s.failed();
        s.failed();
        assert_eq!(s.consecutive_failures(), 3);
        s.observed(10_000);
        assert_eq!(s.consecutive_failures(), 0);
    }
}
