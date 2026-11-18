use std::collections::{BTreeMap, BTreeSet};

use fusiform_core::Timestamp;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::{Map, Value};

use crate::{advance_catalog_version_in, CatalogError, CatalogStore};

/// Dependency order matters: eras and artifact events refer to observations.
pub const STORE_HISTORY_TABLES: &[&str] = &[
    "observation",
    "era",
    "observation_artifact_event",
    "quality_grade_era",
    "model_mark_era",
    "fact_override_era",
    "curated_model_era",
    "cache_lifetime_era",
    "plan_price_era",
    "effort_grade_era",
    "workload_effort_era",
];

const DOCUMENT_TABLE: &str = "store_document_era";
const LOCAL_TABLES: &[&str] = &[
    "override_grade_era",
    "override_mark_era",
    "override_effort_grade_era",
    "override_workload_effort_era",
];

pub type ImportedTables = BTreeMap<String, Vec<Map<String, Value>>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreRelease {
    pub release: u64,
    pub state_hash: String,
    pub curation_hash: String,
    pub applied_at_ms: i64,
}

pub struct ReleaseArrival {
    pub release: StoreRelease,
    pub rows: ImportedTables,
}

pub type PlanMetadata = (Option<String>, Option<String>, Option<String>);

fn invalid(reason: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
        Some(reason.into()),
    )
}

pub(super) fn require_empty_history(conn: &Connection) -> rusqlite::Result<()> {
    for table in STORE_HISTORY_TABLES
        .iter()
        .chain(LOCAL_TABLES)
        .chain([&DOCUMENT_TABLE])
    {
        let populated =
            conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table})"), [], |r| {
                r.get(0)
            })?;
        if populated {
            return Err(invalid(format!(
                "store mode needs a fresh data folder: {table} already contains rows"
            )));
        }
    }
    Ok(())
}

fn cursor(conn: &Connection) -> rusqlite::Result<Option<StoreRelease>> {
    conn.query_row(
        "SELECT release, state_hash, curation_hash, applied_at_ms FROM store_release ORDER BY release DESC LIMIT 1",
        [],
        |r| Ok(StoreRelease { release: r.get(0)?, state_hash: r.get(1)?, curation_hash: r.get(2)?, applied_at_ms: r.get(3)? }),
    ).optional()
}

fn ingest_rows(
    table: &str,
    rows: &[Map<String, Value>],
    keep_ids: bool,
) -> rusqlite::Result<Vec<(Vec<String>, Vec<rusqlite::types::Value>)>> {
    let mut prior = 0;
    rows.iter()
        .map(|row| {
            if keep_ids {
                let id = row
                    .get("id")
                    .and_then(Value::as_i64)
                    .filter(|id| *id > prior)
                    .ok_or_else(|| {
                        invalid(format!(
                            "{table}: history ids must be positive and in insertion order"
                        ))
                    })?;
                prior = id;
            } else if row.contains_key("id") && table != "observation" {
                return Err(invalid(format!(
                    "{table}: release rows must not assign their own ids"
                )));
            }
            row.iter()
                .map(|(column, value)| {
                    use rusqlite::types::Value as Sql;
                    let value = match value {
                        Value::Null => Sql::Null,
                        Value::Bool(value) => Sql::Integer(i64::from(*value)),
                        Value::Number(value) => Sql::Integer(value.as_i64().ok_or_else(|| {
                            invalid(format!("{table}.{column}: expected a signed integer"))
                        })?),
                        Value::String(value) => Sql::Text(value.clone()),
                        _ => {
                            return Err(invalid(format!(
                                "{table}.{column}: expected a SQLite scalar"
                            )))
                        }
                    };
                    Ok((column.clone(), value))
                })
                .collect::<rusqlite::Result<Vec<_>>>()
                .map(|values| values.into_iter().unzip())
        })
        .collect()
}

type SqlRows = Vec<(Vec<String>, Vec<rusqlite::types::Value>)>;

