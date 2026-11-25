use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek},
};

use fusiform_core::Timestamp;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::{json, Map, Value};

use crate::history_base::{self, Tables};
use crate::ingest::{plan_release_ingest, TouchedModels};
use crate::{advance_catalog_version_in, is_rate_relevant_fact, CatalogError, CatalogStore};

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
    pub published_at_ms: i64,
    pub release: StoreRelease,
    pub rows: ImportedTables,
    pub upstream: Option<ReleaseObservation>,
}

pub struct ReleaseObservation {
    pub id: i64,
    pub at: Timestamp,
    pub prior_at: Timestamp,
    pub models: TouchedModels,
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

fn history_read_error(error: serde_json::Error) -> CatalogError {
    CatalogError::Invariant(format!("history base: {error}"))
}

fn write_tables(
    writer: &mut RowWriter<'_>,
    tables: &ImportedTables,
    arrival: Option<&ReleaseArrival>,
) -> rusqlite::Result<()> {
    if let Some(name) = tables.keys().find(|name| {
        !STORE_HISTORY_TABLES.contains(&name.as_str())
            && !(arrival.is_some() && name.as_str() == DOCUMENT_TABLE)
    }) {
        return Err(invalid(format!(
            "unexpected catalog history table {name:?}"
        )));
    }
    for &table in STORE_HISTORY_TABLES.iter().chain([&DOCUMENT_TABLE]) {
        let Some(rows) = tables.get(table) else {
            continue;
        };
        let mut prior = 0;
        for row in rows {
            if arrival.is_none() {
                prior = row
                    .get("id")
                    .and_then(Value::as_i64)
                    .filter(|&id| id > prior)
                    .ok_or_else(|| {
                        invalid(format!(
                            "{table}: history ids must be positive and in insertion order"
                        ))
                    })?;
            }
            writer.write(table, row, arrival)?;
        }
    }
    Ok(())
}

fn write_upstream_eras(
    writer: &mut RowWriter<'_>,
    arrival: &ReleaseArrival,
) -> rusqlite::Result<bool> {
    let Some(observation) = &arrival.upstream else {
        return Ok(false);
    };
    // Compare with the history in this transaction, including the imported
    // base and any earlier releases in the batch. A model's last published
    // facts stay in force for the comparison if it returns after a withdrawal.
    let plan = plan_release_ingest(
        writer.tx,
        &observation.models,
        observation.at,
        observation.id,
    )?;
    let mut rates_changed = false;
    for era in plan.eras {
        let fact = era.fact_key.as_str();
        rates_changed |= is_rate_relevant_fact(fact);
        let row = json!({
            "source": era.source.as_str(), "provider_id": era.provider_id, "model_id": era.model_id,
            "fact_key": fact, "value_json": era.value_json, "boundary_at_ms": era.boundary_at.0,
            "boundary_kind": "observed", "observation_id": era.observation_id,
            "prior_observation_at_ms": observation.prior_at.0,
        });
        writer.write(
            "era",
            row.as_object().expect("era row is an object"),
            Some(arrival),
        )?;
    }
    Ok(rates_changed)
}

type InsertRow<'a> = &'a dyn Fn(&str, &[&str], &[rusqlite::types::Value]) -> rusqlite::Result<()>;

struct RowWriter<'a> {
    tx: &'a Connection,
    insert: InsertRow<'a>,
    valid_columns: BTreeMap<&'static str, BTreeSet<String>>,
}

impl<'a> RowWriter<'a> {
    fn new(tx: &'a Connection, insert: InsertRow<'a>) -> Self {
        Self {
            tx,
            insert,
            valid_columns: BTreeMap::new(),
        }
    }

