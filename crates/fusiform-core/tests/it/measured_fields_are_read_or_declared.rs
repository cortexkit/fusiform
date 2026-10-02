//! Every field the measurement document says the upstream publishes is either
//! READ by the parser or explicitly declared unread.
//!
//! # Why this fence exists
//!
//! `docs/upstream-models-dev-measured.md` recorded on day one that
//! models.dev's `limit` object is `{context, output, input?}`. It counted the
//! zeros in `limit.input` and named the models carrying them. Three days later
//! `RawLimit` still had two fields, and serde had been dropping the third in
//! silence on 1,199 rows — 470 of which state a model's geometry outright
//! (`input + output == context`), which is the fact this project had been
//! sourcing from provider documentation one page at a time.
//!
//! The document and the code disagreed, both written here, and **nothing
//! compared them**.
//!
//! That class already has a fence one layer over: `schema_doc.rs` holds the
//! design note's table list against the real schema in both directions,
//! because a note cannot say whether it describes an intention or the shipped
//! artifact. This is the same fence pointed at the artifact whose entire job is
//! to record what the upstream contains.
//!
//! A measurement document is worse than a design note in one specific way. A
//! design note describes what we DECIDED, so drift is a decision nobody made. A
//! measurement document describes what EXISTS, so drift means the code is not
//! reading something the upstream is sending — and every unread field is silent
//! by construction, because "absent from my parser" and "absent from the
//! payload" render identically downstream.
//!
//! # Why unread must be DECLARED rather than inferred
//!
//! Not every upstream field should be read. `provider` is a transport override
//! that decides how a request is spoken and must never be served; `status` and
//! `description` are advisory. The distinction this project has drawn since day
//! one is that **a quarantine is a promise and an unread field is a fact** — so
//! the fence does not demand that everything be read. It demands that the
//! decision EXIST, in writing, at the moment a field is added to the document.

use std::collections::BTreeSet;

const MEASURED: &str = include_str!("../../../../docs/upstream-models-dev-measured.md");
const RAW: &str = include_str!("../../src/normalize/raw.rs");

/// Fields the parser deliberately does not read, each with the reason.
///
/// Adding a name here is a decision that a future reader can weigh. Leaving a
/// field out of both this list and the parser is the state that shipped the
/// `limit.input` gap, and it is what this fence makes impossible.
const DECLARED_UNREAD: &[(&str, &str)] = &[
    (
        "description",
        "prose for humans; nothing branches on it and it is not served",
    ),
    (
        "status",
        "deprecated/beta/alpha — advisory. Retirement is observed from the \
         model leaving the document, which is a fact about the upstream rather \
         than a label it chose to apply.",
    ),
    (
        "input",
        "the third authored field of the `limit` object. Parsed, and \
         deliberately not carried into the domain: it is real geometry — 1,199 \
         models publish it and 470 have `input + output == context` exactly — \
         but WHICH access path it constrains is unstated. A reseller API-path \
         ceiling and a first-party prompt ceiling are both plausible and the \
         payload distinguishes neither, so serving it beside `limit.context` \
         would imply they bound the same thing. The overlay is where a \
         per-path ceiling belongs, because a cell there names the path it was \
         measured on. Enforced by total destructuring at the raw -> domain \
         conversion, not by this list alone.",
    ),
    (
        "temperature",
        "whether the model accepts a temperature parameter. Published on 5,791 \
         models, and a KNOB rather than a capacity: it says what a request may \
         set, not what the model can do. The line is worth stating because a \
         bool on the model object reads like a capability, and \
         `capability.reasoning` IS one. The difference is that reasoning says \
         the model can reason while temperature says a field is accepted, and \
         no consumer has asked fusiform to serve that. \
         \
         It was READ into the domain type and dropped there until 2026-08-16, \
         which is worse than unread: this fence counts a field as handled once \
         the parser touches it, so a field that reaches a struct and no further \
         passes while being served to nobody.",
    ),
    (
        "structured_output",
        "a capability no consumer has asked for. Unread rather than served, \
         and this line is the record of that being a choice.",
    ),
    (
        "interleaved",
        "bool OR {field: \"...\"} — a shape that changes between rows. Reading \
         it would require deciding what the object form means, and nobody \
         consumes it.",
    ),
];

