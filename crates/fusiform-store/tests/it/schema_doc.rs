//! The data-model doc's table list matches the schema it describes.
//!
//! The public statement of the schema is `docs/data-model.md`. Its paragraph
//! beginning "Tables, as shipped:" is held against a real store in both
//! directions, and the functions it names as mechanisms are held against the
//! source.
//!
//! # Why a test rather than care
//!
//! An earlier design document named six tables. Four never existed, and the
//! list omitted `era` — the table holding every fact fusiform serves. It had
//! been a specification written before any code, and it sat under a status row
//! reading **built**, so nothing in the document told a reader it was a plan.
//!
//! A document cannot say whether it describes an intention or the shipped
//! system, and the reader has no way to tell. Re-reading confirms it either
//! way, because the sentence is well formed and describes something that could
//! exist.
//!
//! "Re-verify the notes more often" is attention, and attention fails on the
//! day nobody is watching. A table list is mechanically checkable, so it is
//! checked.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_store::CatalogStore;

#[test]
fn quality_grade_v8_import_preserves_exact_rows_on_populated_v7() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for migration in &fusiform_store::schema::MIGRATIONS[..7] {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn.execute("INSERT INTO observation (source, observed_at_ms, outcome) VALUES ('models.dev', 1000, 'changed')", []).unwrap();
    let migration = fusiform_store::schema::MIGRATIONS
        .get(7)
        .expect("quality grades require v8");
    conn.execute_batch(migration.statements).unwrap();
    let snapshot: serde_json::Value =
        serde_json::from_str(include_str!("../../src/quality-v8.json")).unwrap();
    let mut stmt = conn.prepare("SELECT model, elo, eq, withdrawn, graded_by, reason, recorded_at_ms FROM quality_grade_era ORDER BY model").unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), snapshot["rows"].as_array().unwrap().len());
    for (row, expected) in rows.iter().zip(snapshot["rows"].as_array().unwrap()) {
        assert_eq!(row.0, expected["model"].as_str().unwrap());
        assert_eq!(row.1, expected["elo"].to_string());
        assert_eq!(row.2, expected["eq"].to_string());
        assert!(!row.3);
        assert_eq!(row.4, "operator");
        assert!(row.5.contains("import"));
        assert_eq!(row.6, snapshot["graded_at_ms"].as_i64().unwrap());
    }
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM observation", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    // The JSON operators must retain decimal tokens, not convert through REAL.
    assert_eq!(
        conn.query_row("SELECT '{\"elo\":1599.800}' -> 'elo'", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "1599.800"
    );
    for (elo, eq, withdrawn) in [
        (Some("1"), None, 0),
        (None, None, 0),
        (Some("1"), Some("2"), 1),
        (None, None, 2),
    ] {
        assert!(conn.execute("INSERT INTO quality_grade_era (model, elo, eq, withdrawn, graded_by, reason, recorded_at_ms) VALUES ('x/y', ?1, ?2, ?3, 'operator', 'invalid', 1)", rusqlite::params![elo, eq, withdrawn]).is_err());
    }
}

