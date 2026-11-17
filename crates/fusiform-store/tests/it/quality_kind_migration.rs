use cortexkit_store::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_store::{schema, CatalogStore};
use rusqlite::{types::Value, Connection};

fn descriptor(path: &std::path::Path) -> StorageDescriptor {
    StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_string_lossy().into(),
        },
    }
}

fn snapshot(conn: &Connection, table: &str, columns: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn
        .prepare(&format!("SELECT {columns} FROM {table} ORDER BY id"))
        .unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |row| {
        (0..n)
            .map(|i| row.get(i))
            .collect::<Result<Vec<Value>, _>>()
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

#[test]
fn v13_preserves_every_old_column_and_checks_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("migration.db");
    let descriptor = descriptor(&path);
    {
        let old = cortexkit_store::open_sqlite(&descriptor).unwrap();
        old.migrate(schema::NAMESPACE, &schema::MIGRATIONS[..12])
            .unwrap();
        old.with_conn_fenced(|conn| conn.execute_batch(r#"
            INSERT INTO quality_grade_era (id, model, elo, eq, withdrawn, graded_by, reason, recorded_at_ms)
            VALUES (1000,'test/exact','1599.800','1200.00',0,'operator','exact tokens',9000);
            INSERT INTO quality_grade_era (id, model, withdrawn, graded_by, reason, recorded_at_ms)
            VALUES (1001,'test/exact',1,'operator','withdrawn',1);
            INSERT INTO era (source, provider_id, model_id, fact_key, value_json, boundary_at_ms, boundary_kind)
            VALUES ('models.dev','test','exact','existence','"present"',1000,'seed');
        "#)).unwrap();
    }
    let conn = Connection::open(&path).unwrap();
    let old_columns = "id, model, elo, eq, withdrawn, graded_by, reason, recorded_at_ms, grade";
    let grades = snapshot(&conn, "quality_grade_era", old_columns);
    let marks = snapshot(&conn, "model_mark_era", "*");
    let eras = snapshot(&conn, "era", "*");
    let store = CatalogStore::open(&descriptor).unwrap();
    assert_eq!(snapshot(&conn, "quality_grade_era", old_columns), grades);
    assert_eq!(snapshot(&conn, "model_mark_era", "*"), marks);
    assert_eq!(snapshot(&conn, "era", "*"), eras);
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM quality_grade_era WHERE kind IS NOT NULL",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    for kind in ["transcription", "speech", "image", "video", "search"] {
        conn.execute("INSERT INTO quality_grade_era (model, kind, grade, withdrawn, graded_by, reason, recorded_at_ms) VALUES ('test/media',?1,'87.500',0,'operator','media',3)", [kind]).unwrap();
        conn.execute("INSERT INTO quality_grade_era (model, kind, withdrawn, graded_by, reason, recorded_at_ms) VALUES ('test/media',?1,1,'operator','withdraw',2)", [kind]).unwrap();
        assert!(conn.execute("INSERT INTO quality_grade_era (model, kind, elo, eq, withdrawn, graded_by, reason, recorded_at_ms) VALUES ('test/invalid',?1,'1','2',0,'operator','invalid',1)", [kind]).is_err());
    }
    for kind in [None, Some("chat"), Some("unknown"), Some("")] {
        assert!(conn.execute("INSERT INTO quality_grade_era (model, kind, grade, withdrawn, graded_by, reason, recorded_at_ms) VALUES ('test/invalid',?1,'1',0,'operator','invalid',1)", [kind]).is_err());
    }
    assert!(conn.execute("INSERT INTO quality_grade_era (model, kind, withdrawn, graded_by, reason, recorded_at_ms) VALUES ('test/invalid','chat',1,'operator','invalid',1)", []).is_err());
    assert_eq!(
        store.quality_grade_history("test/exact").unwrap()[1].id,
        1001
    );
}

#[test]
fn v13_refuses_one_legacy_media_grade_without_changing_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("refusal.db");
    let descriptor = descriptor(&path);
    {
        let old = cortexkit_store::open_sqlite(&descriptor).unwrap();
        old.migrate(schema::NAMESPACE, &schema::MIGRATIONS[..12])
            .unwrap();
        old.with_conn_fenced(|conn| conn.execute("INSERT INTO quality_grade_era (model, grade, withdrawn, graded_by, reason, recorded_at_ms) VALUES ('test/media','87.500',0,'operator','legacy media',1)", [])).unwrap();
    }
    let conn = Connection::open(&path).unwrap();
    let before = snapshot(&conn, "quality_grade_era", "*");
    let error = match CatalogStore::open(&descriptor) {
        Ok(_) => panic!("legacy media grade must refuse migration"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("1 legacy media grade rows have no kind"),
        "{error}"
    );
    assert_eq!(snapshot(&conn, "quality_grade_era", "*"), before);
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('quality_grade_era') WHERE name='kind'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

fn row_counts(conn: &Connection) -> Vec<(&'static str, i64)> {
    [
        "quality_grade_era",
        "model_mark_era",
        "era",
        "plan_price_era",
        "curated_model_era",
        "fact_override_era",
        "cache_lifetime_era",
    ]
    .into_iter()
    .map(|table| {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        let count = if exists {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        } else {
            0
        };
        (table, count)
    })
    .collect()
}

/// Run only against an operator-provided copy: opening acquires the writer lease.
#[test]
#[ignore]
fn rehearse_v13_on_store_copy() {
    let path = std::env::var_os("FUSIFORM_REHEARSAL_DB")
        .expect("set FUSIFORM_REHEARSAL_DB to a copy of the store");
    let path = std::path::Path::new(&path);
    let conn =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let before = row_counts(&conn);
    eprintln!("before migration: {before:?}");
    drop(conn);
    let _store = CatalogStore::open(&descriptor(path)).unwrap();
    let conn =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let after = row_counts(&conn);
    eprintln!("after migration: {after:?}");
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .unwrap();
    eprintln!("quick_check: {check}");
    assert_eq!(check, "ok");
    assert_eq!(after, before);
}
