//! What health says about writes after a restart.
//!
//! Written as a probe \u2014 print, assert nothing, read the output \u2014 after
//! production reported this minutes after a placement, with 68,768 eras in the
//! store:
//!
//!     "observation_age_ms": 92678,   <- adopted from the store at startup
//!     "last_write_age_ms": null      <- not adopted
//!
//! Two adjacent metrics of the same shape, one restored across a restart and
//! one not. Null is a strong claim rather than a missing value: it says
//! fusiform has never written anything, which is the correct answer for a
//! fresh install and a false one for a restart. The metric could not tell them
//! apart.
//!
//! The probe also found a second defect nobody was looking for \u2014 see
//! `the_two_constructors_agree_about_never`.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::{health, signals::Signals};
use fusiform_store::{CatalogStore, FactKey, NewEra, NewObservation};

fn open(path: &std::path::Path) -> CatalogStore {
    CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.join("store.db").to_string_lossy().to_string(),
        },
    })
    .unwrap()
}

const HOUR: i64 = 3_600_000;

/// Write a store with history: observed and written three hours ago.
fn seed_history(dir: &std::path::Path, at: Timestamp) {
    let store = open(dir);
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: at,
            outcome: ObservationOutcome::Changed { snapshot_seq: 1 },
            normalized_hash: Some("h".into()),
            raw_hash: Some("r".into()),
            etag: None,
            duration_ms: Some(100),
            detail: None,
        })
        .unwrap();
    // A poll that FAILED and then healed, an hour before the successful one.
    //
    // Without it, `last_failure_age_ms` is null on a restart — correct, because
    // nothing ever failed, and INDISTINGUISHABLE from the metric not being
    // adopted. The fence below cannot tell those apart from a value, so the
    // fixture has to contain the event.
    //
    // This is the whole reason the durable count exists: an operator arriving
    // after recovery sees a zero streak and a cleared class, and the failure is
    // unreadable from health without this pair.
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(at.0 - HOUR),
            outcome: ObservationOutcome::Failed {
                class: fusiform_core::FailureClass::Network,
            },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: Some(50),
            detail: Some("upstream unreachable".into()),
        })
        .unwrap();
    store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "p".into(),
            model_id: "m".into(),
            fact_key: FactKey::existence(),
            value_json: "\"present\"".into(),
            boundary_at: at,
            // A seed boundary carries no observation; the schema enforces it,
            // which is what caught the first version of this probe.
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        }])
        .unwrap();
}

/// A restart: a fresh process opening an existing store.
///
/// Calls the SAME function the daemon's startup path calls, rather than
/// reproducing what it does. The first version of this helper adopted the two
/// instants itself, and a mutation deleting the adoption from the startup path
/// reddened nothing — it proved the helper worked, not that anything called it.
fn restart(dir: &std::path::Path) -> (CatalogStore, Signals) {
    let store = open(dir);
    let signals = Signals::new();
    signals.store_opened();
    signals.adopt_from_store(&store, SourceId::ModelsDev);
    (store, signals)
}

/// A restart must not claim the catalog has never been written.
///
/// The write clock lives in an atomic that a restart empties, so without
/// adopting the newest era's boundary a module holding a full catalog reports
/// `last_write_age_ms: null`. An operator reads that as a statement about the
/// catalog; it is a statement about the process.
#[test]
fn a_restart_adopts_the_instant_the_catalog_last_changed() {
    let dir = tempfile::tempdir().unwrap();
    let now = 10 * HOUR;
    seed_history(dir.path(), Timestamp(now - 3 * HOUR));

    let (_store, signals) = restart(dir.path());
    let metrics = health::report(&signals, now)
        .metrics
        .expect("metrics are always present");

    assert_eq!(
        metrics["last_write_age_ms"],
        3 * HOUR,
        "a restarted module must report the real age of its newest era, not null"
    );
    assert_eq!(
        metrics["observation_age_ms"],
        3 * HOUR,
        "and the observation clock, which was already adopted"
    );
}