#[test]
fn quality_grade_v8_reopen_never_reimports_and_v7_reports_store_ahead() {
    let dir = tempfile::tempdir().unwrap();
    let descriptor = StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("grades.db").to_string_lossy().into(),
        },
    };
    {
        let old = cortexkit_store::open_sqlite(&descriptor).unwrap();
        old.migrate(
            fusiform_store::schema::NAMESPACE,
            &fusiform_store::schema::MIGRATIONS[..7],
        )
        .unwrap();
        old.with_conn_fenced(|tx| tx.execute("INSERT INTO observation (source, observed_at_ms, outcome) VALUES ('models.dev', 1000, 'changed')", [])).unwrap();
    }
    let snapshot: serde_json::Value =
        serde_json::from_str(include_str!("../../src/quality-v8.json")).unwrap();
    let count = snapshot["rows"].as_array().unwrap().len();
    let watermark;
    {
        let store = CatalogStore::open(&descriptor).unwrap();
        assert_eq!(store.quality_grades().unwrap().len(), count);
        assert_eq!(
            store.catalog_version().unwrap(),
            0,
            "migration import is not a live grade write"
        );
        let grade = fusiform_store::QualityGrade::Chat {
            elo: serde_json::from_str("1599.800").unwrap(),
            eq: serde_json::from_str("1200.00").unwrap(),
        };
        store
            .append_quality_grade(
                "openai/gpt-4",
                Some(&grade),
                "operator",
                "new evaluation",
                5000,
            )
            .unwrap();
        assert_eq!(store.catalog_version().unwrap(), 5000);
        store
            .append_quality_grade("openai/gpt-4", None, "operator", "withdrawn", 1)
            .unwrap();
        assert_eq!(
            store.catalog_version().unwrap(),
            5001,
            "a backwards clock must not rewind the version"
        );
        watermark = store.latest_quality_grade_id().unwrap();
    }
    for _ in 0..2 {
        let store = CatalogStore::open(&descriptor).unwrap();
        assert_eq!(store.latest_quality_grade_id().unwrap(), watermark);
        assert_eq!(store.quality_grades().unwrap().len(), count - 1);
        let history = store.quality_grade_history("openai/gpt-4").unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(
            history[1]
                .grade
                .as_ref()
                .unwrap()
                .elo()
                .unwrap()
                .to_string(),
            "1599.800"
        );
        assert!(
            history[2].grade.is_none(),
            "reopening must not restore a withdrawn grade"
        );
    }
    let old = cortexkit_store::open_sqlite(&descriptor).unwrap();
    let outcome = old
        .migrate(
            fusiform_store::schema::NAMESPACE,
            &fusiform_store::schema::MIGRATIONS[..7],
        )
        .unwrap();
    assert!(
        outcome.store_ahead(),
        "the v7 binary's existing refusal guard must reject a newer store"
    );
    assert_eq!(outcome.recorded, 10);
    assert_eq!(outcome.chain_max, 7);
}

