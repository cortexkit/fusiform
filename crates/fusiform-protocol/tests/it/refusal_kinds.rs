//! Every refusal code a producer emits maps to its own kind.
//!
//! A consumer acts on the KIND, not the string: a deliberate refusal read as
//! Malformed tells the caller their request is broken when it was understood
//! and declined. The unit tests beside `RefusalKind::of` assert only that
//! no_coverage and bad_request differ and that an unknown code is not
//! retryable, so any code could move to another non-retryable kind unnoticed.
//! This pins the whole table.

use fusiform_protocol::refusal::{
    RefusalKind, CODE_BAD_REQUEST, CODE_NO_COVERAGE, CODE_REFUSED, CODE_UNAVAILABLE,
};

#[test]
fn every_code_maps_to_its_own_kind() {
    let table = [
        (CODE_BAD_REQUEST, RefusalKind::Malformed),
        (CODE_NO_COVERAGE, RefusalKind::OutsideCoverage),
        (CODE_UNAVAILABLE, RefusalKind::Retryable),
        (CODE_REFUSED, RefusalKind::Declined),
    ];
    for (code, kind) in table {
        assert_eq!(RefusalKind::of(code), kind, "{code} maps to the wrong kind");
    }
}