/// A genuinely empty store still reports null, because null is then true.
///
/// The pair matters: adopting an instant unconditionally would replace one
/// wrong answer with another, and a fresh install reporting a write age would
/// be worse than the defect being fixed.
#[test]
fn a_fresh_install_still_reports_no_write() {
    let dir = tempfile::tempdir().unwrap();
    let (_store, signals) = restart(dir.path());

    let metrics = health::report(&signals, 10 * HOUR)
        .metrics
        .expect("metrics are always present");

    assert!(
        metrics["last_write_age_ms"].is_null(),
        "an empty store has genuinely never been written: {}",
        metrics["last_write_age_ms"]
    );
}

/// Both constructors mean the same thing by "never".
///
/// Found by the probe above while looking for something else. `Signals` derived
/// `Default`, and `AtomicI64::default()` is ZERO \u2014 a real instant, the unix
/// epoch \u2014 while `new()` uses `i64::MIN` as its sentinel. So a defaulted
/// `Signals` claimed fusiform last wrote in 1970 and reported an age of the
/// entire unix clock, where production correctly reported null.
///
/// The defect was not either value: it was that every test used `Default` and
/// production used `new()`, so the sentinel under test was never the sentinel
/// that ships. A test can only catch what it constructs.
#[test]
fn the_two_constructors_agree_about_never() {
    let now = 10 * HOUR;
    let from_new = health::report(&Signals::new(), now).metrics.unwrap();
    let from_default = health::report(&Signals::default(), now).metrics.unwrap();

    assert_eq!(
        from_new, from_default,
        "Signals::new() and Signals::default() must describe the same module"
    );
    assert!(
        from_new["last_write_age_ms"].is_null() && from_new["observation_age_ms"].is_null(),
        "a module that has done nothing reports null, not an age since the epoch: {from_new}"
    );
}

/// Every metric is either process-scoped by name, or survives a restart.
///
/// The convention SUBC drew out of the null-write-age defect: a process-scoped
/// and a subject-scoped metric need different names or different values, never
/// the same field. `poll_attempts` read 1 on a module running all day against a
/// store holding fourteen observations — true of the process, and sitting in a
/// flat object beside `observation_age_ms`, which describes the catalog.
///
/// This is the mechanical form of that rule, so it fires on a metric added
/// later rather than depending on someone remembering the convention. Written
/// as an enumeration over the real metrics object, so a new field is covered
/// the day it appears rather than the day someone updates a list.
#[test]
fn every_metric_is_process_scoped_by_name_or_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let now = 10 * HOUR;
    seed_history(dir.path(), Timestamp(now - 3 * HOUR));

    let (_store, signals) = restart(dir.path());
    let metrics = health::report(&signals, now).metrics.unwrap();
    let object = metrics.as_object().expect("metrics is an object");

    // A restart against a store with real history. Anything describing the
    // CATALOG must have been adopted and therefore be non-null; anything
    // describing the PROCESS is legitimately empty and must say so in its name.
    for (key, value) in object {
        if key.starts_with("process_") {
            continue;
        }
        // `store_open` is a boolean about the process's own store handle rather
        // than an adopted instant, and it is true here because the store opened.
        if key == "store_open" {
            assert_eq!(value, true, "the store is open in this fixture");
            continue;
        }
        // `last_failure_class` is null when there is no current failure, which
        // is the honest answer rather than an unadopted one. Note the contrast
        // with `last_failure_age_ms` directly below: the CLASS describes the
        // current streak and clears with it, the AGE describes history and must
        // survive. Two adjacent fields about failure, one legitimately null
        // here and one not — which is exactly the pair this fence exists to
        // keep apart.
        // NO EXEMPTION for `last_failure_class` any more, and the expiry is the
        // point.
        //
        // It was skipped here because "null is the honest answer when there is
        // no current failure" — true while the field was stamped only by the
        // live streak. Once the class is adopted from history, null on a restart
        // against a store holding a failure is exactly the unadopted reading
        // this fence exists to catch.
        //
        // The exemption did not become wrong by being edited. It became wrong
        // because the field's meaning moved underneath it, and a skip carries no
        // signal when its justification expires: the fence kept passing, and
        // passing was the failure.

        assert!(
            !value.is_null(),
            "{key} is not process-scoped by name, so it must describe the catalog \
             and be adopted from the store at startup — it reported null on a \
             restart against a store with real history"
        );
    }

    // And the process-scoped ones are genuinely reset, which is what makes the
    // naming meaningful rather than decorative.
    assert_eq!(metrics["process_poll_attempts"], 0);
    assert_eq!(metrics["process_polls_recorded"], 0);
    assert_eq!(metrics["process_consecutive_failures"], 0);
}

