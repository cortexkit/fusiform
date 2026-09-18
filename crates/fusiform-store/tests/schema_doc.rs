//! The design note's table list matches the schema it describes.
//!
//! # Why a test rather than care
//!
//! §9 of `docs/design/schema-and-store.md` named six tables. Four never
//! existed, and the list omitted `era` — the table holding every fact fusiform
//! serves. It had been a specification written before any code, and it sat
//! under a §0.1 row reading **built**, so nothing in the document told a reader
//! it was a plan.
//!
//! That is ASTRO's 2026-08-13 finding in a different artifact: a note cannot
//! say whether it describes an intention or the shipped system, and the reader
//! has no way to tell. Their prose was true about a crate and false about their
//! runtime; mine was true about a design and false about the database. In both
//! cases re-reading confirms it, because the sentence is well formed and
//! describes something that could exist.
//!
//! "Re-verify the notes more often" is attention, and attention fails on the
//! day nobody is watching. A table list is mechanically checkable, so it is
//! checked.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_store::CatalogStore;

/// Every table a store has once it has been written to.
///
/// Written to, not merely opened: the store crate creates `cortexkit_fence` on
/// the FIRST FENCED WRITE rather than at open, measured with a probe. A fixture
/// that only opens would compare the note against a state no running module is
/// ever in, and would report the fence table as a phantom in the note.
fn tables_in_a_real_store() -> Vec<String> {
    use fusiform_core::{ObservationOutcome, SourceId, Timestamp};
    use fusiform_store::NewObservation;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_string_lossy().to_string(),
        },
    })
    .unwrap();
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1_000),
            outcome: ObservationOutcome::NotModified,
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: Some(1),
            detail: None,
        })
        .unwrap();
    // Drop the store so the lease is released before opening a read connection.
    drop(store);

    let conn = rusqlite::Connection::open(&path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='table' \
             AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap();
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    names
}

#[test]
fn the_design_note_names_the_tables_that_exist() {
    let note = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/design/schema-and-store.md"),
    )
    .expect("the design note is in the repository");

    // The PARAGRAPH under §9 that states the table list, not its first line.
    // The note is hard-wrapped, so the list spans two lines and reading one of
    // them silently drops half the names — which the first version of this test
    // did, reporting a real table as missing from a note that names it.
    let paragraph = note
        .split("\n\n")
        .find(|p| p.trim_start().starts_with("Tables, as shipped:"))
        .expect("§9 states a table list beginning 'Tables, as shipped:'");
    let line = &paragraph.replace('\n', " ");

    // Both directions below are FOR loops, and a for loop over an empty
    // collection asserts nothing at all. So both collections are checked for
    // content first: an extraction that stops matching would otherwise pass this
    // test in silence, which is the same asymmetry that produced seven
    // misreports in scripts/mutate.sh — a failure path returning the same type
    // as the success path, so failure-to-measure arrives dressed as evidence.
    //
    // The floors are measured, not guessed: the store creates five tables
    // (observation, era, catalog_version, cortexkit_schema_version, and
    // cortexkit_fence once a fenced write has happened), and §9 names all of
    // them. Stated as floors so adding a table does not fail a test about
    // extraction.
    let real_tables = tables_in_a_real_store();
    assert!(
        real_tables.len() >= 3,
        "the store enumeration found {} tables, which cannot be right: a query \
         that stopped matching would make every assertion below vacuous. Found: \
         {real_tables:?}",
        real_tables.len()
    );
    let named: Vec<&str> = line.split('`').skip(1).step_by(2).collect();
    assert!(
        named.len() >= 3,
        "the note extraction found {} backticked names in §9. The reverse \
         direction below iterates over these, so an extraction that finds \
         nothing reports no phantom tables while checking none. Paragraph was: \
         {line}",
        named.len()
    );

    for table in tables_in_a_real_store() {
        assert!(
            line.contains(&format!("`{table}`")),
            "the store creates `{table}` and the design note's §9 does not name it.\n\
             Note says: {line}\n\
             A reader takes that list for the schema, and §0.1 marks §9 as built."
        );
    }

    // And the reverse: a name in the note that no longer exists. Without this,
    // a dropped table leaves a phantom in the document forever — which is the
    // half of the defect that was actually present, four times over.
    let real = real_tables;
    for word in named {
        assert!(
            real.iter().any(|t| t == word),
            "the design note's §9 names `{word}`, which the store does not create.\n\
             Real tables: {real:?}"
        );
    }
}

