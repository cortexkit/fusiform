//! Operator quality judgments have their own append-only history, not upstream eras.

use rusqlite::params;

use crate::{CatalogError, CatalogStore};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QualityGrade {
    pub elo: serde_json::Number,
    pub eq: serde_json::Number,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QualityGradeEra {
    pub id: i64,
    pub model: String,
    pub grade: Option<QualityGrade>,
    pub graded_by: String,
    pub reason: String,
    pub recorded_at_ms: i64,
}

fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<QualityGradeEra> {
    let elo: Option<String> = row.get(2)?;
    let eq: Option<String> = row.get(3)?;
    let number = |text: String| {
        serde_json::from_str(&text).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
        })
    };
    Ok(QualityGradeEra {
        id: row.get(0)?,
        model: row.get(1)?,
        grade: match (elo, eq) {
            (Some(elo), Some(eq)) => Some(QualityGrade {
                elo: number(elo)?,
                eq: number(eq)?,
            }),
            _ => None,
        },
        graded_by: row.get(4)?,
        reason: row.get(5)?,
        recorded_at_ms: row.get(6)?,
    })
}

impl CatalogStore {
    /// Append one judgment or withdrawal. Decimal tokens are TEXT, never REAL.
    pub fn append_quality_grade(
        &self,
        model: &str,
        grade: Option<&QualityGrade>,
        graded_by: &str,
        reason: &str,
        recorded_at_ms: i64,
    ) -> Result<i64, CatalogError> {
        Ok(self.inner.with_conn_fenced(|tx| {
            tx.execute(
                "INSERT INTO quality_grade_era \
                 (model, elo, eq, withdrawn, graded_by, reason, recorded_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    model,
                    grade.map(|g| g.elo.to_string()),
                    grade.map(|g| g.eq.to_string()),
                    grade.is_none(),
                    graded_by,
                    reason,
                    recorded_at_ms
                ],
            )?;
            crate::advance_catalog_version_in(tx, None, fusiform_core::Timestamp(recorded_at_ms))?;
            Ok(tx.last_insert_rowid())
        })?)
    }

    /// Current, non-withdrawn grades, read through the pool without a writer lease.
    pub fn quality_grades(&self) -> Result<Vec<QualityGradeEra>, CatalogError> {
        self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, model, elo, eq, graded_by, reason, recorded_at_ms \
                 FROM quality_grade_era q WHERE withdrawn = 0 \
                 AND id = (SELECT MAX(id) FROM quality_grade_era WHERE model = q.model) \
                 ORDER BY model",
            )?;
            let rows = stmt.query_map([], decode)?.collect();
            rows
        })
    }

    /// A model's full history, including withdrawals, in insertion order.
    pub fn quality_grade_history(&self, model: &str) -> Result<Vec<QualityGradeEra>, CatalogError> {
        self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, model, elo, eq, graded_by, reason, recorded_at_ms \
                 FROM quality_grade_era WHERE model = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map([model], decode)?.collect();
            rows
        })
    }

    /// Includes withdrawals so every write invalidates cached response packing.
    pub fn latest_quality_grade_id(&self) -> Result<i64, CatalogError> {
        self.raw_conn(|conn| {
            conn.query_row(
                "SELECT COALESCE(MAX(id), 0) FROM quality_grade_era",
                [],
                |r| r.get(0),
            )
        })
    }
}