/// Every metric name is accounted for, so a new one forces a scope decision.
///
/// The null check above has a gap it cannot close on its own: a new
/// process-scoped COUNTER would read `0` after a restart, and zero is not null,
/// so it would pass while telling an operator the catalog has never been
/// polled. No value-based test can separate a process counter from a catalog
/// one — both are numbers and both can legitimately be zero.
///
/// So the fence is on the key set instead. Adding a metric fails this test
/// until its name is listed here, which is the moment to decide which question
/// it answers. That is an exhaustiveness check at the producer rather than a
/// convention someone has to remember.
#[test]
fn the_metric_names_are_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let (_store, signals) = restart(dir.path());
    let metrics = health::report(&signals, 10 * HOUR).metrics.unwrap();

    let mut actual: Vec<&str> = metrics
        .as_object()
        .expect("metrics is an object")
        .keys()
        .map(String::as_str)
        .collect();
    actual.sort_unstable();

    // Process-scoped: what this process has done since it started. Reset by a
    // restart, and the prefix says so.
    // Catalog-scoped: adopted from the store, so a restart does not erase it.
    let expected = [
        "failures_ever",
        "last_failure_age_ms",
        "last_failure_class",
        "last_write_age_ms",
        "observation_age_ms",
        "process_attempt_age_ms",
        "process_consecutive_failures",
        "process_poll_attempts",
        "process_polls_recorded",
        "process_polls_unrecorded",
        "store_open",
    ];

    assert_eq!(
        actual, expected,
        "the metric names changed. Each one is either process-scoped (prefixed \
         `process_`, reset by a restart) or describes the catalog (adopted from \
         the store at startup). Pick one and add the name here."
    );
}

/// A restart adopts the failure COUNT, not just the instant.
///
/// Split from the naming fence deliberately. That fence asserts
/// `last_failure_age_ms` is non-null on a restart, which is satisfied by
/// adopting only `last_failure_ms` — so dropping the count's adoption passed it
/// while a restarted module reported `failures_ever: 0` beside a real age.
///
/// Two fields from one query, and a check on either is not a check on both.
/// Found by mutation: neutering the count survived every test until this one.
#[test]
fn a_restart_adopts_the_failure_count() {
    let dir = tempfile::tempdir().unwrap();
    let now = 10 * HOUR;
    seed_history(dir.path(), Timestamp(now - 3 * HOUR));

    let (_store, signals) = restart(dir.path());
    let metrics = health::report(&signals, now).metrics.unwrap();

    assert_eq!(
        metrics["failures_ever"], 1,
        "the fixture holds one failed poll, so a restarted module must report \
         it. Reading 0 means the count was not adopted — and 0 is the same \
         value a module with a clean history reports, so the difference is \
         invisible from the metric alone: {metrics}"
    );
    assert!(
        !metrics["last_failure_age_ms"].is_null(),
        "the instant must be adopted too — the pair comes from one query and \
         either half can be dropped independently"
    );
}