/// Field names the measurement document says the upstream publishes.
fn fields_in_the_document() -> BTreeSet<String> {
    let mut out = BTreeSet::new();

    // Scoped to the `## Model object` section, and the scoping is the whole
    // difficulty. The document holds several tables — provider fields, cost
    // keys, HTTP response headers — and a parse that reads all of them reports
    // `etag`, `content-type` and `npm` as unread model fields. That is a
    // detector wrong about its own population, which presents as a long
    // confident list of findings.
    let start = MEASURED
        .find("## Model object")
        .expect("the measurement document must have a Model object section");
    let section = &MEASURED[start..];
    let end = section[3..]
        .find("\n## ")
        .map(|i| i + 3)
        .unwrap_or(section.len());
    let section = &section[..end];

    // The inventory is a markdown table whose first column is one or more
    // backticked field names. Rows outside it have no backticked first cell.
    for line in section.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let Some(first) = line.split('|').nth(1) else {
            continue;
        };
        let first = first.trim();
        if !first.starts_with('`') {
            continue;
        }
        for name in first.split(',') {
            let name = name.trim().trim_matches('`').trim();
            // Only top-level model fields: nested ones (`limit.input`) are
            // covered by the struct that reads their parent.
            if !name.is_empty() && !name.contains('.') && !name.contains(' ') {
                out.insert(name.to_string());
            }
        }
    }

    // The detector must find something before its silence means anything.
    // Measured: the inventory table carries 19 rows at the time of writing.
    // Held here rather than in one test, because three tests read this set and
    // a parse that finds nothing would pass each of them green.
    assert!(
        out.len() >= 15,
        "the field inventory parse found only {} fields, which means the \
         document's table shape changed and these fences are reading nothing. \
         Fix the parse before trusting anything that uses it.",
        out.len()
    );
    out
}

/// Field names the parser declares on `RawModel`.
fn fields_the_parser_reads() -> BTreeSet<String> {
    let start = RAW
        .find("pub struct RawModel {")
        .expect("RawModel must exist for this fence to mean anything");
    let body = &RAW[start..];
    let end = body.find("\n}").expect("RawModel must be a closed struct");
    let read: BTreeSet<String> = body[..end]
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix("pub ")?;
            // Fields only: the `pub struct RawModel {` line also starts with
            // `pub ` and otherwise arrives as a field named "struct RawModel {".
            if rest.starts_with("struct ") {
                return None;
            }
            let name = rest.split(':').next()?.trim();
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect();

    // Same reason as the document parse: three tests compare against this set,
    // and an empty one makes "the parser reads nothing undocumented" and "no
    // declared-unread field is read" both trivially true.
    assert!(
        read.len() >= 10,
        "the RawModel parse found only {} fields; the struct's shape changed \
         and these fences are comparing against an empty set",
        read.len()
    );
    read
}

#[test]
fn every_measured_field_is_read_or_declared_unread() {
    let documented = fields_in_the_document();
    let read = fields_the_parser_reads();

    let declared: BTreeSet<&str> = DECLARED_UNREAD.iter().map(|(f, _)| *f).collect();
    let unaccounted: Vec<&String> = documented
        .iter()
        .filter(|f| !read.contains(*f) && !declared.contains(f.as_str()))
        .collect();

    assert!(
        unaccounted.is_empty(),
        "the measurement document says the upstream publishes these fields and \
         the parser neither reads them nor declares them unread: {unaccounted:?}\n\n\
         This is the state that shipped the `limit.input` gap: measured on day \
         one, written down, and dropped by serde for three days on 1,199 rows. \
         Either read the field, or add it to DECLARED_UNREAD with the reason. \
         Both are fine; silence is not."
    );
}

#[test]
fn the_document_names_every_field_the_parser_reads() {
    // The reverse direction, which a natural test omits because you write the
    // check FROM the document. §9 of the design note named four tables that
    // never existed and only this direction caught them.
    //
    // A parser field missing from the inventory means the document has stopped
    // describing the payload — the same drift, pointed the other way.
    let documented = fields_in_the_document();
    let read = fields_the_parser_reads();

    let undocumented: Vec<&String> = read.iter().filter(|f| !documented.contains(*f)).collect();
    assert!(
        undocumented.is_empty(),
        "the parser reads these fields and the measurement document's inventory \
         does not name them: {undocumented:?}\n\n\
         Either the document is stale, or the parser is reading something the \
         upstream does not publish. Both are worth knowing, and neither is \
         visible from the side you were looking at."
    );
}

