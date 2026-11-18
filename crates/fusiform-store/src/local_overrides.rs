use fusiform_core::Timestamp;
use rusqlite::{params, Row};

use crate::{
    advance_catalog_version_in, quality, quality_efforts, CatalogError, CatalogStore,
    EffortGradeEra, QualityGrade, QualityGradeEra, WorkloadEffortEra,
};

#[derive(Debug)]
pub struct LocalMark {
    pub model: String,
    pub decommissioned: bool,
    pub reason: String,
    pub recorded_at_ms: i64,
}

#[derive(Default)]
pub struct LocalOverrides {
    pub grades: Vec<QualityGradeEra>,
    pub efforts: Vec<EffortGradeEra>,
    pub workloads: Vec<WorkloadEffortEra>,
    pub marks: Vec<LocalMark>,
}

impl CatalogStore {
    pub fn append_override_mark(
        &self,
        model: &str,
        value: Option<bool>,
        reason: &str,
        at: i64,
    ) -> Result<i64, CatalogError> {
        if !self.is_store_mode() {
            return Err(CatalogError::Invariant(
                "local mark overrides require store mode".into(),
            ));
        }
        let id = self.inner.with_conn_fenced(|tx| {
            tx.execute("INSERT INTO override_mark_era (model,decommissioned,marked_by,reason,recorded_at_ms) VALUES (?1,?2,'user',?3,?4)", params![model,value,reason,at])?;
            advance_catalog_version_in(tx, None, Timestamp(at))?;
            Ok(tx.last_insert_rowid())
        })?;
        self.signal_catalog_changed();
        Ok(id)
    }

    pub(super) fn append_override_grade(
        &self,
        model: &str,
        kind: Option<&str>,
        grade: Option<&QualityGrade>,
        reason: &str,
        at: i64,
        effort: Option<&str>,
    ) -> Result<i64, CatalogError> {
        let id = self.inner.with_conn_fenced(|tx| {
            tx.execute("INSERT INTO override_grade_era (model,kind,elo,eq,grade,withdrawn,graded_by,reason,recorded_at_ms,graded_effort)
                VALUES (?1,?2,?3,?4,?5,?6,'user',?7,?8,?9)", params![model,kind,
                grade.and_then(QualityGrade::elo).map(ToString::to_string),grade.and_then(QualityGrade::eq).map(ToString::to_string),
                grade.and_then(QualityGrade::grade).map(ToString::to_string),grade.is_none(),reason,at,effort])?;
            advance_catalog_version_in(tx, None, Timestamp(at))?;
            Ok(tx.last_insert_rowid())
        })?;
        self.signal_catalog_changed();
        Ok(id)
    }

    pub(super) fn append_override_effort(&self, row: &EffortGradeEra) -> Result<(), CatalogError> {
        self.inner.with_conn_fenced(|tx| {
            tx.execute("INSERT INTO override_effort_grade_era (model,effort,elo,eq,withdrawn,graded_by,reason,recorded_at_ms) VALUES (?1,?2,?3,?4,?5,'user',?6,?7)",
                params![row.model,row.effort,row.elo.as_ref().map(ToString::to_string),row.eq.as_ref().map(ToString::to_string),row.elo.is_none(),row.reason,row.recorded_at_ms])?;
            advance_catalog_version_in(tx, None, Timestamp(row.recorded_at_ms))?;
            Ok(())
        })?;
        self.signal_catalog_changed();
        Ok(())
    }

    pub(super) fn append_override_workload(
        &self,
        row: &WorkloadEffortEra,
    ) -> Result<(), CatalogError> {
        self.inner.with_conn_fenced(|tx| {
            tx.execute("INSERT INTO override_workload_effort_era (model,workload,effort_default,effort_minimum,excluded,withdrawn,set_by,reason,recorded_at_ms) VALUES (?1,?2,?3,?4,?5,?6,'user',?7,?8)",
                params![row.model,row.workload,row.effort_default,row.effort_minimum,row.excluded,row.is_withdrawn(),row.reason,row.recorded_at_ms])?;
            advance_catalog_version_in(tx, None, Timestamp(row.recorded_at_ms))?;
            Ok(())
        })?;
        self.signal_catalog_changed();
        Ok(())
    }

    pub fn local_overrides_at(&self, at: Option<i64>) -> Result<LocalOverrides, CatalogError> {
        if !self.is_store_mode() {
            return Ok(LocalOverrides::default());
        }
        self.raw_conn(|conn| {
            fn read<T>(db: &rusqlite::Connection, sql: &str, at: i64, decode: impl FnMut(&Row<'_>) -> rusqlite::Result<T>) -> rusqlite::Result<Vec<T>> {
                db.prepare(sql)?.query_map([at], decode)?.collect()
            }
            let at = at.unwrap_or(i64::MAX);
            let grades = read(conn, "SELECT id,model,elo,eq,graded_by,reason,recorded_at_ms,grade,kind,graded_effort FROM override_grade_era e
                WHERE withdrawn=0 AND id=(SELECT MAX(id) FROM override_grade_era WHERE model=e.model AND kind IS e.kind AND recorded_at_ms<=?1)", at, quality::decode)?;
            let efforts = read(conn, "SELECT model,effort,elo,eq,reason,recorded_at_ms FROM override_effort_grade_era e
                WHERE withdrawn=0 AND id=(SELECT MAX(id) FROM override_effort_grade_era WHERE model=e.model AND effort=e.effort AND recorded_at_ms<=?1)", at,
                |r| Ok(EffortGradeEra { model:r.get(0)?, effort:r.get(1)?, elo:quality_efforts::number(r,2)?, eq:quality_efforts::number(r,3)?, reason:r.get(4)?, recorded_at_ms:r.get(5)? }))?;
            let workloads = read(conn, "SELECT model,workload,effort_default,effort_minimum,excluded,reason,recorded_at_ms FROM override_workload_effort_era e
                WHERE withdrawn=0 AND id=(SELECT MAX(id) FROM override_workload_effort_era WHERE model=e.model AND workload=e.workload AND recorded_at_ms<=?1)", at,
                quality_efforts::workload_row)?;
            let marks = read(conn, "SELECT model,decommissioned,reason,recorded_at_ms FROM override_mark_era e
                WHERE decommissioned IS NOT NULL AND id=(SELECT MAX(id) FROM override_mark_era WHERE model=e.model AND recorded_at_ms<=?1)", at,
                |r| Ok(LocalMark { model:r.get(0)?, decommissioned:r.get(1)?,reason:r.get(2)?,recorded_at_ms:r.get(3)? }))?;
            Ok(LocalOverrides { grades, efforts, workloads, marks })
        })
    }
}