/// A failure stays readable after it has healed.
///
/// # The gap the adoption fence cannot see
///
/// `every_metric_is_process_scoped_by_name_or_survives_a_restart` proves
/// `failures_ever` is ADOPTED from the store. It says nothing about what
/// happens next: a success arriving in the same process could reset the total
/// and the fence would still pass, because the fence only ever looks at a
/// freshly restarted module.
///
/// That reset is the exact defect BROCA found in their own refusal gauge — a
/// current-state reading dressed as a history. Both conditions this counts are
/// rare AND self-clearing, so the realistic read is hours after the event, and
/// a total that clears on recovery reports zero exactly when someone is finally
/// looking.
///
/// AN INSTRUMENT THAT ONLY READS DURING THE EVENT IS ONLY AS GOOD AS THE ODDS
/// SOMEONE IS LOOKING AT THE RIGHT MOMENT. This is that argument applied to an
/// instrument rather than to an experiment.
#[test]
fn a_healed_failure_is_still_readable() {
    let signals = Signals::new();
    let now = 10 * HOUR;

    signals.failed(fusiform_core::FailureClass::Network);
    signals.failed(fusiform_core::FailureClass::Network);

    // The control: while it is happening, both readings agree.
    let during = health::report(&signals, now).metrics.unwrap();
    assert_eq!(during["process_consecutive_failures"], 2);
    assert_eq!(
        during["failures_ever"], 2,
        "control: the total must count the failures, or the recovery assertion \
         below proves nothing"
    );

    // Recovery.
    signals.observed(now);

    let after = health::report(&signals, now).metrics.unwrap();
    assert_eq!(
        after["process_consecutive_failures"], 0,
        "the streak must clear on success — it describes the CURRENT state"
    );
    // THE CLASS NO LONGER CLEARS, and this assertion flipping is the record of
    // a contract change rather than a test being loosened.
    //
    // It read `is_null()` when the class described the STREAK — true then, and
    // the reason `observed()` cleared it. Making the class durable in 785ea37
    // changed what the field means: it now answers "what was the last failure",
    // which stays true after recovery. The streak below is what answers "is
    // something failing now".
    //
    // Production caught the two writers disagreeing before this test did: the
    // status surface read the class from the store and showed "(network)" while
    // health showed null, because the first successful poll after startup
    // erased what adoption had stamped.
    assert_eq!(
        after["last_failure_class"], "network",
        "the class must survive recovery: it describes the LAST failure, not \
         the current streak, and an operator asking what went wrong yesterday \
         needs it after the failure has healed"
    );
    assert_eq!(
        after["failures_ever"], 2,
        "the durable total must NOT clear on success. An operator reading health \
         after a failure has healed sees a zero streak; if the \
         total also reads zero, the event is unreadable and the metric answers \
         'is it failing now' while appearing to answer 'has it ever failed'."
    );
}

/// A live failure's class is not overwritten by history.
///
/// # Why this condition is load-bearing rather than defensive
///
/// `adopt_from_store` runs at startup in `main.rs`, where the streak is always
/// zero — so the guard is trivially true at the only shipped call site, and a
/// mutation making it unconditional was caught only incidentally, by the golden
/// fixture noticing a wire change.
///
/// That is the shape I have been calling dead defensiveness all day: a
/// condition defending a state no caller reaches. The resolution is not to
/// delete it, because `adopt_from_store` is public and adopting mid-life is a
/// plausible future call — a supervisor re-adopting after a store swap, or a
/// test. The resolution is to DRIVE that call so the condition is exercised and
/// the precedence is stated.
///
/// The precedence matters because the two sources disagree about the present:
/// history says "the last failure was a parse error, 23 hours ago", the streak
/// says "the upstream is unreachable right now". An operator needs the second.
#[test]
fn a_live_failure_class_survives_adoption() {
    let dir = tempfile::tempdir().unwrap();
    seed_history(dir.path(), Timestamp(1_000));
    let store = open(dir.path());

    // A store whose HISTORY holds a parse failure.
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(500),
            outcome: ObservationOutcome::Failed {
                class: fusiform_core::FailureClass::Parse,
            },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: Some(10),
            detail: None,
        })
        .unwrap();

    // A process that is failing RIGHT NOW, for a different reason.
    let signals = Signals::new();
    signals.failed(fusiform_core::FailureClass::Network);

    // The control: the live class is in place before adoption, or the assertion
    // below cannot distinguish "adoption preserved it" from "adoption set it".
    let before = health::report(&signals, 10 * HOUR)
        .metrics
        .expect("metrics are always present");
    assert_eq!(
        before["last_failure_class"], "network",
        "control: the live failure must be stamped before adoption runs"
    );

    signals.adopt_from_store(&store, SourceId::ModelsDev);

    let after = health::report(&signals, 10 * HOUR)
        .metrics
        .expect("metrics are always present");
    assert_eq!(
        after["last_failure_class"], "network",
        "a live failure's class must survive adoption: history says parse, the \
         streak says network, and the operator needs to know what is failing \
         NOW. Got {after}"
    );
    // And the durable half is still adopted, so the guard narrows what it must.
    //
    // TWO, not one: `seed_history` already puts a healed failure in the store —
    // added earlier so the naming fence could tell an unadopted null from an
    // honest one — and this test adds the parse failure above. I wrote 1 here
    // from my model of the fixture rather than from its contents, which is the
    // unquestioned-input shape at the smallest possible scale.
    assert_eq!(
        after["failures_ever"], 2,
        "the count must still be adopted even when the class is not: the \
         fixture's healed failure plus this test's parse failure"
    );
}

