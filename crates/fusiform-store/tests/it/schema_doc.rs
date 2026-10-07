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

fn table_list(note: &str) -> String {
    note.split("\n\n")
        .find(|p| p.trim_start().starts_with("Tables, as shipped:"))
        .expect("§9 states a table list beginning 'Tables, as shipped:'")
        .replace('\n', " ")
}

fn table_names(line: &str) -> Vec<&str> {
    line.split('`').skip(1).step_by(2).collect()
}

fn table_list_violations(note: &str, real: &[String]) -> Vec<String> {
    let line = &table_list(note);
    let named = table_names(line);
    let mut violations = Vec::new();
    for table in real {
        if !line.contains(&format!("`{table}`")) {
            violations.push(format!("undocumented table: {table}"));
        }
    }
    for word in named {
        if !real.iter().any(|t| t == word) {
            violations.push(format!("phantom table: {word}"));
        }
    }
    violations
}

fn missing_documented_functions(note: &str, source: &str, symbols: &[&str]) -> Vec<String> {
    symbols
        .iter()
        .filter(|symbol| note.contains(**symbol) && !source.contains(&format!("fn {symbol}")))
        .map(|symbol| (*symbol).to_string())
        .collect()
}

#[test]
fn planted_table_list_drift_is_reported() {
    assert_eq!(
        table_list_violations(
            "Tables, as shipped: `real`.",
            &["real".into(), "lost".into()]
        ),
        ["undocumented table: lost"]
    );
    assert_eq!(
        table_list_violations("Tables, as shipped: `real`, `ghost`.", &["real".into()]),
        ["phantom table: ghost"]
    );
}