fn prepare_tables(
    tables: &ImportedTables,
    keep_ids: bool,
) -> rusqlite::Result<Vec<(String, SqlRows)>> {
    let allowed =
        |table: &str| STORE_HISTORY_TABLES.contains(&table) || !keep_ids && table == DOCUMENT_TABLE;
    if let Some(table) = tables.keys().find(|table| !allowed(table)) {
        return Err(invalid(format!(
            "unexpected catalog history table {table:?}"
        )));
    }
    STORE_HISTORY_TABLES
        .iter()
        .chain([&DOCUMENT_TABLE])
        .filter_map(|table| {
            tables
                .get(*table)
                .map(|rows| Ok(((*table).to_owned(), ingest_rows(table, rows, keep_ids)?)))
        })
        .collect()
}

impl CatalogStore {
    pub fn published_plan_metadata(
        &self,
        provider: &str,
        tier: &str,
        at: i64,
    ) -> Result<PlanMetadata, CatalogError> {
        self.raw_conn(|conn| conn.query_row(
            "SELECT quota_value_json, service_tiers_json, concurrency_json FROM plan_price_era
             WHERE provider_id = ?1 AND tier = ?2 AND boundary_at_ms <= ?3 ORDER BY id DESC LIMIT 1",
            params![provider, tier, at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ))
    }
    pub fn latest_store_release(&self) -> Result<Option<StoreRelease>, CatalogError> {
        self.raw_conn(cursor)
    }

    pub fn latest_observation_id(&self) -> Result<i64, CatalogError> {
        self.raw_conn(|conn| {
            conn.query_row("SELECT COALESCE(MAX(id),0) FROM observation", [], |r| {
                r.get(0)
            })
        })
    }

    /// Restore the base and release chain together, or append releases to the
    /// already verified cursor. Nothing from a rejected batch becomes visible.
    pub fn apply_store_releases(
        &self,
        base: Option<&ImportedTables>,
        arrivals: &[ReleaseArrival],
    ) -> Result<(), CatalogError> {
        if !self.is_store_mode() {
            return Err(CatalogError::Invariant(
                "published releases require store mode".into(),
            ));
        }
        let base = base
            .map(|tables| prepare_tables(tables, true))
            .transpose()
            .map_err(|error| CatalogError::Invariant(error.to_string()))?;
        let rows = arrivals
            .iter()
            .map(|arrival| prepare_tables(&arrival.rows, false))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| CatalogError::Invariant(error.to_string()))?;
        self.inner.with_conn_fenced(|tx| {
            let write_rows = |groups: &[(String, SqlRows)]| -> rusqlite::Result<()> {
                for (table, rows) in groups {
                    // Table names were allowlisted; column names are checked against the
                    // installed schema before being interpolated. Every value is bound.
                    let mut info = tx.prepare("SELECT name FROM pragma_table_info(?1)")?;
                    let valid = info.query_map([table], |row| row.get::<_, String>(0))?
                        .collect::<Result<BTreeSet<_>, _>>()?;
                    for (cols, values) in rows {
                        if cols.is_empty() || cols.iter().any(|name| !valid.contains(name)) {
                            return Err(invalid(format!("{table}: invalid history row columns")));
                        }
                        let sql = format!("INSERT INTO {table} ({}) VALUES ({})", cols.join(","), vec!["?"; cols.len()].join(","));
                        tx.prepare_cached(&sql)?.execute(params_from_iter(values))?;
                    }
                }
                Ok(())
            };
            let current = cursor(tx)?;
            if current.is_none() != base.is_some() || arrivals.is_empty() {
                return Err(invalid("store bootstrap needs a base and releases; incremental apply needs an existing cursor"));
            }
            if let Some(base) = &base {
                require_empty_history(tx)?;
                write_rows(base)?;
            }
            let mut previous = current.map_or(0, |r| r.release);
            for (arrival, rows) in arrivals.iter().zip(&rows) {
                let release = &arrival.release;
                if previous.checked_add(1) != Some(release.release) {
                    return Err(invalid(format!("release {} does not follow {previous}", release.release)));
                }
                write_rows(rows)?;
                tx.execute("INSERT INTO store_release (release, state_hash, curation_hash, applied_at_ms) VALUES (?1, ?2, ?3, ?4)",
                    params![release.release, release.state_hash, release.curation_hash, release.applied_at_ms])?;
                advance_catalog_version_in(tx, None, Timestamp(release.applied_at_ms))?;
                previous = release.release;
            }
            Ok(())
        })?;
        self.signal_catalog_changed();
        Ok(())
    }

