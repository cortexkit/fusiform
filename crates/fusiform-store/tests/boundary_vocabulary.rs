//! The schema's boundary-kind vocabulary matches the encoder's, both ways.
//!
//! # Why a text comparison rather than a behavioural test
//!
//! The `CHECK (boundary_kind IN (...))` constraint is unreachable through this
//! crate's API by construction: `boundary_columns` matches exhaustively over a
//! closed enum, so no caller can produce a string outside the list. Neutering
//! the CHECK entirely survives every test in the suite, and that is correct
//! rather than a coverage gap — the constraint defends against paths that
//! bypass the encoder (a hand-run `UPDATE`, a future migration writing rows
//! directly, a partial restore), and no test can reach those either.
//!
//! What IS reachable, and what this file exists for: **adding a `BoundaryKind`
//! variant compiles fine and fails at write time.** The `match` forces a new
//! arm, so the encoder is updated and the code builds; the CHECK is DDL and
//! knows nothing about it. The first write carrying the new kind fails with a
//! constraint violation — in production, at the moment the new boundary is
//! first needed, which for `Asserted` would be the first upstream that
//! publishes real effective dates.
//!
//! Two artifacts stating one vocabulary, with nothing comparing them. The same
//! shape as the design note's table list (`schema_doc.rs`) and the health
//! metric key set, and the same remedy: a fence that fails when the divergence
//! is WRITTEN rather than when someone next looks.
//!
//! # Both directions, deliberately
//!
//! A test written from the CHECK naturally asserts "every listed kind is
//! encodable" and misses the direction that matters — a kind the encoder
//! produces that the CHECK does not accept. Recorded in this repository's
//! history: §9 of the design note named four tables that never existed, and
//! only the reverse direction found them.

use std::collections::BTreeSet;

const SCHEMA: &str = include_str!("../src/schema.rs");
const STORE: &str = include_str!("../src/lib.rs");

/// The kinds the schema will accept.
fn schema_vocabulary() -> BTreeSet<String> {
    let anchor = "CHECK (boundary_kind IN (";
    let start = SCHEMA
        .find(anchor)
        .expect("the boundary-kind CHECK must exist; if it was removed, that is the finding")
        + anchor.len();
    let end = SCHEMA[start..]
        .find(')')
        .expect("the CHECK's list must close")
        + start;

    SCHEMA[start..end]
        .split(',')
        .map(|s| s.trim().trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The kinds the encoder produces.
///
/// Read from the `boundary_columns` body rather than by calling it, because
/// calling it requires constructing every variant — which is the enumeration
/// problem this test has, restated. The source text is the artifact that has to
/// agree with the DDL, and the DDL is text too.
fn encoder_vocabulary() -> BTreeSet<String> {
    let anchor = "fn boundary_columns(";
    let start = STORE.find(anchor).expect("boundary_columns must exist");
    let body = &STORE[start..];
    let end = body
        .find("\n}\n")
        .expect("the function must close at column zero");

    body[..end]
        .lines()
        .filter_map(|line| {
            // Every arm produces its column value as the first element of a
            // tuple: `=> ("observed", None)` or `=> (\n "corrected",`.
            let line = line.trim();
            let after_arrow = line.strip_prefix("BoundaryKind::").map(|rest| {
                rest.split_once("=>")
                    .map(|(_, r)| r.trim().to_string())
                    .unwrap_or_default()
            })?;
            let quoted = after_arrow.trim_start_matches('(').trim();
            if let Some(rest) = quoted.strip_prefix('"') {
                rest.split('"').next().map(str::to_string)
            } else {
                // A multi-line arm: the string is on the following line, and
                // `Corrected` is the one that takes this shape today. Handled by
                // the fallback scan below rather than silently dropped, because
                // dropping it would make this test blind to exactly the variant
                // carrying the most schema machinery.
                None
            }
        })
        .chain(
            // Multi-line arms: any bare quoted string inside the function body
            // that sits alone on its line.
            body[..end].lines().filter_map(|line| {
                let t = line.trim().trim_end_matches(',');
                if t.starts_with('"') && t.ends_with('"') && t.len() > 2 {
                    Some(t.trim_matches('"').to_string())
                } else {
                    None
                }
            }),
        )
        .collect()
}

#[test]
fn every_kind_the_encoder_produces_is_accepted_by_the_schema() {
    let schema = schema_vocabulary();
    let encoder = encoder_vocabulary();

    // The control: both extractions found something. Without it, a refactor
    // that breaks either parser leaves two empty sets comparing equal, and a
    // green test asserting nothing.
    assert!(
        encoder.len() >= 4,
        "the encoder extraction found {} kinds, which is fewer than the four \
         that exist — the parser has drifted from the source's shape and this \
         test is no longer reading what it claims to. Found: {encoder:?}",
        encoder.len()
    );
    assert!(
        schema.len() >= 4,
        "the schema extraction found {} kinds. Found: {schema:?}",
        schema.len()
    );

    let unaccepted: Vec<_> = encoder.difference(&schema).collect();
    assert!(
        unaccepted.is_empty(),
        "the encoder produces {unaccepted:?}, which the schema's CHECK will \
         REFUSE. This compiles: the match forces a new arm and the DDL knows \
         nothing about it, so the failure lands at the first write carrying the \
         new kind — in production, at the moment that boundary is first needed. \
         Add the kind to the CHECK in a migration."
    );
}

#[test]
fn the_schema_accepts_no_kind_the_encoder_cannot_produce() {
    let schema = schema_vocabulary();
    let encoder = encoder_vocabulary();

    // The direction a test written FROM the CHECK would omit, and the one that
    // found four nonexistent tables in the design note.
    let unproducible: Vec<_> = schema.difference(&encoder).collect();
    assert!(
        unproducible.is_empty(),
        "the schema accepts {unproducible:?}, which nothing produces. Either a \
         kind was renamed and the CHECK kept the old spelling — in which case \
         the constraint no longer defends what it names — or a kind was removed \
         and the vocabulary is now wider than the domain."
    );
}
