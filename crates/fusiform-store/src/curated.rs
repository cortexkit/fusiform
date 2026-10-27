//! The stored history of fusiform's hand-curated model rows.
//!
//! The module serves curated model rows straight from its compiled file on a
//! current read. A point-in-time read needs to know which claim was in force at
//! an instant, so every boot applies the file here, and a change to a row's
//! claim becomes a new row in `curated_model_era` (see the schema for the
//! dating rule).

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, OptionalExtension};

use crate::{CatalogError, CatalogStore};

/// One curated model row, as the store records it.
///
/// Kept apart from the module's loaded row for the reason `NewPlanPrice` is: a
/// field added to the file for the loader's benefit must not silently become
/// part of the stored claim, or every boot after the change would record a
/// phantom change on every row.
#[derive(Debug, Clone, PartialEq)]
pub struct NewCuratedModel {
    pub provider_id: String,
    pub model_id: String,
    /// Fact key to served value.
    pub facts: BTreeMap<String, serde_json::Value>,
    /// The login methods allowed to read the row.
    pub auth_methods: Vec<String>,
    pub source_ref: String,
    pub established_by: Option<String>,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

/// One stored row of a curated model's history.
#[derive(Debug, Clone, PartialEq)]
pub struct CuratedModelEra {
    pub provider_id: String,
    pub model_id: String,
    /// When fusiform began serving this claim.
    pub boundary_at_ms: i64,
    /// The claim in force from the boundary, or `None` for a tombstone: from
    /// the boundary fusiform serves no curated row for this id.
    pub claim: Option<NewCuratedModel>,
}

/// The reason every tombstone carries.
///
/// The already-tombstoned check reads only whether the newest row is a
/// tombstone, not this text, so rewording it writes nothing new.
pub const CURATED_MODEL_NOT_SERVED: &str = "no longer served by fusiform's curated model file: \
     the file stopped carrying this row, or holds it";

/// What a tombstone cites, since no vendor page stands behind an absence.
pub const CURATED_MODELS_FILE: &str = "crates/fusiform-module/data/curated-models.json";

/// The part of a stored row the boot diff compares: facts and the login
/// allowlist. Provenance is left out on purpose (see `append_curated_models`).
/// The allowlist is compared as a set, since its order means nothing.
type Claim = (serde_json::Value, BTreeSet<String>);

fn claim_of(row: &NewCuratedModel) -> Claim {
    (
        serde_json::to_value(&row.facts).unwrap_or(serde_json::Value::Null),
        row.auth_methods.iter().cloned().collect(),
    )
}

/// The newest stored row for a key: its boundary, and its claim unless it is a
/// tombstone.
type Newest = (i64, Option<Claim>);

/// Decode a JSON column, naming the column when it does not parse.
fn decode<T: serde::de::DeserializeOwned>(
    column: &'static str,
    raw: &str,
) -> Result<T, CatalogError> {
    serde_json::from_str(raw).map_err(|_| CatalogError::Decode {
        column,
        value: raw.to_string(),
    })
}

/// A stored row before its JSON columns are decoded.
struct RawEra {
    provider_id: String,
    model_id: String,
    boundary_at_ms: i64,
    facts_json: Option<String>,
    auth_methods_json: Option<String>,
    source_ref: String,
    established_by: Option<String>,
    established_at_ms: i64,
    review_by_ms: Option<i64>,
}

const RAW_COLUMNS: &str = "provider_id, model_id, boundary_at_ms, facts_json, auth_methods_json, \
     source_ref, established_by, established_at_ms, review_by_ms";

fn raw_era(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawEra> {
    Ok(RawEra {
        provider_id: r.get(0)?,
        model_id: r.get(1)?,
        boundary_at_ms: r.get(2)?,
        facts_json: r.get(3)?,
        auth_methods_json: r.get(4)?,
        source_ref: r.get(5)?,
        established_by: r.get(6)?,
        established_at_ms: r.get(7)?,
        review_by_ms: r.get(8)?,
    })
}

impl RawEra {
    fn decode(self) -> Result<CuratedModelEra, CatalogError> {
        let claim = match (self.facts_json, self.auth_methods_json, self.review_by_ms) {
            (Some(facts), Some(auth_methods), Some(review_by_ms)) => Some(NewCuratedModel {
                provider_id: self.provider_id.clone(),
                model_id: self.model_id.clone(),
                facts: decode("curated_model_era.facts_json", &facts)?,
                auth_methods: decode("curated_model_era.auth_methods_json", &auth_methods)?,
                source_ref: self.source_ref,
                established_by: self.established_by,
                established_at_ms: self.established_at_ms,
                review_by_ms,
            }),
            // The schema's CHECKs make a partial claim unrepresentable, so
            // anything but all three is a tombstone.
            _ => None,
        };
        Ok(CuratedModelEra {
            provider_id: self.provider_id,
            model_id: self.model_id,
            boundary_at_ms: self.boundary_at_ms,
            claim,
        })
    }
}

impl CatalogStore {
    /// Record the curated model file as it stands at this boot.
    ///
    /// `rows` are the rows the file serves; `held` names rows the file carries
    /// but does not serve. `boot_at_ms` is the instant this build started.
    /// Returns how many rows were written.
    ///
    /// # What is written
    ///
    /// For each served row, compared with the newest stored row for its
    /// `(provider_id, model_id)`:
    ///
    /// - nothing stored: the row's first era, dated at its `established_at_ms`,
    ///   the date it was curated;
    /// - a tombstone, or a different claim (facts or login allowlist): a new
    ///   era dated at `boot_at_ms`, because that is when fusiform began serving
    ///   the new value;
    /// - the same claim: nothing.
    ///
    /// Every stored key the file no longer serves, because the row was removed
    /// or is held, gets a tombstone dated at `boot_at_ms` unless its newest row
    /// is already one. A held row with no stored history is tombstoned too, so
    /// that releasing it later dates its first served claim at that release
    /// rather than at a curation date from before the hold.
    ///
    /// A boot dated before the key's newest stored boundary (a clock behind the
    /// record) writes at that boundary instead, so a key's history never runs
    /// backwards; the newer id wins among equal boundaries.
    ///
    /// # Why the claim and not the provenance
    ///
    /// Re-reading a page and re-dating the row moves `established_at_ms`,
    /// `review_by_ms` and often `source_ref` without any served fact changing.
    /// Recording that as an era would make a confirmation indistinguishable
    /// from a price change, the same reason `append_plan_prices` diffs on the
    /// price alone. The stored provenance therefore stays as it was when the
    /// claim was first recorded.
    ///
    /// # Why a dropped key is tombstoned
    ///
    /// In an append-only store, writing nothing for a key the file stopped
    /// carrying would leave its last claim in force forever, served to every
    /// point-in-time read after the removal.
    pub fn append_curated_models(
        &self,
        rows: &[NewCuratedModel],
        held: &[(String, String)],
        boot_at_ms: i64,
    ) -> Result<usize, CatalogError> {
        let served: BTreeSet<(&str, &str)> = rows
            .iter()
            .map(|r| (r.provider_id.as_str(), r.model_id.as_str()))
            .collect();

        let written = self.inner.with_conn_fenced(|tx| {
            // The newest row per key, read inside the fenced transaction so a
            // concurrent apply cannot make this diff act on a stale view.
            let newest = |provider_id: &str, model_id: &str| -> rusqlite::Result<Option<Newest>> {
                tx.query_row(
                    "SELECT boundary_at_ms, facts_json, auth_methods_json \
                     FROM curated_model_era \
                     WHERE provider_id = ?1 AND model_id = ?2 \
                     ORDER BY boundary_at_ms DESC, id DESC LIMIT 1",
                    params![provider_id, model_id],
                    |r| {
                        let boundary: i64 = r.get(0)?;
                        let facts: Option<String> = r.get(1)?;
                        let auth: Option<String> = r.get(2)?;
                        // A stored value that does not parse compares as a
                        // different claim, so the file's row is recorded again
                        // rather than the boot failing.
                        let claim = match (facts, auth) {
                            (Some(facts), Some(auth)) => Some((
                                serde_json::from_str(&facts).unwrap_or(serde_json::Value::Null),
                                serde_json::from_str::<Vec<String>>(&auth)
                                    .map(|v| v.into_iter().collect())
                                    .unwrap_or_default(),
                            )),
                            _ => None,
                        };
                        Ok((boundary, claim))
                    },
                )
                .optional()
            };

            // One write path for claims and tombstones alike: a tombstone is
            // the row with no claim.
            let insert = |provider_id: &str,
                          model_id: &str,
                          boundary_at_ms: i64,
                          claim: Option<&NewCuratedModel>|
             -> rusqlite::Result<()> {
                let facts_json = claim.map(|c| {
                    serde_json::to_string(&c.facts).expect("a JSON map always serializes")
                });
                let auth_json = claim.map(|c| {
                    serde_json::to_string(&c.auth_methods).expect("a string list always serializes")
                });
                tx.execute(
                    "INSERT \
                     INTO curated_model_era \
                     (provider_id, model_id, boundary_at_ms, facts_json, auth_methods_json, \
                      source_ref, established_by, established_at_ms, review_by_ms, \
                      tombstone_reason) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        provider_id,
                        model_id,
                        boundary_at_ms,
                        facts_json,
                        auth_json,
                        claim.map_or(CURATED_MODELS_FILE, |c| c.source_ref.as_str()),
                        claim.map_or(Some("fusiform"), |c| c.established_by.as_deref()),
                        claim.map_or(boundary_at_ms, |c| c.established_at_ms),
                        claim.map(|c| c.review_by_ms),
                        claim.is_none().then_some(CURATED_MODEL_NOT_SERVED),
                    ],
                )?;
                Ok(())
            };

            let mut written = 0usize;
            for row in rows {
                let boundary = match newest(&row.provider_id, &row.model_id)? {
                    None => row.established_at_ms,
                    Some((_, Some(claim))) if claim == claim_of(row) => continue,
                    Some((boundary, _)) => boot_at_ms.max(boundary),
                };
                insert(&row.provider_id, &row.model_id, boundary, Some(row))?;
                written += 1;
            }

            // Every stored key, so one the file no longer serves can be named.
            let mut stmt =
                tx.prepare("SELECT DISTINCT provider_id, model_id FROM curated_model_era")?;
            let mut unserved: BTreeSet<(String, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<_, _>>()?;
            drop(stmt);
            unserved.extend(held.iter().cloned());
            for (provider_id, model_id) in unserved {
                if served.contains(&(provider_id.as_str(), model_id.as_str())) {
                    continue;
                }
                let boundary = match newest(&provider_id, &model_id)? {
                    // Already tombstoned: an ordinary boot, nothing to say.
                    Some((_, None)) => continue,
                    Some((boundary, Some(_))) => boot_at_ms.max(boundary),
                    None => boot_at_ms,
                };
                insert(&provider_id, &model_id, boundary, None)?;
                written += 1;
            }
            Ok(written)
        })?;
        Ok(written)
    }

    /// Every curated row's era in force at `at_ms`, tombstones included, one per
    /// key, ordered by key. Narrowed to one provider when `provider_id` is set.
    ///
    /// The era in force is the one with the newest boundary at or before the
    /// instant, the newest id breaking a tie, matching the write path's view of
    /// "newest".
    pub fn curated_models_at(
        &self,
        at_ms: i64,
        provider_id: Option<&str>,
    ) -> Result<Vec<CuratedModelEra>, CatalogError> {
        let sql = format!(
            "SELECT {RAW_COLUMNS} FROM curated_model_era c \
             WHERE c.boundary_at_ms <= ?1 \
               AND (?2 IS NULL OR c.provider_id = ?2) \
               AND c.id = ( \
                   SELECT d.id FROM curated_model_era d \
                   WHERE d.provider_id = c.provider_id AND d.model_id = c.model_id \
                     AND d.boundary_at_ms <= ?1 \
                   ORDER BY d.boundary_at_ms DESC, d.id DESC LIMIT 1 \
               ) \
             ORDER BY c.provider_id, c.model_id"
        );
        let raw = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![at_ms, provider_id], raw_era)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        raw.into_iter().map(RawEra::decode).collect()
    }

    /// Every stored era for one curated row, oldest first, tombstones included.
    /// Empty when the store has never recorded the row.
    pub fn curated_model_eras(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<Vec<CuratedModelEra>, CatalogError> {
        let sql = format!(
            "SELECT {RAW_COLUMNS} FROM curated_model_era \
             WHERE provider_id = ?1 AND model_id = ?2 \
             ORDER BY boundary_at_ms, id"
        );
        let raw = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![provider_id, model_id], raw_era)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        raw.into_iter().map(RawEra::decode).collect()
    }
}