    pub fn store_documents_at(
        &self,
        at: Option<i64>,
    ) -> Result<BTreeMap<String, String>, CatalogError> {
        self.raw_conn(|conn| {
            let mut sql = conn.prepare("SELECT document, document_json FROM store_document_era d WHERE document_json IS NOT NULL AND id = (
                SELECT last.id FROM store_document_era last WHERE last.document = d.document AND last.recorded_at_ms <= ?1
                ORDER BY last.recorded_at_ms DESC, last.id DESC LIMIT 1) ORDER BY document")?;
            let documents = sql.query_map([at.unwrap_or(i64::MAX)], |r| Ok((r.get(0)?, r.get(1)?)))?.collect();
            documents
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
    use serde_json::json;

    fn descriptor(path: &std::path::Path) -> StorageDescriptor {
        StorageDescriptor {
            module_id: "fusiform".into(),
            storage_namespace: "default".into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: path.to_string_lossy().into_owned(),
            },
        }
    }

    #[test]
    fn store_consumer_refuses_existing_history_and_preserves_imported_ids_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = descriptor(&dir.path().join("local.db"));
        let legacy = CatalogStore::open(&legacy_path).unwrap();
        assert!(!legacy.quality_grades().unwrap().is_empty());
        assert!(legacy.latest_store_release().unwrap().is_none());
        assert!(legacy.store_documents_at(None).unwrap().is_empty());
        drop(legacy);
        let refusal = CatalogStore::open_for_store(&legacy_path).err().unwrap();
        assert!(refusal.to_string().contains("needs a fresh data folder"));

        let input_path = descriptor(&dir.path().join("remote.db"));
        let store = CatalogStore::open_for_store(&input_path).unwrap();
        assert_eq!(store.catalog_version().unwrap(), 0);
        assert!(store.quality_grades().unwrap().is_empty());
        assert!(store.model_marks().unwrap().is_empty());
        assert!(store.cache_lifetimes_at(i64::MAX, None).unwrap().is_empty());

        let mut base = ImportedTables::new();
        base.insert(
            "observation".into(),
            vec![
                json!({"id":4321,"source":"models.dev","observed_at_ms":1000,"outcome":"changed"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ],
        );
        base.insert("observation_artifact_event".into(), vec![json!({"id":44,"observation_id":4321,"action":"marked","reason":"format correction","recorded_at_ms":1001}).as_object().unwrap().clone()]);
        let arrival = |release| ReleaseArrival {
            release: StoreRelease {
                release,
                state_hash: "a".repeat(64),
                curation_hash: "b".repeat(64),
                applied_at_ms: 2000,
            },
            rows: ImportedTables::new(),
        };
        assert!(store
            .apply_store_releases(Some(&base), &[arrival(1), arrival(3)])
            .is_err());
        assert_eq!(store.catalog_version().unwrap(), 0);
        assert!(store.latest_store_release().unwrap().is_none());
        assert_eq!(
            store
                .raw_conn(
                    |db| db.query_row("SELECT COUNT(*) FROM observation", [], |r| r
                        .get::<_, i64>(0))
                )
                .unwrap(),
            0
        );
        store
            .apply_store_releases(Some(&base), &[arrival(1), arrival(2)])
            .unwrap();
        assert_eq!(store.catalog_version().unwrap(), 2001);
        drop(store);
        let reopened = CatalogStore::open_for_store(&input_path).unwrap();
        assert_eq!(reopened.latest_store_release().unwrap().unwrap().release, 2);
        let ids = reopened
            .raw_conn(|db| {
                db.query_row(
                    "SELECT id, observation_id FROM observation_artifact_event",
                    [],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
            })
            .unwrap();
        assert_eq!(ids, (44, 4321));
    }
}