#[test]
fn planted_nonexistent_documented_function_is_reported() {
    assert_eq!(
        missing_documented_functions("Use `lost`.", "fn other() {}", &["lost"]),
        ["lost"]
    );
}

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
    let line = &table_list(&note);

    // Both directions below are FOR loops, and a for loop over an empty
    // collection asserts nothing at all. So both collections are checked for
    // content first: an extraction that stops matching would otherwise pass this
    // test in silence, which is the same asymmetry that produced seven
    // misreports in this repository's former mutation script (since replaced by
    // `ckdev-mutate`) — a failure path returning the same type as the success
    // path, so failure-to-measure arrives dressed as evidence.
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
    let named = table_names(line);
    assert!(
        named.len() >= 3,
        "the note extraction found {} backticked names in §9. The reverse \
         direction below iterates over these, so an extraction that finds \
         nothing reports no phantom tables while checking none. Paragraph was: \
         {line}",
        named.len()
    );

    let violations = table_list_violations(&note, &real_tables);
    for table in &real_tables {
        assert!(
            !violations.contains(&format!("undocumented table: {table}")),
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
            !violations.contains(&format!("phantom table: {word}")),
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
    let note = include_str!("../../../../docs/design/schema-and-store.md");
    let ingest = include_str!("../../src/ingest.rs");

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
        missing_documented_functions(note, ingest, &[symbol]).is_empty(),
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
         (provider_id, tier, boundary_kind, boundary_at_ms, \
          established_by, established_at_ms, review_by_ms, source_ref, \
          refusal_reason) \
         VALUES ('openai', 'enterprise', 'asserted', 1000, \
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
         (provider_id, tier, boundary_kind, boundary_at_ms, \
          established_by, established_at_ms, review_by_ms, source_ref) \
         VALUES ('x', 'y', 'asserted', 1000, 'fusi', 2000, 3000, \
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

    // A CORRECTION: the same key at the same vendor instant, a different value.
    //
    // This is the case a unique index would have made unrepresentable. Correcting
    // a mistyped price does not change the vendor's effective date — it changes
    // fusiform's reading of it — so the corrected row carries the SAME boundary.
    // Refusing it would force either an in-place UPDATE, which this store does
    // not do, or a falsified boundary chosen to dodge the constraint.
    conn.execute(
        "INSERT INTO plan_price_era \
         (provider_id, tier, minor_units, exponent, currency, period, \
          boundary_kind, boundary_at_ms, established_by, established_at_ms, \
          review_by_ms, source_ref) \
         VALUES ('anthropic', 'max_20x', 20000, 2, 'USD', 'month', \
                 'asserted', 1000, 'fusi', 4000, 5000, 'https://example')",
        [],
    )
    .expect(
        "a correction at the same boundary must be accepted, or a mistyped \
             price can only be fixed by falsifying the vendor's date",
    );

    // And supersession is by insertion order, so the newest row is the one in
    // force. Without this arm the insert above proves only that the constraint
    // is gone, not that the correction can be READ back.
    let (units, by): (i64, String) = conn
        .query_row(
            "SELECT minor_units, established_by FROM plan_price_era \
             WHERE provider_id='anthropic' AND tier='max_20x' \
               AND boundary_at_ms <= 1000 \
             ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the corrected row must be readable");
    assert_eq!(units, 20000);
    assert_eq!(
        by, "fusi",
        "the newest row at or before the instant is the one in force"
    );

    // A refusal carrying a period is refused, because period is part of the
    // money group: a row with no price has no period, and a stored "month"
    // beside a NULL price is a claim nobody made.
    let refusal_with_a_period = conn.execute(
        "INSERT INTO plan_price_era \
         (provider_id, tier, period, boundary_kind, boundary_at_ms, \
          established_by, established_at_ms, review_by_ms, source_ref, \
          refusal_reason) \
         VALUES ('x', 'y', 'month', 'asserted', 1000, 'fusi', 2000, 3000, \
                 'https://example', 'why')",
        [],
    );
    assert!(
        refusal_with_a_period.is_err(),
        "a refusal has no price and therefore no period; a defaulted one is \
         indistinguishable from a period the source stated"
    );

    // An observed boundary is a CATEGORY error here, not a data error: every
    // row in this plane is a date a vendor stated with no fetch behind it.
    let observed = conn.execute(
        "INSERT INTO plan_price_era \
         (provider_id, tier, boundary_kind, boundary_at_ms, \
          established_by, established_at_ms, review_by_ms, source_ref, \
          refusal_reason) \
         VALUES ('x', 'y', 'observed', 1000, 'fusi', 2000, 3000, \
                 'https://example', 'why')",
        [],
    );
    assert!(
        observed.is_err(),
        "this plane has no observed boundaries; nothing here is fetched"
    );
}

/// Re-applying the same curated file writes nothing the second time.
///
/// The file is compiled in and applied on every startup, so a blind insert
/// would write the whole file again on each boot: the table would grow without
/// bound and every row would read as repriced on each restart. That is the
/// August phantom-era failure in different clothes — a write path recording
/// fusiform's own repetition as though the world had changed.
///
/// The third arm is the one that makes this a test rather than a demonstration:
/// a CHANGED price must still write, or an ingest that never writes anything
/// passes the first two.
#[test]
fn re_applying_the_curated_file_is_a_no_op() {
    let dir = tempfile::tempdir().expect("tempdir");
    let descriptor = cortexkit_store_types::StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: cortexkit_store_types::Isolation::Module,
        backend: cortexkit_store_types::StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    };
    let store = fusiform_store::CatalogStore::open(&descriptor).expect("open");

    let row = |units: Option<i64>, by: &str| fusiform_store::NewPlanPrice {
        provider_id: "anthropic".to_string(),
        tier: "max_20x".to_string(),
        minor_units: units,
        exponent: units.map(|_| 2),
        currency: units.map(|_| "USD".to_string()),
        period: units.map(|_| "month".to_string()),
        boundary_at_ms: 1_000,
        established_by: by.to_string(),
        established_at_ms: 2_000,
        review_by_ms: 3_000,
        source_ref: "https://example".to_string(),
        refusal_reason: units.is_none().then(|| "no published price".to_string()),
    };

    let first = store
        .append_plan_prices(&[row(Some(20000), "fusi")])
        .expect("first apply");
    assert_eq!(first, 1, "a new row must be written");

    // Same claim, DIFFERENT provenance: a re-read that confirmed the price.
    // This must not write, or confirming a price would be indistinguishable
    // from a reprice in the row's own history.
    let again = store
        .append_plan_prices(&[row(Some(20000), "someone-else")])
        .expect("second apply");
    assert_eq!(
        again, 0,
        "re-reading a page and confirming the same number is not a change; \
         recording it would make a review look like a reprice"
    );

    // CONTROL: a real change still writes. Without this the two assertions
    // above pass against an ingest that writes nothing at all.
    let moved = store
        .append_plan_prices(&[row(Some(25000), "fusi")])
        .expect("third apply");
    assert_eq!(moved, 1, "a changed price must be recorded");

    // And a refusal replacing a price is a change too: the vendor withdrawing a
    // published figure is exactly the event this plane exists to notice.
    let withdrawn = store
        .append_plan_prices(&[row(None, "fusi")])
        .expect("fourth apply");
    assert_eq!(withdrawn, 1, "a price becoming unpublished is a change");
}

/// A key dropped from the curated file is TOMBSTONED, not left serving.
///
/// # The defect this replaced, reproduced before it was fixed
///
/// `append_plan_prices` iterates the FILE'S rows, so a key present in the store
/// and absent from the file was never visited: its last row stayed in force
/// with its price intact, indistinguishable from a curated one. The review gate
/// reads the FILE, so that key's date was no longer checked by anything either.
///
/// Which is the worst failure this plane has. Its premise is that curated data
/// has no fetch, no diff and no possible contradiction, so a review date is the
/// ONLY liveness signal it can carry. A row that escapes the gate is a price
/// that can be wrong forever with nothing able to notice — and deleting a cell
/// is the obvious way to retire a tier.
///
/// The defect was asserted here first and this test watched it hold, because a
/// fix whose defect was never reproduced is a fix for a described problem.
#[test]
fn a_key_dropped_from_the_file_is_tombstoned() {
    let dir = tempfile::tempdir().expect("tempdir");
    let descriptor = cortexkit_store_types::StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: cortexkit_store_types::Isolation::Module,
        backend: cortexkit_store_types::StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    };
    let store = fusiform_store::CatalogStore::open(&descriptor).expect("open");

    let row = |tier: &str, units: i64| fusiform_store::NewPlanPrice {
        provider_id: "acme".to_string(),
        tier: tier.to_string(),
        minor_units: Some(units),
        exponent: Some(2),
        currency: Some("USD".to_string()),
        period: Some("month".to_string()),
        boundary_at_ms: 1_000,
        established_by: "fusi".to_string(),
        established_at_ms: 2_000,
        review_by_ms: 3_000,
        source_ref: "https://example".to_string(),
        refusal_reason: None,
    };

    store
        .append_plan_prices(&[row("pro", 2000), row("retired_tier", 5000)])
        .expect("first apply");

    // The file now carries only one of them — someone retired a tier by
    // deleting its cell, which is the obvious way to do it.
    let second = store
        .append_plan_prices(&[row("pro", 2000)])
        .expect("second apply");
    assert_eq!(second, 1, "the dropped key must be tombstoned, and only it");

    let served = store.plan_prices_at(9_999_999, None).expect("read");

    // STILL A ROW, because a refusal is a row here: a consumer querying the
    // retired tier gets a reason rather than a no_coverage refusal that sends
    // them hunting a curation gap.
    let stale = served
        .iter()
        .find(|r| r.tier == "retired_tier")
        .expect("the dropped key still answers, with a reason");
    assert_eq!(
        stale.minor_units, None,
        "but carries NO price — the defect was that its old price stood"
    );
    assert!(
        stale
            .refusal_reason
            .as_deref()
            .unwrap_or_default()
            .contains("no longer carried"),
        "and says why: {:?}",
        stale.refusal_reason
    );

    // CONTROL: the curated key is untouched. Without this the assertions above
    // pass against an apply that tombstones everything.
    let live = served
        .iter()
        .find(|r| r.tier == "pro")
        .expect("still curated");
    assert_eq!(live.minor_units, Some(2000));
    assert!(live.refusal_reason.is_none());

    // And a third apply writes NOTHING. Without this the table gains a
    // tombstone on every boot — the phantom-era failure inside the fix for the
    // stale-row one.
    let third = store
        .append_plan_prices(&[row("pro", 2000)])
        .expect("third apply");
    assert_eq!(third, 0, "re-application must stay a no-op");
}
