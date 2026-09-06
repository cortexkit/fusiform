//! What a refusal means, on the crate a consumer can depend on.
//!
//! # Why this is here
//!
//! ASTRO went looking for fusiform's before-history refusal and could not find
//! it at source, because it lives in the module crate rather than this one. So
//! the protocol crate carried neither the payload vocabulary nor the refusal
//! contract, and both omissions have the same consequence: a consumer builds a
//! plausible decoder that is wrong in a direction nothing detects.
//!
//! For the payload that meant a hand-written [`crate::money::RateValue`] whose
//! default arm prices what should be refused. Here it means treating a
//! permanent refusal as a transport fault and retrying it forever, or treating
//! a legitimate coverage answer as a caller defect and reporting a bug.
//!
//! # The distinction the code alone could not carry
//!
//! `bad_request` used to cover two things a consumer must act on differently:
//! a request this producer could not read, and a well-formed request about
//! catalog fusiform does not have. The first is the caller's defect. The second
//! is an ANSWER — "no model by that name has ever been recorded" is a fact
//! about the world, arrived at deliberately, and reporting it as a client bug
//! sends someone to debug a correct request.
//!
//! So they carry different codes, and [`RefusalKind`] is what a consumer
//! matches on rather than the string.

use serde::{Deserialize, Serialize};

/// The request could not be read: malformed JSON, a missing required field, an
/// identity that is incomplete. The caller's defect; retrying unchanged fails
/// identically.
pub const CODE_BAD_REQUEST: &str = "bad_request";

/// The request was well-formed and fusiform has no catalog to answer it with:
/// an unknown provider, an unknown model, an instant before the record begins.
///
/// NOT a caller defect. This is the refusal that exists so an empty catalog is
/// never returned in place of "nobody was watching" — the distinction between
/// absent and unobserved that this producer exists to hold.
pub const CODE_NO_COVERAGE: &str = "no_coverage";

/// Fusiform could not answer right now: the store is unreadable, the module is
/// still warming. The only kind worth retrying.
pub const CODE_UNAVAILABLE: &str = "unavailable";

/// A write fusiform declines to perform because performing it would be wrong —
/// a correction whose target value did not change across the window, which
/// would withhold a value that is correct.
pub const CODE_REFUSED: &str = "refused";

/// What a consumer should DO about a refusal.
///
/// Three kinds because they have three different responses, and a consumer that
/// cannot tell them apart necessarily gets two of them wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalKind {
    /// Fix the request. Retrying it unchanged will fail the same way.
    Malformed,
    /// Accept the answer. The request was fine and the catalog does not cover
    /// it — retrying changes nothing, and treating it as an error reports a
    /// defect that does not exist.
    OutsideCoverage,
    /// Retry later. A transient condition, and the ONLY kind where retrying is
    /// the right response.
    Retryable,
    /// A deliberate refusal to act. Retrying is wrong; the request itself is
    /// the thing to reconsider.
    Declined,
    /// A code this consumer's build does not recognise.
    ///
    /// Present so an added code NARROWS what a consumer may conclude rather
    /// than breaking the channel that carries it — the same posture as a closed
    /// vocabulary rejecting per-cell instead of rejecting a whole file. A
    /// consumer seeing this must not guess: the honest response is the same as
    /// for an error it cannot classify, which is to surface it rather than act
    /// on it.
    Unrecognized,
}

impl RefusalKind {
    /// Classify a wire code.
    ///
    /// Deliberately total and never panicking: an unfamiliar code from a newer
    /// producer must not take down a consumer that was correct yesterday.
    pub fn of(code: &str) -> Self {
        match code {
            CODE_BAD_REQUEST => RefusalKind::Malformed,
            CODE_NO_COVERAGE => RefusalKind::OutsideCoverage,
            CODE_UNAVAILABLE => RefusalKind::Retryable,
            CODE_REFUSED => RefusalKind::Declined,
            _ => RefusalKind::Unrecognized,
        }
    }

    /// Whether retrying the identical request could succeed.
    ///
    /// `Unrecognized` answers FALSE, and that is the load-bearing choice: an
    /// unknown code retried in a loop is the failure mode that takes down a
    /// producer, and a consumer that stops is recoverable by a human while one
    /// that hammers is not.
    pub fn is_retryable(self) -> bool {
        matches!(self, RefusalKind::Retryable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_code_is_not_retryable() {
        assert_eq!(RefusalKind::of("something_new"), RefusalKind::Unrecognized);
        assert!(
            !RefusalKind::of("something_new").is_retryable(),
            "an unrecognised code must not be retried: a consumer that hammers \
             a producer over a code it does not understand is the failure a \
             human cannot recover from"
        );

        // CONTROL: the one retryable kind really is retryable, so the assertion
        // above cannot pass by nothing ever being retryable.
        assert!(
            RefusalKind::of(CODE_UNAVAILABLE).is_retryable(),
            "unavailable must be retryable, else the assertion above is vacuous"
        );
    }

    #[test]
    fn coverage_and_malformed_are_distinguishable() {
        assert_ne!(
            RefusalKind::of(CODE_NO_COVERAGE),
            RefusalKind::of(CODE_BAD_REQUEST),
            "a well-formed request about catalog fusiform lacks is an ANSWER, \
             and a malformed one is a caller defect: collapsing them sends \
             someone to debug a correct request"
        );
    }
}