#[test]
fn fact_override_v9_migrates_populated_v8_and_older_binaries_refuse_it() {
    let dir = tempfile::tempdir().unwrap();
    let descriptor = StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into(),
        },
    };
    {
        let old = cortexkit_store::open_sqlite(&descriptor).unwrap();
        old.migrate(
            fusiform_store::schema::NAMESPACE,
            &fusiform_store::schema::MIGRATIONS[..8],
        )
        .unwrap();
        old.with_conn_fenced(|tx| tx.execute_batch(r#"
            INSERT INTO observation (source, observed_at_ms, outcome) VALUES ('models.dev',1000,'seeded');
            INSERT INTO era (source, provider_id, model_id, fact_key, value_json, boundary_at_ms, boundary_kind)
                VALUES ('models.dev','p','m','existence','"present"',1000,'seed');
            INSERT INTO plan_price_era (provider_id,tier,minor_units,exponent,currency,period,boundary_kind,
                boundary_at_ms,established_by,established_at_ms,review_by_ms,source_ref)
                VALUES ('p','tier',100,2,'USD','month','asserted',1000,'fusiform',1000,2000,'source');
            INSERT INTO curated_model_era (provider_id,model_id,boundary_at_ms,facts_json,auth_methods_json,
                source_ref,established_by,established_at_ms,review_by_ms)
                VALUES ('p','curated',1000,'{"existence":"present"}','["apikey"]','source','fusiform',1000,2000);
        "#)).unwrap();
    }
    let path = dir.path().join("store.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    let tables = [
        "observation",
        "era",
        "plan_price_era",
        "curated_model_era",
        "quality_grade_era",
    ];
    let snapshot = |table: &&str| {
        let columns = if *table == "quality_grade_era" {
            "id, model, elo, eq, withdrawn, graded_by, reason, recorded_at_ms"
        } else {
            "*"
        };
        let mut stmt = conn
            .prepare(&format!("SELECT {columns} FROM {table} ORDER BY id"))
            .unwrap();
        let columns = stmt.column_count();
        stmt.query_map([], |r| {
            (0..columns)
                .map(|i| r.get::<_, rusqlite::types::Value>(i))
                .collect::<Result<Vec<_>, _>>()
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    let before: Vec<_> = tables.iter().map(&snapshot).collect();
    {
        let store = CatalogStore::open(&descriptor).unwrap();
        assert!(store.fact_overrides_at(i64::MAX, None).unwrap().is_empty());
        assert_eq!(store.catalog_version().unwrap(), 0);
    }
    for (table, expected) in tables.iter().zip(before) {
        assert_eq!(snapshot(table), expected, "v9 must preserve {table}");
    }
    let value: String = conn
        .query_row("SELECT value_json FROM era", [], |r| r.get(0))
        .unwrap();
    assert_eq!(value, "\"present\"");
    drop(conn);
    let old = cortexkit_store::open_sqlite(&descriptor).unwrap();
    let outcome = old
        .migrate(
            fusiform_store::schema::NAMESPACE,
            &fusiform_store::schema::MIGRATIONS[..8],
        )
        .unwrap();
    assert!(
        outcome.store_ahead(),
        "the existing v8 refusal guard must reject a newer store"
    );
    assert_eq!((outcome.recorded, outcome.chain_max), (10, 8));
    let outcome = old
        .migrate(
            fusiform_store::schema::NAMESPACE,
            &fusiform_store::schema::MIGRATIONS[..9],
        )
        .unwrap();
    assert!(outcome.store_ahead(), "v9 must also refuse a v10 store");
    assert_eq!((outcome.recorded, outcome.chain_max), (10, 9));
}

#[test]
fn quality_grade_v10_preserves_populated_v8_and_checks_exclusive_dimensions() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for migration in &fusiform_store::schema::MIGRATIONS[..8] {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn.execute("INSERT INTO quality_grade_era (model, elo, eq, withdrawn, graded_by, reason, recorded_at_ms) VALUES ('test/exact', '1599.800', '1200.00', 0, 'operator', 'exact tokens', 1)", []).unwrap();
    conn.execute("INSERT INTO quality_grade_era (model, withdrawn, graded_by, reason, recorded_at_ms) VALUES ('test/exact', 1, 'operator', 'withdrawn', 2)", []).unwrap();
    let snapshot = || {
        conn.prepare("SELECT id, model, elo, eq, withdrawn, graded_by, reason, recorded_at_ms FROM quality_grade_era ORDER BY id").unwrap()
            .query_map([], |row| (0..8).map(|i| row.get::<_, rusqlite::types::Value>(i)).collect::<Result<Vec<_>, _>>()).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap()
    };
    let before = snapshot();
    for migration in &fusiform_store::schema::MIGRATIONS[8..] {
        conn.execute_batch(migration.statements).unwrap();
    }
    assert_eq!(snapshot(), before);
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM quality_grade_era WHERE grade IS NOT NULL",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    for (elo, eq, grade, withdrawn) in [
        (None, None, None, 0),
        (Some("1"), Some("2"), Some("3"), 0),
        (Some("1"), None, Some("3"), 0),
        (None, Some("2"), Some("3"), 0),
        (Some("1"), None, None, 0),
        (None, Some("2"), None, 0),
        (None, None, Some("3"), 1),
        (Some("1"), Some("2"), None, 1),
        (None, None, None, 2),
    ] {
        assert!(conn.execute("INSERT INTO quality_grade_era (model, elo, eq, grade, withdrawn, graded_by, reason, recorded_at_ms) VALUES ('test/invalid', ?1, ?2, ?3, ?4, 'operator', 'invalid', 3)", rusqlite::params![elo, eq, grade, withdrawn]).is_err());
    }
    conn.execute("INSERT INTO quality_grade_era (model, grade, withdrawn, graded_by, reason, recorded_at_ms) VALUES ('test/media', '87.500', 0, 'operator', 'media', 3)", []).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT grade FROM quality_grade_era WHERE model='test/media'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "87.500"
    );
}

#[test]
fn fact_override_v9_constraints_reject_partial_claims_and_tombstones() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    for m in fusiform_store::schema::MIGRATIONS {
        conn.execute_batch(m.statements).unwrap();
    }
    let insert = |value: Option<&str>,
                  replaces: Option<&str>,
                  reason: Option<&str>,
                  quote: Option<&str>,
                  review: Option<i64>,
                  tombstone: Option<&str>,
                  notes: Option<&str>| {
        conn.execute("INSERT INTO fact_override_era (provider_id,model_id,fact_key,boundary_at_ms,value_json,
            replaces_json,reason,source_ref,quote,notes,established_by,established_at_ms,review_by_ms,tombstone_reason)
            VALUES ('p','m','limit.output',1000,?1,?2,?3,'source',?4,?7,'fusiform',1000,?5,?6)",
            rusqlite::params![value,replaces,reason,quote,review,tombstone,notes])
    };
    assert!(insert(
        Some("2"),
        Some("1"),
        Some("reason"),
        Some("quote"),
        Some(2000),
        None,
        None
    )
    .is_ok());
    assert!(insert(
        Some("2"),
        None,
        Some("reason"),
        Some("quote"),
        Some(2000),
        None,
        None
    )
    .is_ok());
    assert!(insert(None, None, None, None, None, Some("removed"), None).is_ok());
    for (value, replaces, reason, quote, review, tombstone, notes) in [
        (
            Some("2"),
            None,
            Some("r"),
            Some("q"),
            Some(2000),
            Some("removed"),
            None,
        ),
        (None, None, None, None, None, None, None),
        (Some("2"), None, None, Some("q"), Some(2000), None, None),
        (Some("2"), None, Some("r"), None, Some(2000), None, None),
        (Some("2"), None, Some("r"), Some("q"), None, None, None),
        (None, Some("1"), None, None, None, Some("removed"), None),
        (None, None, None, None, None, Some("removed"), Some("notes")),
    ] {
        assert!(insert(value, replaces, reason, quote, review, tombstone, notes).is_err());
    }
}