/// The nested shapes the document records, held against the structs that read
/// them.
///
/// # This is the test the other three would have passed
///
/// The inventory row for `limit` reads ``{context, output, input?}`` — the
/// field NAME is `limit`, which the parser reads, so every check above is
/// satisfied while the third key goes unread. The fence built for the
/// `limit.input` gap did not catch `limit.input`.
///
/// Found by pointing the finished fence at its own motivating case, which is
/// the only check that finds this: a guard that passes its own defect is
/// indistinguishable from a guard that works, and re-reading it cannot tell
/// them apart because the code is exactly what its author meant to write.
#[test]
fn every_nested_key_the_document_records_is_read() {
    // (documented field, the struct that must read its keys)
    let nested = [("limit", "RawLimit"), ("modalities", "RawModalities")];

    let start = MEASURED
        .find("## Model object")
        .expect("the Model object section must exist");
    let section = &MEASURED[start..];
    let end = section[3..]
        .find("\n## ")
        .map(|i| i + 3)
        .unwrap_or(section.len());
    let section = &section[..end];

    let mut checked = 0usize;
    for (field, struct_name) in nested {
        // The Notes cell for this row, which is where the shape is recorded.
        let row = section
            .lines()
            .find(|l| l.trim_start().starts_with(&format!("| `{field}`")))
            .unwrap_or_else(|| panic!("the inventory must have a row for {field}"));
        let notes = row.split('|').nth(3).unwrap_or_default();

        // Keys inside the braces, with `?` marking optional ones. An optional
        // key is still a key the upstream sends.
        let Some(open) = notes.find('{') else {
            continue;
        };
        let Some(close) = notes.find('}') else {
            continue;
        };
        let keys: Vec<String> = notes[open + 1..close]
            .split(',')
            .map(|k| {
                k.trim()
                    .trim_end_matches('?')
                    .split(':')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .trim_matches('`')
                    .to_string()
            })
            .filter(|k| !k.is_empty())
            .collect();
        assert!(
            keys.len() >= 2,
            "the shape parse for {field} found {} keys; the notes cell format \
             changed and this check is reading nothing",
            keys.len()
        );

        let decl = format!("pub struct {struct_name} {{");
        let sstart = RAW
            .find(&decl)
            .unwrap_or_else(|| panic!("{struct_name} must exist"));
        let body = &RAW[sstart..];
        let send = body.find("\n}").expect("a closed struct");
        let read: Vec<String> = body[..send]
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                let rest = l.strip_prefix("pub ")?;
                if rest.starts_with("struct ") {
                    return None;
                }
                Some(rest.split(':').next()?.trim().to_string())
            })
            .collect();

        for key in &keys {
            assert!(
                read.iter().any(|r| r == key),
                "the document records `{field}.{key}` and {struct_name} does \
                 not read it. This is the `limit.input` gap verbatim: measured \
                 on day one, written down, and dropped by serde on 1,199 rows \
                 for three days. Serde ignores unknown keys silently, so the \
                 only signal is this test."
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 4,
        "only {checked} nested keys were checked; the parse found nothing and \
         this test would pass on any codebase"
    );
}

/// Keys inside one `experimental.modes.<name>` entry the parser deliberately
/// does not read, each with the reason.
///
/// Kept apart from [`DECLARED_UNREAD`] because `experimental` itself IS read:
/// its modes' `cost` blocks are served as mode rates. What is declared here is
/// the rest of a mode.
const DECLARED_UNREAD_IN_A_MODE: &[(&str, &str)] = &[(
    "provider",
    "the request bytes that switch the mode on: `body` parameters such as \
     `service_tier: priority` or `speed: fast`, and `headers` such as \
     `anthropic-beta`. They would land verbatim in an outbound request, so \
     serving them would make this catalog decide how a consumer's request is \
     spoken. Not a field of `RawMode`, so the bytes are dropped at the parse \
     boundary and nothing downstream can store or serve them.",
)];

/// Every key the measurement document records inside a mode is either read by
/// `RawMode` or declared unread, and no declared key is secretly read.
///
/// The inventory row for `experimental` carries no brace shape for the nested
/// fence above to parse, so the mode's keys are taken from the document's
/// worked example of a mode instead.
#[test]
fn every_key_inside_a_mode_is_read_or_declared_unread() {
    // The worked example in the document is a JSON block under `modes`.
    let start = MEASURED
        .find("\"modes\": {")
        .expect("the measurement document must show a worked example of a mode");
    let example = &MEASURED[start..];
    let example = &example[..example.find("```").expect("the example is fenced")];
    let documented: Vec<&str> = ["cost", "provider"]
        .into_iter()
        .filter(|k| example.contains(&format!("\"{k}\":")))
        .collect();
    assert_eq!(
        documented,
        ["cost", "provider"],
        "the document's mode example no longer shows both keys a mode carries; \
         this check is reading nothing"
    );

    let decl = "pub struct RawMode {";
    let sstart = RAW.find(decl).expect("RawMode must exist");
    let body = &RAW[sstart..];
    let send = body.find("\n}").expect("a closed struct");
    let read: Vec<String> = body[..send]
        .lines()
        .filter_map(|l| {
            let rest = l.trim().strip_prefix("pub ")?;
            if rest.starts_with("struct ") {
                return None;
            }
            Some(rest.split(':').next()?.trim().to_string())
        })
        .collect();

    for key in documented {
        let is_read = read.iter().any(|r| r == key);
        let is_declared = DECLARED_UNREAD_IN_A_MODE.iter().any(|(k, _)| *k == key);
        assert!(
            is_read != is_declared,
            "inside a mode, `{key}` must be exactly one of read by RawMode ({is_read}) \
             or declared unread ({is_declared})"
        );
    }
    assert!(
        read.iter().any(|r| r == "cost"),
        "RawMode must read the mode's cost, which is what mode rates are served from"
    );
}

#[test]
fn a_declared_unread_field_is_actually_unread() {
    // A declaration that has quietly become false is worse than no declaration:
    // it reads as a considered decision while describing the opposite of what
    // the code does.
    let read = fields_the_parser_reads();
    for (field, reason) in DECLARED_UNREAD {
        assert!(
            !read.contains(*field),
            "{field} is declared unread with the reason {reason:?}, but \
             RawModel reads it. Remove the declaration: a stale exemption \
             passes silently and there is no signal when its justification \
             expires."
        );
    }
}