/// A successful poll does not erase the adopted failure class.
///
/// # The second writer, which is what production actually caught
///
/// Adoption stamps the class at startup. `observed()` used to clear it on every
/// successful poll — correct while the field described the STREAK, and wrong the
/// moment it became durable. So the class survived exactly until the first
/// successful poll, about thirty minutes.
///
/// Measured in production on `ebe975b`: `ck models status` showed `(network)`,
/// read from the store, while `ck health` showed `last_failure_class: null`.
/// Two surfaces disagreeing about one fact, and SUBC's dual-surface acceptance
/// is the only reason it was visible — a single-surface check passes on status.
///
/// Neither the adoption nor the mapping was wrong. A second writer with the old
/// semantics undid the first, later, which no test of either writer alone can
/// see.
#[test]
fn a_successful_poll_does_not_erase_the_adopted_class() {
    let dir = tempfile::tempdir().unwrap();
    seed_history(dir.path(), Timestamp(1_000));
    let store = open(dir.path());
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(2_000),
            outcome: ObservationOutcome::Failed {
                class: fusiform_core::FailureClass::Network,
            },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: Some(10),
            detail: None,
        })
        .unwrap();

    let signals = Signals::new();
    signals.adopt_from_store(&store, SourceId::ModelsDev);

    // The control: adoption really did stamp it, or the assertion below cannot
    // tell "survived a poll" from "was never there".
    let after_adopt = health::report(&signals, 10 * HOUR)
        .metrics
        .expect("metrics are always present");
    assert_eq!(
        after_adopt["last_failure_class"], "network",
        "control: adoption must stamp the class before a poll can erase it"
    );

    // The event: a poll succeeds, which is the ordinary case within half an
    // hour of any restart.
    signals.observed(11 * HOUR);

    let after_poll = health::report(&signals, 11 * HOUR)
        .metrics
        .expect("metrics are always present");
    assert_eq!(
        after_poll["last_failure_class"], "network",
        "a successful poll must not erase the adopted class: the durable \
         history says a network failure happened, and that stays true after \
         the next poll succeeds. Got {after_poll}"
    );
    assert_eq!(
        after_poll["process_consecutive_failures"], 0,
        "and the streak must still clear — it is the field that says whether \
         something is failing NOW"
    );
}