    fn write(
        &mut self,
        table: &'static str,
        row: &Map<String, Value>,
        arrival: Option<&ReleaseArrival>,
    ) -> rusqlite::Result<()> {
        use rusqlite::types::Value as Sql;
        let valid = match self.valid_columns.entry(table) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let mut info = self.tx.prepare("SELECT name FROM pragma_table_info(?1)")?;
                let columns = info
                    .query_map([table], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<BTreeSet<_>>>()?;
                entry.insert(columns)
            }
        };
        if row.is_empty() || row.keys().any(|column| !valid.contains(column)) {
            return Err(invalid(format!("{table}: invalid history row columns")));
        }
        if arrival.is_some() && row.contains_key("id") && table != "observation" {
            return Err(invalid(format!(
                "{table}: release rows must not assign their own ids"
            )));
        }
        let columns: Vec<_> = row.keys().map(String::as_str).collect();
        let mut values =
            row.iter()
                .map(|(column, value)| match value {
                    Value::Null => Ok(Sql::Null),
                    Value::Bool(value) => Ok(Sql::Integer(i64::from(*value))),
                    Value::Number(value) => value.as_i64().map(Sql::Integer).ok_or_else(|| {
                        invalid(format!("{table}.{column}: expected a signed integer"))
                    }),
                    Value::String(value) => Ok(Sql::Text(value.clone())),
                    _ => Err(invalid(format!(
                        "{table}.{column}: expected a SQLite scalar"
                    ))),
                })
                .collect::<rusqlite::Result<Vec<_>>>()?;
        if let Some(arrival) = arrival.filter(|_| table == "plan_price_era") {
            if !resolve_plan_boundary(self.tx, &columns, &mut values, arrival)? {
                return Ok(());
            }
        }
        (self.insert)(table, &columns, &values)
    }
}