/// Functions the design note names as mechanisms must exist.
///
/// # Why this is a fence rather than a convention
///
/// The note tells a later author where a producer annotation may live and
/// names the function that enforces it: the diff and the digest share one
/// definition of the upstream's claim, `upstream_claim_of`. That sentence is
/// the reason someone will not repeat the incident of 2026-08-16 — and it is
/// prose, which nothing checks.
///
/// This week produced two rots of exactly that shape: a `--polls 60` hint that
/// was true when written and pointed 136 polls short by the time anyone
/// followed it, and a route comment describing the same constant in words.
/// Both were correct at the moment of writing and silently false afterwards.
///
/// So a name the note relies on is held against the source. Renaming the
/// function is fine; renaming it while leaving the note pointing at the old
/// name is what this refuses.
#[test]
fn the_design_note_names_functions_that_exist() {
    let note = include_str!("../../../docs/design/schema-and-store.md");
    let ingest = include_str!("../src/ingest.rs");

    // Named in the note as the shared definition of what the upstream said.
    // One name today; add to this check rather than beside it, so the reason
    // stays attached to the rule.
    let symbol = "upstream_claim_of";
    assert!(
        note.contains(symbol),
        "the note must still name {symbol}, or this test is guarding a \
         sentence nobody wrote"
    );
    assert!(
        ingest.contains(&format!("fn {symbol}")),
        "the design note names `{symbol}` as the mechanism that keeps the diff \
         and the digest agreeing about what the upstream claimed, and no such \
         function exists. Either restore it or correct the note — a reader \
         deciding where to put a new annotation follows that name."
    );
}

/// The artifact table survives migration onto a store that already holds data.
///
/// A migration that only ever runs against an empty fixture is a migration
/// nobody has tested — every real application of it lands on a store with
/// history, and this one lands on 101,904 eras in production.
#[test]
fn the_artifact_table_arrives_on_a_populated_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.db");

    // Build a store at the PREVIOUS schema, with rows in it, then migrate.
    {
        let conn = rusqlite::Connection::open(&path).expect("open");
        for m in &fusiform_store::schema::MIGRATIONS[..3] {
            conn.execute_batch(m.statements).expect("earlier migration");
        }
        conn.execute(
            "INSERT INTO observation (source, observed_at_ms, outcome) \
             VALUES ('models.dev', 1000, 'changed')",
            [],
        )
        .expect("a row that must survive");
    }

    let conn = rusqlite::Connection::open(&path).expect("reopen");
    conn.execute_batch(fusiform_store::schema::MIGRATIONS[3].statements)
        .expect("v4 must apply to a store that already has rows");

    let kept: i64 = conn
        .query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0))
        .expect("the pre-existing row must still be there");
    assert_eq!(kept, 1, "migrating must not disturb existing observations");

    // The table is usable. NOTE the REFERENCES clause is documentation, not
    // enforcement: SQLite defaults `foreign_keys` to OFF, measured rather than
    // assumed, so nothing stops an artifact naming an observation that does not
    // exist. Stated here because the clause reads like a guarantee.
    conn.execute(
        "INSERT INTO observation_artifact (observation_id, reason, recorded_at_ms) \
         VALUES (1, 'docs/findings/2026-08-16-provenance-rewrote-the-rate-plane.md', 2000)",
        [],
    )
    .expect("marking a real observation must work");

    let reason: String = conn
        .query_row(
            "SELECT reason FROM observation_artifact WHERE observation_id = 1",
            [],
            |r| r.get(0),
        )
        .expect("the mark must be readable");
    assert!(
        reason.contains("findings/"),
        "the reason names the evidence rather than summarising it: {reason}"
    );
}

/// A store written by a newer binary is refused rather than served from.
///
/// The migrator reports and does not refuse, deliberately, so that a binary
/// rollback is not bricked. This asserts fusiform's own answer to that report.
///
/// The control matters more than the refusal: without it, a test that only
/// checks the ahead case passes equally well against a store that refuses
/// EVERYTHING, which would be a worse defect than the one being guarded.
#[test]
fn a_store_written_by_a_newer_binary_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let descriptor = StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_string_lossy().to_string(),
        },
    };

    // CONTROL, asserted first: a store this binary wrote opens.
    {
        let store = CatalogStore::open(&descriptor).expect("a store at parity must open");
        drop(store);
    }

    // Now record a chain entry from the future, exactly as a newer binary's
    // migrator would have left it.
    let ahead = fusiform_store::schema::MIGRATIONS
        .last()
        .expect("the chain is not empty")
        .version
        + 1;
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO cortexkit_schema_version (namespace, version, applied_at_unix) \
         VALUES (?1, ?2, 0)",
        rusqlite::params![fusiform_store::schema::NAMESPACE, ahead],
    )
    .unwrap();
    drop(conn);

    let err = CatalogStore::open(&descriptor)
        .err()
        .expect("a store ahead of this binary must be refused");
    let text = err.to_string();
    assert!(
        text.contains(&ahead.to_string()),
        "the refusal must name the store's version so an operator knows which \
         binary to run: {text}"
    );
    assert!(
        text.contains("boundary_kind"),
        "the refusal must say WHY this binary cannot serve the store, not just \
         that it will not: {text}"
    );
}