#[test]
fn quality_grade_and_version_commit_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("grades.db");
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_string_lossy().into(),
        },
    })
    .unwrap();
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("CREATE TRIGGER refuse_version BEFORE UPDATE ON catalog_version BEGIN SELECT RAISE(ABORT, 'version refused'); END;").unwrap();
    let grade = fusiform_store::QualityGrade::Chat {
        elo: serde_json::from_str("1").unwrap(),
        eq: serde_json::from_str("2").unwrap(),
    };
    assert!(store
        .append_quality_grade("test/model", Some(&grade), "operator", "evaluation", 5000)
        .is_err());
    assert!(store
        .quality_grade_history("test/model")
        .unwrap()
        .is_empty());
    assert_eq!(store.catalog_version().unwrap(), 0);
}

fn table_list(note: &str) -> String {
    note.split("\n\n")
        .find(|p| p.trim_start().starts_with("Tables, as shipped:"))
        .expect("docs/data-model.md states a table list beginning 'Tables, as shipped:'")
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
/// that only opens would compare the doc against a state no running module is
/// ever in, and would report the fence table as a phantom in the doc.
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

// Named for the design note this list used to live in; the table list now
// lives in docs/data-model.md.
#[test]
fn the_design_note_names_the_tables_that_exist() {
    let note = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/data-model.md"),
    )
    .expect("the data-model doc is in the repository");

    // The PARAGRAPH that states the table list, not its first line. The doc is
    // hard-wrapped, so the list spans several lines and reading one of them
    // silently drops names — which the first version of this test did,
    // reporting a real table as missing from a document that names it.
    let line = &table_list(&note);

    // Both directions below are FOR loops, and a for loop over an empty
    // collection asserts nothing at all. So both collections are checked for
    // content first: an extraction that stops matching would otherwise pass this
    // test in silence, which is the same asymmetry that produced seven
    // misreports in this repository's former mutation script: a failure path
    // returning the same type as the success path, so failure-to-measure
    // arrives dressed as evidence.
    //
    // The floors are deliberately low: the store creates eight tables
    // (observation, era, catalog_version, observation_artifact_event,
    // plan_price_era, curated_model_era, cortexkit_schema_version, and
    // cortexkit_fence once a fenced write has happened), and the doc names all
    // of them. Stated as floors so adding a table does not fail a test about
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
        "the doc extraction found {} backticked names in the table list. The reverse \
         direction below iterates over these, so an extraction that finds \
         nothing reports no phantom tables while checking none. Paragraph was: \
         {line}",
        named.len()
    );

    let violations = table_list_violations(&note, &real_tables);
    for table in &real_tables {
        assert!(
            !violations.contains(&format!("undocumented table: {table}")),
            "the store creates `{table}` and docs/data-model.md does not name it.\n\
             Doc says: {line}\n\
             A reader takes that list for the schema."
        );
    }

    // And the reverse: a name in the doc that no longer exists. Without this,
    // a dropped table leaves a phantom in the document forever — which is the
    // half of the defect that was actually present, four times over.
    let real = real_tables;
    for word in named {
        assert!(
            !violations.contains(&format!("phantom table: {word}")),
            "docs/data-model.md names `{word}` as a table, which the store does not create.\n\
             Real tables: {real:?}"
        );
    }
}