fn resolve_plan_boundary(
    conn: &Connection,
    columns: &[&str],
    values: &mut [rusqlite::types::Value],
    arrival: &ReleaseArrival,
) -> rusqlite::Result<bool> {
    use rusqlite::types::Value as Sql;
    let position = |name: &str| columns.iter().position(|&column| column == name);
    let field = |name| position(name).map_or(&Sql::Null, |i| &values[i]);
    let previous = conn
        .query_row(
            "SELECT minor_units, exponent, currency, period, refusal_reason, quota_value_json, \
                    service_tiers_json, concurrency_json FROM plan_price_era \
             WHERE provider_id = ?1 AND tier = ?2 AND boundary_at_ms <= ?3 \
             ORDER BY id DESC LIMIT 1",
            params![field("provider_id"), field("tier"), arrival.published_at_ms],
            |row| {
                (0..8)
                    .map(|i| row.get::<_, Sql>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            },
        )
        .optional()?;
    let Some(previous) = previous else {
        return Ok(true);
    };
    let claim = [
        "minor_units",
        "exponent",
        "currency",
        "period",
        "refusal_reason",
    ];
    if claim.into_iter().map(field).eq(previous[..5].iter()) {
        let metadata = ["quota_value_json", "service_tiers_json", "concurrency_json"];
        if arrival.release.release == 1 && metadata.into_iter().map(field).eq(previous[5..].iter())
        {
            return Ok(false);
        }
        // The price is unchanged. Only the new metadata becomes eligible at publication.
        let index =
            position("boundary_at_ms").ok_or_else(|| invalid("plan-price row needs a boundary"))?;
        values[index] = Sql::Integer(arrival.published_at_ms);
    }
    Ok(true)
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
        self.apply_releases(
            base.map(|base| move |writer: &mut RowWriter<'_>| write_tables(writer, base, None)),
            arrivals,
        )
    }

    /// Read only the observation watermark needed to translate later releases.
    /// The caller rewinds the same verified base before applying it.
    pub fn history_observation_watermark(input: impl Read) -> Result<i64, CatalogError> {
        let mut watermark = 0;
        history_base::read(input, Tables::Observations, |_, row| {
            watermark = row["id"].as_i64().expect("validated observation id");
            Ok(())
        })
        .map_err(history_read_error)?;
        Ok(watermark)
    }

    /// Stream an already verified v1 history base into the release transaction.
    pub fn apply_streamed_store_releases(
        &self,
        mut input: impl Read + Seek,
        arrivals: &[ReleaseArrival],
    ) -> Result<(), CatalogError> {
        self.apply_releases(
            Some(|writer: &mut RowWriter<'_>| {
                // The wire orders tables by name. Install observations first so
                // SQLite checks era and artifact foreign keys on each insert.
                for tables in [Tables::Observations, Tables::Rest] {
                    input.rewind().map_err(|error| invalid(error.to_string()))?;
                    history_base::read(&mut input, tables, |table, row| {
                        writer.write(table, &row, None)
                    })
                    .map_err(|error| invalid(error.to_string()))?;
                }
                Ok(())
            }),
            arrivals,
        )
    }

    fn apply_releases(
        &self,
        write_base: Option<impl FnOnce(&mut RowWriter<'_>) -> rusqlite::Result<()>>,
        arrivals: &[ReleaseArrival],
    ) -> Result<(), CatalogError> {
        if !self.is_store_mode() {
            return Err(CatalogError::Invariant(
                "published releases require store mode".into(),
            ));
        }
        let rates_version = self.inner.with_conn_fenced(|tx| {
            let insert = |table: &str, columns: &[&str], values: &[rusqlite::types::Value]| {
                // The row writer validates table and column names; row values
                // are passed as SQL parameters, never interpolated.
                let sql = format!(
                    "INSERT INTO {table} ({}) VALUES ({})",
                    columns.join(","),
                    vec!["?"; columns.len()].join(",")
                );
                tx.prepare_cached(&sql)?.execute(params_from_iter(values.iter()))?;
                Ok(())
            };
            let mut writer = RowWriter::new(tx, &insert);
            let current = cursor(tx)?;
            if current.is_none() != write_base.is_some() || arrivals.is_empty() {
                return Err(invalid("store bootstrap needs a base and releases; incremental apply needs an existing cursor"));
            }
            if let Some(write_base) = write_base {
                require_empty_history(tx)?;
                write_base(&mut writer)?;
            }
            let mut previous = current.as_ref().map_or(0, |r| r.release);
            let mut previous_curation = current.as_ref().map(|r| r.curation_hash.as_str());
            let mut rates_version = None;
            for arrival in arrivals {
                let release = &arrival.release;
                if previous.checked_add(1) != Some(release.release) {
                    return Err(invalid(format!("release {} does not follow {previous}", release.release)));
                }
                write_tables(&mut writer, &arrival.rows, Some(arrival))?;
                let upstream_rates_changed = write_upstream_eras(&mut writer, arrival)?;
                tx.execute("INSERT INTO store_release (release, state_hash, curation_hash, applied_at_ms) VALUES (?1, ?2, ?3, ?4)",
                    params![release.release, release.state_hash, release.curation_hash, release.applied_at_ms])?;
                let issued = advance_catalog_version_in(tx, None, Timestamp(release.applied_at_ms))?;
                // Release one installs the base and has no prior curation hash.
                if previous_curation != Some(release.curation_hash.as_str())
                    || upstream_rates_changed
                    || arrival.rows.get("era").into_iter().flatten().any(|row| {
                        row.get("fact_key").and_then(Value::as_str).is_some_and(is_rate_relevant_fact)
                    })
                {
                    rates_version = Some(issued);
                }
                previous = release.release;
                previous_curation = Some(&release.curation_hash);
            }
            Ok(rates_version)
        })?;
        if let Some(issued) = rates_version {
            self.record_rates_version(issued);
        }
        self.signal_catalog_changed();
        Ok(())
    }

    pub fn store_document_at(
        &self,
        document: &str,
        at: i64,
    ) -> Result<Option<String>, CatalogError> {
        self.raw_conn(|conn| {
            Ok(conn.query_row(
                "SELECT document_json FROM store_document_era WHERE document = ?1 AND recorded_at_ms <= ?2 \
                 ORDER BY recorded_at_ms DESC, id DESC LIMIT 1",
                params![document, at],
                |row| row.get::<_, Option<String>>(0),
            ).optional()?.flatten())
        })
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
    fn store_rates_version_tracks_last_relevant_release_after_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            CatalogStore::open_for_store(&descriptor(&dir.path().join("rates.db"))).unwrap();
        let arrival = |release, curation: char, fact: Option<&str>| {
            let mut rows = ImportedTables::new();
            if let Some(fact) = fact {
                let row = json!({"source":"models.dev","provider_id":"example","model_id":"m",
                    "fact_key":fact,"value_json":"null","boundary_at_ms":1000,"boundary_kind":"asserted"});
                rows.insert("era".into(), vec![row.as_object().unwrap().clone()]);
            }
            ReleaseArrival {
                published_at_ms: 2000,
                release: StoreRelease {
                    release,
                    state_hash: "a".repeat(64),
                    curation_hash: curation.to_string().repeat(64),
                    applied_at_ms: 2000,
                },
                rows,
                upstream: None,
            }
        };
        store
            .apply_store_releases(Some(&ImportedTables::new()), &[arrival(1, 'a', None)])
            .unwrap();
        assert_eq!(
            store.rates_version(),
            2000,
            "the initial curation is rate-relevant"
        );

        store
            .apply_store_releases(
                None,
                &[
                    arrival(2, 'a', Some("rate.input")),
                    arrival(3, 'a', Some("model.name")),
                ],
            )
            .unwrap();
        assert_eq!(store.catalog_version().unwrap(), 2002);
        assert_eq!(store.rates_version(), 2001);

        store
            .apply_store_releases(None, &[arrival(4, 'b', None), arrival(5, 'b', None)])
            .unwrap();
        assert_eq!(store.catalog_version().unwrap(), 2004);
        assert_eq!(
            store.rates_version(),
            2003,
            "the curation change advances only its own version"
        );

        assert!(store
            .apply_store_releases(
                None,
                &[arrival(6, 'b', Some("model.family")), arrival(8, 'b', None),]
            )
            .is_err());
        assert_eq!(store.catalog_version().unwrap(), 2004);
        assert_eq!(
            store.rates_version(),
            2003,
            "a rejected batch publishes no version"
        );
    }

    #[test]
    fn streamed_history_checks_table_rows_and_rolls_back_the_whole_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            CatalogStore::open_for_store(&descriptor(&dir.path().join("stream.db"))).unwrap();
        let tables: Map<String, Value> = STORE_HISTORY_TABLES[..9]
            .iter()
            .map(|&table| (table.into(), json!([])))
            .collect();
        let mut base = json!({"format":"ck-models-history-base/v1","release":1,"tables":tables});
        let observation =
            json!({"id":24,"source":"models.dev","observed_at_ms":1000,"outcome":"changed"});
        base["tables"]["observation"] = json!([observation]);
        base["tables"]["era"] = json!([{"id":31,"source":"models.dev","provider_id":"p","model_id":"m",
            "fact_key":"existence","value_json":"\"present\"","boundary_at_ms":1000,"boundary_kind":"observed","prior_observation_at_ms":999,"observation_id":24}]);
        let arrival = |release| ReleaseArrival {
            published_at_ms: 2000,
            release: StoreRelease {
                release,
                state_hash: "a".repeat(64),
                curation_hash: "b".repeat(64),
                applied_at_ms: 2000,
            },
            rows: ImportedTables::new(),
            upstream: None,
        };
        let input = |value: &Value| std::io::Cursor::new(serde_json::to_vec(value).unwrap());
        let assert_empty = || {
            assert!(store.latest_store_release().unwrap().is_none());
            assert_eq!(store.catalog_version().unwrap(), 0);
            assert_eq!(store.latest_observation_id().unwrap(), 0);
        };

        let mut unknown = base.clone();
        unknown["tables"]["unrecognized"] = json!([]);
        let mut bad_column = base.clone();
        bad_column["tables"]["era"][0]["invented_column"] = json!(1);
        let mut bad_id = base.clone();
        bad_id["tables"]["era"] = json!([base["tables"]["era"][0], base["tables"]["era"][0]]);
        let mut orphan = base.clone();
        orphan["tables"]["era"][0]["observation_id"] = json!(25);
        for (document, expected) in [
            (unknown, "unexpected catalog history table"),
            (bad_column, "invalid history row columns"),
            (
                bad_id,
                "history ids must be positive and in insertion order",
            ),
            (orphan, "FOREIGN KEY constraint failed"),
        ] {
            let error = store
                .apply_streamed_store_releases(input(&document), &[arrival(1)])
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{error}");
            assert_empty();
        }
        let mut trailing = serde_json::to_vec(&base).unwrap();
        trailing.extend_from_slice(b" true");
        let error = store
            .apply_streamed_store_releases(std::io::Cursor::new(trailing), &[arrival(1)])
            .unwrap_err();
        assert!(error.to_string().contains("trailing characters"));
        assert_empty();
        let error = store
            .apply_streamed_store_releases(input(&base), &[arrival(1), arrival(3)])
            .unwrap_err();
        assert!(error.to_string().contains("release 3 does not follow 1"));
        assert_empty();

        assert_eq!(
            CatalogStore::history_observation_watermark(input(&base)).unwrap(),
            24
        );
        store
            .apply_streamed_store_releases(input(&base), &[arrival(1)])
            .unwrap();
        assert_eq!(store.latest_observation_id().unwrap(), 24);
        assert_eq!(store.latest_store_release().unwrap().unwrap().release, 1);
        let id = store
            .raw_conn(|db| db.query_row("SELECT id FROM era", [], |row| row.get::<_, i64>(0)))
            .unwrap();
        assert_eq!(id, 31);
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
            published_at_ms: 2000,
            release: StoreRelease {
                release,
                state_hash: "a".repeat(64),
                curation_hash: "b".repeat(64),
                applied_at_ms: 2000,
            },
            rows: ImportedTables::new(),
            upstream: None,
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
