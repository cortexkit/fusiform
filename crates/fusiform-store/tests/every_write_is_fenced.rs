//! Every write goes through the writer fence.
//!
//! # Why this is a source check, and why it exists at all
//!
//! ASTRO reported on 2026-08-13 that their store has a fence they never call:
//! `with_conn_fenced` has zero call sites in their workspace, because startup
//! opens the lease-bearing store into a local, runs migrations, drops it, then
//! hands the daemon a separate unfenced connection on the same path. The API is
//! real, the epoch comparison is real, the rejection is real, and nothing routes
//! through it.
//!
//! They had read the fence's implementation and never asked who calls it. Those
//! feel like the same act because both end in "I read the code", and a text
//! search for the mechanism returns its definition and looks like a hit.
//!
//! Fusiform is not in that state — checked by enumeration when the report
//! arrived: three writes, three fenced, and the store holds its lease for the
//! whole process lifetime. But an enumeration is a convention someone has to
//! remember, and it goes stale the moment a fourth write is added. This is the
//! same fence-at-the-producer construction as the metric name pin: it fails
//! when a write appears outside the fenced path, at the moment it is written.
//!
//! It is a SOURCE check rather than a behavioural one, and that is a real
//! limitation stated rather than hidden. Making a live store reject a write
//! needs a superseded lease, and five approaches to producing one all failed
//! (documented in `durability.rs`) — the lease is exclusive, so a second opener
//! is refused rather than the first being fenced. What this cannot catch is a
//! fence that is called but does not work; what it does catch is the defect
//! ASTRO actually hit, which is a write that never reaches it.

use std::collections::BTreeSet;

/// Statements that modify the database, as they appear in SQL.
const WRITE_KEYWORDS: [&str; 3] = ["INSERT INTO", "UPDATE ", "DELETE FROM"];

/// Read a store source file, with SQL string continuations joined.
///
/// # The checker must not disagree with the document's own structure
///
/// Rust string literals in this crate are wrapped with `\` continuations, and
/// that is the file's house style rather than an edge case — every multi-line
/// SQL statement in the store is written that way. A line-oriented scan over
/// that source cannot see a keyword split across the wrap:
///
///     conn.execute(
///         "INSERT \
///          INTO observation ...",
///
/// Probed rather than reasoned about: an unfenced write in exactly that shape
/// SURVIVED this checker before the join. ASTRO named the class an hour
/// earlier, from my own checker reading one line of a two-line list in the
/// design note and reporting a real table as missing — line-oriented reading of
/// a wrapped document is a parser that quietly disagrees with the document's
/// structure.
///
/// Joining changes the line NUMBERS, which is harmless here because both the
/// write scan and the fenced-range scan run over this same joined text. They
/// are consistent with each other, which is the only property the comparison
/// needs.
fn source(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(name);
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    join_continuations(&raw)
}

/// Collapse `\` line continuations inside string literals onto one line.
fn join_continuations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'\n') {
            chars.next();
            // Consume the wrapped line's leading whitespace.
            while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
                chars.next();
            }
            // Leave exactly one space, so `INSERT \` + `INTO` joins as
            // `INSERT INTO` rather than `INSERTINTO` or `INSERT  INTO`.
            //
            // The double-space case is not hypothetical: the first version
            // pushed unconditionally, and since the wrapped line already ended
            // with a space the join produced `INSERT  INTO`, which the exact
            // keyword never matches. The mutation still survived and the fix
            // looked right — a corrected parser that corrects to a string
            // nothing compares against.
            if !out.ends_with(' ') {
                out.push(' ');
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The line numbers on which a write statement begins.
fn write_lines(text: &str) -> BTreeSet<usize> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| {
            let trimmed = line.trim_start();
            // Skip comments: a doc comment naming a write is not a write.
            if trimmed.starts_with("//") || trimmed.starts_with("*") {
                return false;
            }
            WRITE_KEYWORDS.iter().any(|k| line.contains(k))
        })
        .map(|(i, _)| i + 1)
        .collect()
}

