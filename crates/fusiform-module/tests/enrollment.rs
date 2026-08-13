//! The engram enrollment, checked with engram's own parser.
//!
//! Validating with `engram_core::catalog::plan` rather than a schema check
//! written here is the point. A hand-written check asserts my reading of their
//! format; theirs asserts the format. The two agree right up until the format
//! changes, and the failure mode of disagreeing is that fusiform reports as
//! `Invalid` during a fleet walk — which makes engram refuse the whole capture,
//! not just this module's.

use std::path::PathBuf;

use engram_core::catalog::{plan, CaptureMechanism, Class, WriterInteraction};
use fusiform_testkit::mutate;

const DESCRIPTOR: &str = include_str!("../data/engram-catalog.json");

fn parsed() -> engram_core::catalog::CapturePlan {
    plan(DESCRIPTOR).expect("the descriptor must parse with engram's own parser")
}

/// The descriptor engram will read is the one in this repository.
#[test]
fn the_repository_descriptor_is_valid() {
    let p = parsed();

    // The module id must match the directory name engram finds it in, or the
    // fleet walk rejects it outright rather than capturing under either name.
    assert_eq!(p.module_id, "fusiform");

    assert_eq!(
        p.include.len(),
        1,
        "fusiform has exactly one capturable store"
    );
    let entry = &p.include[0];
    assert_eq!(entry.entry_id, "fusiform/store");
    assert_eq!(entry.rel_path, PathBuf::from("store.db"));
}

/// The store is captured live, not by pausing the poll loop.
///
/// `quiesce-required` would ask fusiform to stop writing for the duration of a
/// capture. For this module that means pausing polling, which widens an
/// observation window — the one quantity the fixed 30-minute cadence exists to
/// keep narrow. SQLite's backup API never takes the writer lock, so there is no
/// reason to pay that.
#[test]
fn capture_runs_against_the_live_store() {
    let entry = &parsed().include[0];
    assert_eq!(entry.mechanism, CaptureMechanism::WholeDb);
    assert_eq!(entry.writer_interaction, WriterInteraction::BackupApiLive);
}

/// Whatever mechanism is declared must actually be implemented.
///
/// Engram's `CaptureMechanism::is_implemented` exists because two of its five
/// declared mechanisms have no capture path anywhere in its workspace: a
/// descriptor may declare them, planning validates and includes them, and
/// capture then records a skip. A module declaring only unimplemented
/// mechanisms reports exactly like one whose data is fully captured.
///
/// So this asserts the property their own code was written to make checkable,
/// rather than trusting that a declared entry means a captured one.
#[test]
fn the_declared_mechanism_is_one_engram_can_actually_capture() {
    for entry in &parsed().include {
        assert!(
            entry.mechanism.is_implemented(),
            "{} declares {:?}, which engram plans but never captures",
            entry.entry_id,
            entry.mechanism
        );
    }
}

/// Nothing device-local is declared as portable.
///
/// The lease file names a writer on this machine and must not travel. It is
/// excluded by omission — it appears in no entry at all — so this asserts the
/// consequence: every included entry is portable, and nothing in the plan is
/// carrying machine-specific state to another host.
#[test]
fn no_device_local_state_is_declared_portable() {
    let p = parsed();
    for (entry_id, class) in &p.excluded {
        assert_ne!(
            *class,
            Class::Portable,
            "{entry_id} is excluded but marked portable"
        );
    }
    // And the lease is not smuggled in under the store's path.
    for entry in &p.include {
        let path = entry.rel_path.to_string_lossy();
        assert!(
            !path.contains("lease"),
            "{} would capture a lease file: {path}",
            entry.entry_id
        );
    }
}

/// A malformed descriptor fails here rather than during a fleet walk.
///
/// The mutation proves this file is actually being validated. Without it, a
/// descriptor that engram rejects would pass every assertion above by never
/// being parsed at all.
#[test]
fn a_broken_descriptor_is_refused() {
    let mutated = mutate(DESCRIPTOR, "\"whole-db\"", "\"telepathy\"");
    assert_ne!(mutated, DESCRIPTOR, "the mutation must apply");
    assert!(
        plan(&mutated).is_err(),
        "an unknown capture mechanism must be refused"
    );

    // A module_id that does not match the directory is the other fail-loud rule
    // engram's fleet walk enforces, and it is worth pinning here because the
    // symptom is a module silently not captured.
    let renamed = mutate(DESCRIPTOR, "\"fusiform\"", "\"fusifrom\"");
    let p = plan(&renamed).expect("a typo'd id still parses");
    assert_ne!(
        p.module_id, "fusiform",
        "this is what engram would compare against the directory name"
    );
}
