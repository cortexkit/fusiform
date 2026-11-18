use rusqlite::{params, Row};
use serde_json::Number;

use crate::{advance_catalog_version_in, CatalogError, CatalogStore};
use fusiform_core::Timestamp;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffortGradeEra {
    pub model: String,
    pub effort: String,
    pub elo: Option<Number>,
    pub eq: Option<Number>,
    pub reason: String,
    pub recorded_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadEffortEra {
    pub model: String,
    pub workload: String,
    pub effort_default: Option<String>,
    pub effort_minimum: Option<String>,
    pub excluded: bool,
    pub reason: String,
    pub recorded_at_ms: i64,
}

impl WorkloadEffortEra {
    pub fn is_withdrawn(&self) -> bool {
        !self.excluded && self.effort_default.is_none()
    }
}

pub(super) fn workload_row(r: &Row<'_>) -> rusqlite::Result<WorkloadEffortEra> {
    Ok(WorkloadEffortEra {
        model: r.get(0)?,
        workload: r.get(1)?,
        effort_default: r.get(2)?,
        effort_minimum: r.get(3)?,
        excluded: r.get(4)?,
        reason: r.get(5)?,
        recorded_at_ms: r.get(6)?,
    })
}

pub(super) fn number(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<Number>> {
    row.get::<_, Option<String>>(index)?
        .map(|text| {
            serde_json::from_str(&text).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()
}

impl CatalogStore {
    pub fn append_effort_grade(
        &self,
        value: &EffortGradeEra,
        graded_by: &str,
    ) -> Result<(), CatalogError> {
        if self.is_store_mode() {
            return self.append_override_effort(value);
        }
        self.inner.with_conn_fenced(|tx| {
            tx.execute(
                "INSERT INTO effort_grade_era (model, effort, elo, eq, withdrawn, graded_by, reason, recorded_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![value.model, value.effort, value.elo.as_ref().map(ToString::to_string),
                    value.eq.as_ref().map(ToString::to_string), value.elo.is_none(), graded_by,
                    value.reason, value.recorded_at_ms],
            )?;
            advance_catalog_version_in(tx, None, Timestamp(value.recorded_at_ms))?;
            Ok(())
        })?;
        self.signal_catalog_changed();
        Ok(())
    }

    pub fn append_workload_effort(
        &self,
        value: &WorkloadEffortEra,
        set_by: &str,
    ) -> Result<(), CatalogError> {
        if self.is_store_mode() {
            return self.append_override_workload(value);
        }
        self.inner.with_conn_fenced(|tx| {
            tx.execute(
                "INSERT INTO workload_effort_era (model, workload, effort_default, effort_minimum, excluded, withdrawn, set_by, reason, recorded_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![value.model, value.workload, value.effort_default, value.effort_minimum,
                    value.excluded, value.is_withdrawn(), set_by, value.reason, value.recorded_at_ms],
            )?;
            advance_catalog_version_in(tx, None, Timestamp(value.recorded_at_ms))?;
            Ok(())
        })?;
        self.signal_catalog_changed();
        Ok(())
    }

    pub fn effort_grades_at(&self, at: Option<i64>) -> Result<Vec<EffortGradeEra>, CatalogError> {
        self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT model, effort, elo, eq, reason, recorded_at_ms FROM effort_grade_era e \
                 WHERE withdrawn = 0 AND id = (SELECT MAX(id) FROM effort_grade_era \
                   WHERE model = e.model AND effort = e.effort AND recorded_at_ms <= ?1) \
                 ORDER BY model, effort",
            )?;
            let rows = stmt
                .query_map([at.unwrap_or(i64::MAX)], |r| {
                    Ok(EffortGradeEra {
                        model: r.get(0)?,
                        effort: r.get(1)?,
                        elo: number(r, 2)?,
                        eq: number(r, 3)?,
                        reason: r.get(4)?,
                        recorded_at_ms: r.get(5)?,
                    })
                })?
                .collect();
            rows
        })
    }

    pub fn workload_efforts_at(
        &self,
        at: Option<i64>,
    ) -> Result<Vec<WorkloadEffortEra>, CatalogError> {
        self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT model, workload, effort_default, effort_minimum, excluded, reason, recorded_at_ms \
                 FROM workload_effort_era e WHERE withdrawn = 0 AND id = ( \
                   SELECT MAX(id) FROM workload_effort_era WHERE model = e.model AND workload = e.workload \
                   AND recorded_at_ms <= ?1) ORDER BY model, workload",
            )?;
            let rows = stmt.query_map([at.unwrap_or(i64::MAX)], workload_row)?.collect();
            rows
        })
    }
}