/// The line ranges covered by a `with_conn_fenced` call.
///
/// Brace-counted from the call to its closing brace. Crude, and sufficient:
/// the store's write methods are small and the alternative is a Rust parser for
/// a check that needs to answer one question.
fn fenced_ranges(text: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut ranges = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if !line.contains("with_conn_fenced(") {
            continue;
        }
        let mut depth = 0i32;
        let mut started = false;
        for (j, l) in lines.iter().enumerate().skip(i) {
            for c in l.chars() {
                match c {
                    '{' => {
                        depth += 1;
                        started = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if started && depth <= 0 {
                ranges.push((i + 1, j + 1));
                break;
            }
        }
    }
    ranges
}

/// No write may live outside a fenced block.
///
/// The failure this prevents is not hypothetical: a sibling module has a
/// correct fence with zero call sites, because its writes were routed around it
/// during a refactor and nothing objected.
#[test]
fn no_write_escapes_the_fence() {
    // Every file that can issue SQL against the store. `schema.rs` is excluded
    // deliberately and checked separately below.
    for file in ["lib.rs", "ingest.rs", "serve.rs", "correct.rs"] {
        let text = source(file);
        let ranges = fenced_ranges(&text);

        for line in write_lines(&text) {
            let fenced = ranges
                .iter()
                .any(|(start, end)| line >= *start && line <= *end);
            assert!(
                fenced,
                "{file}:{line} issues a write outside with_conn_fenced. A write that \
                 does not pass the fence can land on top of a replacement writer's \
                 history during a lease handover -- and the fence existing is not the \
                 same as the fence being CALLED, which is how a sibling module ended \
                 up with zero call sites for a correct implementation."
            );
        }
    }
}

/// The fence has call sites at all.
///
/// The literal defect ASTRO reported, stated as its own assertion because it is
/// distinguishable from the one above: a store with NO writes would pass
/// `no_write_escapes_the_fence` vacuously.
#[test]
fn the_fence_is_actually_called() {
    let text = source("lib.rs");
    let call_sites = text.matches("with_conn_fenced(").count();
    assert!(
        call_sites >= 3,
        "expected the observation, era and version writes to be fenced; found \
         {call_sites} call sites. A fence with no callers is a correct \
         implementation that never runs."
    );
}

/// Migrations run while the lease is held.
///
/// The other half of ASTRO's defect, and the half a call-site check cannot see:
/// their lease-bearing handle was dropped after migrations and before the
/// daemon's writes, so the lease was released before the first write. Fusiform's
/// `open` migrates through the same handle it returns, and that handle lives for
/// the process lifetime.
#[test]
fn the_store_migrates_through_the_handle_it_returns() {
    let text = source("lib.rs");
    let open_body = text
        .split("pub fn open(descriptor: &StorageDescriptor)")
        .nth(1)
        .expect("open() exists")
        .split("\n    }")
        .next()
        .expect("open() has a body");

    assert!(
        open_body.contains("inner.migrate("),
        "open() must run migrations on the handle it keeps"
    );

    // Exactly one connection is opened, and it is the one returned.
    //
    // Checking for `Ok(Self { inner })` alone is not enough and the mutation
    // proved it: dropping the handle and reopening a fresh one under the same
    // name still ends in that expression, so the assertion passed on the
    // literal defect it exists to catch. It was testing a string rather than
    // the property.
    //
    // Counting the opens is the property. A second `open_sqlite` in this
    // function means the returned handle is not the one that ran migrations,
    // which is precisely the shape that releases the lease between migration
    // and first write.
    let opens = open_body.matches("open_sqlite(").count();
    assert_eq!(
        opens, 1,
        "open() must acquire the lease exactly once and return that handle. \
         Found {opens} calls to open_sqlite: a handle dropped after migration \
         releases the lease before the first write, which is how a sibling \
         module ended up writing unfenced through a second connection on the \
         same path."
    );
    assert!(
        !open_body.contains("drop(inner)"),
        "open() must not drop the lease-bearing handle"
    );
}
