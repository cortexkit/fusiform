//! Operator quality judgments have their own append-only history, not upstream eras.

use rusqlite::params;

use crate::{CatalogError, CatalogStore};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum QualityGrade {
    Chat {
        elo: serde_json::Number,
        eq: serde_json::Number,
    },
    NonChat {
        grade: serde_json::Number,
    },
}

impl QualityGrade {
    pub fn elo(&self) -> Option<&serde_json::Number> {
        match self {
            Self::Chat { elo, .. } => Some(elo),
            Self::NonChat { .. } => None,
        }
    }

    pub fn eq(&self) -> Option<&serde_json::Number> {
        match self {
            Self::Chat { eq, .. } => Some(eq),
            Self::NonChat { .. } => None,
        }
    }

    pub fn grade(&self) -> Option<&serde_json::Number> {
        match self {
            Self::NonChat { grade } => Some(grade),
            Self::Chat { .. } => None,
        }
    }
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

#[derive(Debug, Clone, PartialEq)]
pub struct ModelMarkEra {
    pub id: i64,
    pub model: String,
    pub decommissioned: bool,
    pub marked_by: String,
    pub reason: String,
    pub recorded_at_ms: i64,
}

fn decode_mark(row: &rusqlite::Row<'_>) -> rusqlite::Result<ModelMarkEra> {
    Ok(ModelMarkEra {
        id: row.get(0)?,
        model: row.get(1)?,
        decommissioned: row.get(2)?,
        marked_by: row.get(3)?,
        reason: row.get(4)?,
        recorded_at_ms: row.get(5)?,
    })
}

fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<QualityGradeEra> {
    let elo: Option<String> = row.get(2)?;
    let eq: Option<String> = row.get(3)?;
    let grade: Option<String> = row.get(7)?;
    let number = |text: String| {
        serde_json::from_str(&text).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
        })
    };
    Ok(QualityGradeEra {
        id: row.get(0)?,
        model: row.get(1)?,
        grade: match (elo, eq, grade) {
            (Some(elo), Some(eq), None) => Some(QualityGrade::Chat {
                elo: number(elo)?,
                eq: number(eq)?,
            }),
            (None, None, Some(grade)) => Some(QualityGrade::NonChat {
                grade: number(grade)?,
            }),
            _ => None,
        },
        graded_by: row.get(4)?,
        reason: row.get(5)?,
        recorded_at_ms: row.get(6)?,
    })
}

impl CatalogStore {
    /// Append an operator mark and publish its catalog version atomically.
    pub fn append_model_mark(
        &self,
        model: &str,
        decommissioned: bool,
        marked_by: &str,
        reason: &str,
        recorded_at_ms: i64,
    ) -> Result<i64, CatalogError> {
        Ok(self.inner.with_conn_fenced(|tx| {
            tx.execute(
                "INSERT INTO model_mark_era (model, decommissioned, marked_by, reason, recorded_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![model, decommissioned, marked_by, reason, recorded_at_ms],
            )?;
            crate::advance_catalog_version_in(tx, None, fusiform_core::Timestamp(recorded_at_ms))?;
            Ok(tx.last_insert_rowid())
        })?)
    }

    /// Current decommissioned keys; a restoration hides the prior mark.
    pub fn model_marks(&self) -> Result<Vec<ModelMarkEra>, CatalogError> {
        self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, model, decommissioned, marked_by, reason, recorded_at_ms FROM model_mark_era m \
                 WHERE decommissioned = 1 AND id = (SELECT MAX(id) FROM model_mark_era WHERE model = m.model) ORDER BY model",
            )?;
            let rows = stmt.query_map([], decode_mark)?.collect();
            rows
        })
    }

    /// Full mark history in insertion order, including restorations.
    pub fn model_mark_history(&self, model: &str) -> Result<Vec<ModelMarkEra>, CatalogError> {
        self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, model, decommissioned, marked_by, reason, recorded_at_ms FROM model_mark_era WHERE model = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map([model], decode_mark)?.collect();
            rows
        })
    }

    /// Restorations also invalidate cached response packing.
    pub fn latest_model_mark_id(&self) -> Result<i64, CatalogError> {
        self.raw_conn(|conn| {
            conn.query_row("SELECT COALESCE(MAX(id), 0) FROM model_mark_era", [], |r| {
                r.get(0)
            })
        })
    }

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
                 (model, elo, eq, withdrawn, graded_by, reason, recorded_at_ms, grade) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    model,
                    grade.and_then(QualityGrade::elo).map(ToString::to_string),
                    grade.and_then(QualityGrade::eq).map(ToString::to_string),
                    grade.is_none(),
                    graded_by,
                    reason,
                    recorded_at_ms,
                    grade.and_then(QualityGrade::grade).map(ToString::to_string)
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
                "SELECT id, model, elo, eq, graded_by, reason, recorded_at_ms, grade \
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
                "SELECT id, model, elo, eq, graded_by, reason, recorded_at_ms, grade \
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
