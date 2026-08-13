#![forbid(unsafe_code)]

//! Test helpers shared across fusiform's crates.
//!
//! Dev-only. Nothing here ships in a binary; every dependent lists it under
//! `[dev-dependencies]`, and `no_production_crate_depends_on_the_testkit`
//! in the workspace tests enforces that.

/// Replace text in a fixture, and refuse to continue if nothing changed.
///
/// # Why this exists rather than a bare `str::replace`
///
/// A test that mutates a fixture and asserts the mutation is REJECTED proves
/// nothing when the mutation silently did not apply: `str::replace` returns the
/// original string when the pattern is absent, so the test runs against clean
/// input, gets the clean result, and passes. Green, with the guard it exists to
/// prove never exercised.
///
/// That is not hypothetical here. A test in `models_dev_normalization.rs`
/// shipped vacuous on exactly this: its pattern carried a trailing comma the
/// fixture did not have, so the replace was a no-op and the test asserted
/// against an unmutated document. And the same class recurred hours later in a
/// mutation probe, where an injected line was not the shape it claimed to be.
///
/// The general form, which ASTRO named: **a mutation that did not apply is
/// indistinguishable from a defect, and both present as green.** The control is
/// to verify the SETUP reached the state it claims before reading the OUTCOME.
///
/// # Panics
///
/// When `from` does not occur in `text`, naming both so the fix does not need a
/// debugger. A panic rather than a returned error because there is no correct
/// way for a caller to continue: the test that follows would be measuring the
/// wrong thing.
pub fn mutate(text: &str, from: &str, to: &str) -> String {
    let out = text.replace(from, to);
    if out == text {
        // `assert_ne!` would print both sides, and a fixture here is a
        // multi-megabyte captured document -- the failure that matters gets
        // buried under the input. The pattern and a nearby anchor are what a
        // reader needs.
        panic!(
            "the mutation did not apply: {from:?} does not occur in the fixture, \
             so this test would run against unmutated input and pass without \
             exercising anything.{}\n\
             Whitespace is the usual cause: a captured JSON document is \
             pretty-printed, so `[\"a\",\"b\"]` never appears in it.",
            nearest_hint(text, from)
        );
    }
    out
}

/// A hint at what the fixture actually contains near the failed pattern.
///
/// Takes the pattern's first few characters and shows one place they occur, so
/// the reader sees the real formatting instead of guessing at it.
fn nearest_hint(text: &str, from: &str) -> String {
    let probe: String = from.chars().take(8).collect();
    let Some(at) = text.find(probe.trim_end_matches(['"', '[', '{'])) else {
        return String::new();
    };
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = text[at..]
        .char_indices()
        .nth(120)
        .map_or(text.len(), |(i, _)| at + i);
    format!(
        "\n\nThe fixture near that text reads:\n{}",
        &text[start..end]
    )
}

/// Replace text in a fixture that is bytes rather than a string.
///
/// The same guarantee as [`mutate`], for the captured-document fixtures.
pub fn mutate_bytes(bytes: &[u8], from: &str, to: &str) -> Vec<u8> {
    let text = String::from_utf8(bytes.to_vec()).expect("fixture is UTF-8");
    mutate(&text, from, to).into_bytes()
}