/// Functions the data-model doc names as mechanisms must exist.
///
/// # Why this is a fence rather than a convention
///
/// The doc tells a later author where a producer annotation may live and
/// names the function that enforces it: the diff and the digest share one
/// definition of the upstream's claim, `upstream_claim_of`. That sentence is
/// the reason someone will not repeat the incident of 2026-08-16 — and it is
/// prose, which nothing checks.
///
/// The same week produced two rots of exactly that shape: a `--polls 60` hint
/// that was true when written and pointed 136 polls short by the time anyone
/// followed it, and a route comment describing the same constant in words.
/// Both were correct at the moment of writing and silently false afterwards.
///
/// So a name the doc relies on is held against the source. Renaming the
/// function is fine; renaming it while leaving the doc pointing at the old
/// name is what this refuses.
#[test]
fn the_design_note_names_functions_that_exist() {
    let note = include_str!("../../../../docs/data-model.md");
    let ingest = include_str!("../../src/ingest.rs");

    // Named in the doc as the shared definition of what the upstream said.
    // One name today; add to this check rather than beside it, so the reason
    // stays attached to the rule.
    let symbol = "upstream_claim_of";
    assert!(
        note.contains(symbol),
        "docs/data-model.md must still name {symbol}, or this test is guarding a \
         sentence nobody wrote"
    );
    assert!(
        missing_documented_functions(note, ingest, &[symbol]).is_empty(),
        "docs/data-model.md names `{symbol}` as the mechanism that keeps the diff \
         and the digest agreeing about what the upstream claimed, and no such \
         function exists. Either restore it or correct the doc — a reader \
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

/// The curated model history table arrives on a populated store at the
/// previous version, and its constraints hold.
///
/// Same reasoning as the plan-price table above: every real application of
/// this migration lands on a store with years of history, so it is tested on
/// one that has rows in the tables before it, including the plan-price table
/// the previous migration added.
#[test]
fn the_curated_model_table_arrives_on_a_populated_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.db");

    {
        let conn = rusqlite::Connection::open(&path).expect("open");
        for m in &fusiform_store::schema::MIGRATIONS[..6] {
            conn.execute_batch(m.statements).expect("earlier migration");
        }
        conn.execute(
            "INSERT INTO observation (source, observed_at_ms, outcome) \
             VALUES ('models.dev', 1000, 'seeded')",
            [],
        )
        .expect("an observation that must survive");
        conn.execute(
            "INSERT INTO era (source, provider_id, model_id, fact_key, value_json, \
                              boundary_at_ms, boundary_kind) \
             VALUES ('models.dev', 'meta', 'm', 'existence', '\"present\"', 1000, 'seed')",
            [],
        )
        .expect("an era that must survive");
        conn.execute(
            "INSERT INTO plan_price_era \
             (provider_id, tier, minor_units, exponent, currency, period, \
              boundary_kind, boundary_at_ms, established_by, established_at_ms, \
              review_by_ms, source_ref) \
             VALUES ('anthropic', 'max_20x', 20000, 2, 'USD', 'month', \
                     'asserted', 1000, 'fusi', 2000, 3000, 'https://example')",
            [],
        )
        .expect("a plan price that must survive");
    }

    let conn = rusqlite::Connection::open(&path).expect("reopen");
    assert_eq!(
        fusiform_store::schema::MIGRATIONS[6].version,
        7,
        "this test applies the curated model migration"
    );
    conn.execute_batch(fusiform_store::schema::MIGRATIONS[6].statements)
        .expect("v7 must apply to a store that already has rows");

    for (table, expected) in [("observation", 1), ("era", 1), ("plan_price_era", 1)] {
        let kept: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .expect("count");
        assert_eq!(kept, expected, "the migration must not disturb {table}");
    }
    let value: String = conn
        .query_row("SELECT value_json FROM era", [], |r| r.get(0))
        .expect("the era is readable");
    assert_eq!(value, "\"present\"");

    // A claim: facts, login methods and a review date, no reason.
    conn.execute(
        "INSERT INTO curated_model_era \
         (provider_id, model_id, boundary_at_ms, facts_json, auth_methods_json, \
          source_ref, established_by, established_at_ms, review_by_ms) \
         VALUES ('typesafe', 'jev', 1000, '{\"existence\":\"present\"}', '[\"apikey\"]', \
                 'https://example', 'fusiform', 1000, 2000)",
        [],
    )
    .expect("a well-formed claim must be accepted");
    // A tombstone: a reason and none of the three.
    conn.execute(
        "INSERT INTO curated_model_era \
         (provider_id, model_id, boundary_at_ms, source_ref, established_by, \
          established_at_ms, tombstone_reason) \
         VALUES ('typesafe', 'jev', 3000, 'file', 'fusiform', 3000, 'removed')",
        [],
    )
    .expect("a well-formed tombstone must be accepted");

    // Each half-written shape is refused, or the constraints are decorative.
    for (shape, sql) in [
        (
            "a claim carrying a tombstone reason",
            "INSERT INTO curated_model_era \
             (provider_id, model_id, boundary_at_ms, facts_json, auth_methods_json, \
              source_ref, established_at_ms, review_by_ms, tombstone_reason) \
             VALUES ('x', 'y', 1, '{}', '[]', 's', 1, 2, 'why')",
        ),
        (
            "a row with neither a claim nor a reason",
            "INSERT INTO curated_model_era \
             (provider_id, model_id, boundary_at_ms, source_ref, established_at_ms) \
             VALUES ('x', 'y', 1, 's', 1)",
        ),
        (
            "a claim without its login methods",
            "INSERT INTO curated_model_era \
             (provider_id, model_id, boundary_at_ms, facts_json, source_ref, \
              established_at_ms, review_by_ms) \
             VALUES ('x', 'y', 1, '{}', 's', 1, 2)",
        ),
        (
            "a claim without its review date",
            "INSERT INTO curated_model_era \
             (provider_id, model_id, boundary_at_ms, facts_json, auth_methods_json, \
              source_ref, established_at_ms) \
             VALUES ('x', 'y', 1, '{}', '[]', 's', 1)",
        ),
    ] {
        assert!(conn.execute(sql, []).is_err(), "{shape} must be refused");
    }
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

#[test]
fn the_shipped_file_retires_openais_old_pro_tier_keys() {
    let dir = tempfile::tempdir().unwrap();
    let descriptor = cortexkit_store_types::StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: cortexkit_store_types::Isolation::Module,
        backend: cortexkit_store_types::StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into_owned(),
        },
    };
    let store = fusiform_store::CatalogStore::open(&descriptor).unwrap();
    let doc: serde_json::Value = serde_json::from_str(include_str!(
        "../../../fusiform-module/data/plan-prices.json"
    ))
    .unwrap();
    let curated: Vec<_> = doc["cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|cell| {
            let price = &cell["price"];
            fusiform_store::NewPlanPrice {
                provider_id: cell["provider_id"].as_str().unwrap().into(),
                tier: cell["tier"].as_str().unwrap().into(),
                minor_units: price["minor_units"].as_i64(),
                exponent: price["exponent"].as_i64().map(|e| e as i32),
                currency: price["currency"].as_str().map(str::to_string),
                period: price["period"].as_str().map(str::to_string),
                boundary_at_ms: cell["boundary_at_ms"].as_i64().unwrap(),
                established_by: cell["established_by"].as_str().unwrap().into(),
                established_at_ms: cell["established_at_ms"].as_i64().unwrap(),
                review_by_ms: cell["review_by_ms"].as_i64().unwrap(),
                source_ref: cell["source_ref"].as_str().unwrap().into(),
                refusal_reason: cell["refusal_reason"].as_str().map(str::to_string),
            }
        })
        .collect();
    let old: Vec<_> = [("pro_200", "pro_20x"), ("pro_100", "pro_5x")]
        .into_iter()
        .map(|(new, old)| {
            let mut row = curated
                .iter()
                .find(|r| r.provider_id == "openai" && r.tier == new)
                .unwrap()
                .clone();
            row.tier = old.into();
            row.boundary_at_ms = 1789785600000;
            row
        })
        .collect();
    assert_eq!(store.append_plan_prices(&old).unwrap(), 2);
    assert_eq!(
        store.append_plan_prices(&curated).unwrap(),
        curated.len() + 2
    );
    // Read at the file's vintage, its newest boundary. Tombstones for keys the
    // file dropped are dated at that vintage, so reading at a fixed earlier
    // instant stops seeing them as soon as any later row is added to the file.
    let vintage = curated.iter().map(|r| r.boundary_at_ms).max().unwrap();
    let served = store.plan_prices_at(vintage, Some("openai")).unwrap();
    for tier in ["pro_20x", "pro_5x"] {
        let row = served.iter().find(|r| r.tier == tier).unwrap();
        assert!(
            row.minor_units.is_none(),
            "{tier} must not serve a stale price"
        );
        assert!(row
            .refusal_reason
            .as_deref()
            .unwrap()
            .contains("no longer carried"));
    }
    for (tier, units) in [
        ("pro_200", 20000),
        ("pro_100", 10000),
        ("pro_500", 50000),
        ("plus", 2000),
    ] {
        let row = served.iter().find(|r| r.tier == tier).unwrap();
        assert_eq!(row.minor_units, Some(units), "{tier}");
        assert_eq!(row.period.as_deref(), Some("month"));
    }
    assert_eq!(store.append_plan_prices(&curated).unwrap(), 0);
}