/// The plan-price table arrives on a populated store, and its constraints hold.
///
/// Same reasoning as the artifact table above: a migration that only ever runs
/// against an empty fixture is untested, because every real application lands
/// on history. Production carries 141,058 eras.
///
/// The constraint arms matter as much as the arrival. This plane's whole
/// defence is that incoherent rows are UNREPRESENTABLE rather than merely
/// unwritten — a priced row carrying a refusal, or a refused row explaining
/// nothing, are exactly what a careless writer produces and a careful one does
/// not. If the CHECKs did not apply, the table would accept both and nothing
/// would say so until a consumer read one.
#[test]
fn the_plan_price_table_arrives_on_a_populated_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.db");

    {
        let conn = rusqlite::Connection::open(&path).expect("open");
        for m in &fusiform_store::schema::MIGRATIONS[..5] {
            conn.execute_batch(m.statements).expect("earlier migration");
        }
        conn.execute(
            "INSERT INTO observation (source, observed_at_ms, outcome) \
             VALUES ('models.dev', 1000, 'changed')",
            [],
        )
        .expect("a row that must survive");
    }

    let conn = rusqlite::Connection::open(&path).expect("reopen");
    conn.execute_batch(fusiform_store::schema::MIGRATIONS[5].statements)
        .expect("v6 must apply to a store that already has rows");

    let kept: i64 = conn
        .query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0))
        .expect("count");
    assert_eq!(kept, 1, "the migration must not disturb existing history");

    // A priced row: all three money parts, no refusal.
    conn.execute(
        "INSERT INTO plan_price_era \
         (provider_id, tier, minor_units, exponent, currency, period, \
          boundary_kind, boundary_at_ms, established_by, established_at_ms, \
          review_by_ms, source_ref) \
         VALUES ('anthropic', 'max_20x', 20000, 2, 'USD', 'month', \
                 'asserted', 1000, 'fusi', 2000, 3000, 'https://example')",
        [],
    )
    .expect("a well-formed priced row must be accepted");

    // A refusal row: no money parts, a reason.
    conn.execute(
        "INSERT INTO plan_price_era \
         (provider_id, tier, period, boundary_kind, boundary_at_ms, \
          established_by, established_at_ms, review_by_ms, source_ref, \
          refusal_reason) \
         VALUES ('openai', 'enterprise', 'month', 'asserted', 1000, \
                 'fusi', 2000, 3000, 'https://example', \
                 'tier observed, no published price')",
        [],
    )
    .expect("a well-formed refusal row must be accepted");

    // And the three incoherent shapes must be REFUSED, or the constraints are
    // decorative. Each is a thing a careless writer produces.
    let priced_with_a_refusal = conn.execute(
        "INSERT INTO plan_price_era \
         (provider_id, tier, minor_units, exponent, currency, period, \
          boundary_kind, boundary_at_ms, established_by, established_at_ms, \
          review_by_ms, source_ref, refusal_reason) \
         VALUES ('x', 'y', 20000, 2, 'USD', 'month', 'asserted', 1000, \
                 'fusi', 2000, 3000, 'https://example', 'why')",
        [],
    );
    assert!(
        priced_with_a_refusal.is_err(),
        "a row cannot both carry a price and explain why it has none"
    );

    let refused_explaining_nothing = conn.execute(
        "INSERT INTO plan_price_era \
         (provider_id, tier, period, boundary_kind, boundary_at_ms, \
          established_by, established_at_ms, review_by_ms, source_ref) \
         VALUES ('x', 'y', 'month', 'asserted', 1000, 'fusi', 2000, 3000, \
                 'https://example')",
        [],
    );
    assert!(
        refused_explaining_nothing.is_err(),
        "an absent price must say WHICH absence it is: 'not observed' and \
         'observed and unpriced' are different states and only one is work"
    );

    let half_an_amount = conn.execute(
        "INSERT INTO plan_price_era \
         (provider_id, tier, minor_units, period, boundary_kind, \
          boundary_at_ms, established_by, established_at_ms, review_by_ms, \
          source_ref) \
         VALUES ('x', 'y', 20000, 'month', 'asserted', 1000, 'fusi', 2000, \
                 3000, 'https://example')",
        [],
    );
    assert!(
        half_an_amount.is_err(),
        "a value without its exponent and currency is a number, not an amount"
    );

    // An observed boundary is a CATEGORY error here, not a data error: every
    // row in this plane is a date a vendor stated with no fetch behind it.
    let observed = conn.execute(
        "INSERT INTO plan_price_era \
         (provider_id, tier, period, boundary_kind, boundary_at_ms, \
          established_by, established_at_ms, review_by_ms, source_ref, \
          refusal_reason) \
         VALUES ('x', 'y', 'month', 'observed', 1000, 'fusi', 2000, 3000, \
                 'https://example', 'why')",
        [],
    );
    assert!(
        observed.is_err(),
        "this plane has no observed boundaries; nothing here is fetched"
    );
}