/// A first failure in a fresh process reports WHEN, not just what.
///
/// # The opposite of the second-writer defect, found by the same sweep
///
/// After the class arc, I enumerated the writers of every durable field. The
/// class had two writers disagreeing about meaning. `last_failure_ms` had the
/// opposite problem: FEWER writers than its siblings. `failures_ever` and
/// `last_failure_class` were stamped by `failed()`; the instant was only ever
/// stamped by startup adoption.
///
/// So a module whose store held no prior failure, failing for the first time,
/// reported `failures_ever: 1, last_failure_class: network,
/// last_failure_age_ms: null` — one failure, of a known kind, that never
/// happened. Two of three fields describing one event and the third silent
/// about the only question they cannot answer between them.
///
/// This is not a variant of a startup gap: it needs no restart, no adoption,
/// and no store history. It is the ordinary first failure of any fresh process.
#[test]
fn a_first_failure_reports_when_it_happened() {
    let signals = Signals::new();
    signals.store_opened();

    // A fresh process with no history at all: the control is that the instant
    // starts null, or "not null afterwards" proves nothing.
    let before = health::report(&signals, 1_000)
        .metrics
        .expect("metrics are always present");
    assert!(
        before["last_failure_age_ms"].is_null(),
        "control: a process that has never failed must report no instant"
    );

    // The ordinary path: a poll is attempted, and it fails.
    signals.attempted(5_000);
    signals.failed(fusiform_core::FailureClass::Network);

    let after = health::report(&signals, 9_000)
        .metrics
        .expect("metrics are always present");
    assert_eq!(
        after["failures_ever"], 1,
        "the count must record the failure"
    );
    assert_eq!(after["last_failure_class"], "network", "and its kind");
    assert_eq!(
        after["last_failure_age_ms"], 4_000,
        "AND WHEN: a count and a class describing an event with no instant is \
         two thirds of one fact. The age is measured from the attempt stamp, \
         which `tick` records before anything can fail. Got {after}"
    );
}

/// Every durable field is populated by a fresh process doing its own work.
///
/// # The window closes on its own
///
/// SUBC's deployment-scale rule, from the missing-instant defect: every
/// deployed store acquires history immediately, so a fresh process's readings
/// get permanently harder to observe from the moment they start mattering. The
/// live store's 23-hour-old failure populated `last_failure_ms` at startup, so
/// production certified a path that was broken — a store with history is a
/// fixture that CANNOT construct the fresh-process case.
///
/// The specific defect was found by enumerating writers. This is the general
/// form as a fence: for every durable field, a process that adopts NOTHING and
/// then does the work must end up with a value. A field reachable only through
/// adoption is one whose fresh-process path nobody has driven.
///
/// Deliberately asserts on the RENDERED metrics rather than the atomics, so a
/// field that is stamped internally but not surfaced still fails here.
#[test]
fn a_fresh_process_populates_every_durable_field() {
    let signals = Signals::new();
    signals.store_opened();

    // The control: with no adoption and no work, the durable fields must read
    // as "nothing has happened" — otherwise the assertions below cannot tell a
    // populated field from a defaulted one.
    let fresh = health::report(&signals, 1_000)
        .metrics
        .expect("metrics are always present");
    for key in ["last_write_age_ms", "last_failure_age_ms"] {
        assert!(
            fresh[key].is_null(),
            "control: {key} must start null on a process that has adopted \
             nothing, or this test cannot distinguish work from a default"
        );
    }
    assert_eq!(fresh["failures_ever"], 0, "control: no failures yet");

    // Now the work a poll loop does, with no adoption anywhere: one failed
    // poll, then one that observes and writes.
    signals.attempted(2_000);
    signals.failed(fusiform_core::FailureClass::Parse);
    signals.attempted(3_000);
    signals.observed(3_000);
    signals.wrote(3_000);

    let after = health::report(&signals, 4_000)
        .metrics
        .expect("metrics are always present");

    // Each field, with the reason it must be populated rather than a bare
    // non-null check, so a failure says which mechanism is missing.
    assert_eq!(
        after["failures_ever"], 1,
        "the durable count must come from `failed()`, not from adoption"
    );
    assert_eq!(
        after["last_failure_age_ms"], 2_000,
        "the failure instant must be stamped by the failing poll: this is the \
         field that read null on a fresh process until b09d51b, reporting a \
         failure of a known kind that never happened"
    );
    assert_eq!(
        after["last_failure_class"], "parse",
        "and its class must survive the successful poll that followed"
    );
    assert_eq!(
        after["last_write_age_ms"], 1_000,
        "the write instant must be stamped by the writing poll, not adopted"
    );
    assert_eq!(
        after["observation_age_ms"], 1_000,
        "and the observation instant likewise"
    );
}
