//! Append-only history of guarded, sourced fact overrides.

use std::collections::BTreeSet;

use rusqlite::{params, OptionalExtension};

use crate::{CatalogError, CatalogStore};

/// A loaded override as the store records it, separate from loader metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct NewFactOverride {
    pub provider_id: String,
    pub model_id: String,
    pub fact_key: String,
    pub value: serde_json::Value,
    /// The file's upstream guard. A null is interpreted by fact key: additions
    /// require absence, while model.status replacements require upstream null.
    pub replaces: serde_json::Value,
    pub reason: String,
    pub source_ref: String,
    pub quote: String,
    pub notes: Option<String>,
    pub established_by: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

/// A claim in force from its boundary, or a tombstone withdrawing the claim.
#[derive(Debug, Clone, PartialEq)]
pub struct FactOverrideEra {
    pub provider_id: String,
    pub model_id: String,
    pub fact_key: String,
    pub boundary_at_ms: i64,
    pub claim: Option<NewFactOverride>,
}

type Claim = (serde_json::Value, serde_json::Value);
type Newest = (i64, Option<Claim>);

impl CatalogStore {
    /// Diff the compiled overrides at boot, retaining original provenance when
    /// only source wording or review dates change. As for curated model rows,
    /// the first era starts at establishment; changes, reappearances and
    /// removals start at boot, clamped to the newest boundary if the clock is
    /// behind. Newest id wins ties. Returns the number of appended eras.
    pub fn append_fact_overrides(
        &self,
        rows: &[NewFactOverride],
        boot_at_ms: i64,
    ) -> Result<usize, CatalogError> {
        let served: BTreeSet<_> = rows
            .iter()
            .map(|r| {
                (
                    r.provider_id.as_str(),
                    r.model_id.as_str(),
                    r.fact_key.as_str(),
                )
            })
            .collect();
        let written = self.inner.with_conn_fenced(|tx| {
            let newest =
                |provider: &str, model: &str, key: &str| -> rusqlite::Result<Option<Newest>> {
                    tx.query_row(
                        "SELECT boundary_at_ms, value_json, replaces_json FROM fact_override_era \
                     WHERE provider_id = ?1 AND model_id = ?2 AND fact_key = ?3 \
                     ORDER BY boundary_at_ms DESC, id DESC LIMIT 1",
                        params![provider, model, key],
                        |r| {
                            let value: Option<String> = r.get(1)?;
                            let replaces: Option<String> = r.get(2)?;
                            // Invalid JSON compares as a different claim so a boot
                            // can record the compiled claim again, as curated rows do.
                            let claim = value.and_then(|v| {
                                let value = serde_json::from_str(&v).ok()?;
                                let replaces = match replaces {
                                    Some(v) => serde_json::from_str(&v).ok()?,
                                    None => serde_json::Value::Null,
                                };
                                Some((value, replaces))
                            });
                            Ok((r.get(0)?, claim))
                        },
                    )
                    .optional()
                };
            // Claims and tombstones share one fenced INSERT site.
            let insert = |provider: &str,
                          model: &str,
                          key: &str,
                          boundary: i64,
                          claim: Option<&NewFactOverride>|
             -> rusqlite::Result<()> {
                let value = claim.map(|c| c.value.to_string());
                let replaces = claim
                    .filter(|c| !c.replaces.is_null())
                    .map(|c| c.replaces.to_string());
                tx.execute(
                    "INSERT INTO fact_override_era \
                     (provider_id, model_id, fact_key, boundary_at_ms, value_json, replaces_json, \
                      reason, source_ref, quote, notes, established_by, established_at_ms, \
                      review_by_ms, tombstone_reason) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        provider,
                        model,
                        key,
                        boundary,
                        value,
                        replaces,
                        claim.map(|c| c.reason.as_str()),
                        claim.map_or("crates/fusiform-module/data/fact-overrides.json", |c| c
                            .source_ref
                            .as_str()),
                        claim.map(|c| c.quote.as_str()),
                        claim.and_then(|c| c.notes.as_deref()),
                        claim.map_or("fusiform", |c| c.established_by.as_str()),
                        claim.map_or(boundary, |c| c.established_at_ms),
                        claim.map(|c| c.review_by_ms),
                        claim
                            .is_none()
                            .then_some("no longer served by fusiform's fact override file")
                    ],
                )?;
                Ok(())
            };
            let mut written = 0;
            for row in rows {
                let boundary = match newest(&row.provider_id, &row.model_id, &row.fact_key)? {
                    None => row.established_at_ms,
                    Some((_, Some(claim)))
                        if claim == (row.value.clone(), row.replaces.clone()) =>
                    {
                        continue
                    }
                    Some((boundary, _)) => boot_at_ms.max(boundary),
                };
                insert(
                    &row.provider_id,
                    &row.model_id,
                    &row.fact_key,
                    boundary,
                    Some(row),
                )?;
                written += 1;
            }
            let mut stmt = tx.prepare(
                "SELECT DISTINCT provider_id, model_id, fact_key FROM fact_override_era",
            )?;
            let keys = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);
            for (provider, model, key) in keys {
                if served.contains(&(provider.as_str(), model.as_str(), key.as_str())) {
                    continue;
                }
                if let Some((boundary, Some(_))) = newest(&provider, &model, &key)? {
                    insert(&provider, &model, &key, boot_at_ms.max(boundary), None)?;
                    written += 1;
                }
            }
            Ok(written)
        })?;
        Ok(written)
    }

    /// One era in force per override key, tombstones included. Reads before a
    /// key's first boundary return no era, leaving upstream facts untouched.
    pub fn fact_overrides_at(
        &self,
        at_ms: i64,
        provider_id: Option<&str>,
    ) -> Result<Vec<FactOverrideEra>, CatalogError> {
        let raw = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT provider_id, model_id, fact_key, boundary_at_ms, value_json, replaces_json, \
                        reason, source_ref, quote, notes, established_by, established_at_ms, review_by_ms \
                 FROM fact_override_era c WHERE c.boundary_at_ms <= ?1 \
                   AND (?2 IS NULL OR c.provider_id = ?2) \
                   AND c.id = (SELECT d.id FROM fact_override_era d \
                       WHERE d.provider_id = c.provider_id AND d.model_id = c.model_id \
                         AND d.fact_key = c.fact_key AND d.boundary_at_ms <= ?1 \
                       ORDER BY d.boundary_at_ms DESC, d.id DESC LIMIT 1) \
                 ORDER BY c.provider_id, c.model_id, c.fact_key",
            )?;
            let rows = stmt.query_map(params![at_ms, provider_id], |r| {
                Ok(RawEra {
                    provider: r.get(0)?, model: r.get(1)?, key: r.get(2)?, boundary: r.get(3)?,
                    value: r.get(4)?, replaces: r.get(5)?, reason: r.get(6)?, source: r.get(7)?,
                    quote: r.get(8)?, notes: r.get(9)?, established_by: r.get(10)?,
                    established_at: r.get(11)?, review_by: r.get(12)?,
                })
            })?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        raw.into_iter().map(RawEra::decode).collect()
    }
}

struct RawEra {
    provider: String,
    model: String,
    key: String,
    boundary: i64,
    value: Option<String>,
    replaces: Option<String>,
    reason: Option<String>,
    source: String,
    quote: Option<String>,
    notes: Option<String>,
    established_by: String,
    established_at: i64,
    review_by: Option<i64>,
}

impl RawEra {
    fn decode(self) -> Result<FactOverrideEra, CatalogError> {
        let decode = |column, raw: &str| {
            serde_json::from_str(raw).map_err(|_| CatalogError::Decode {
                column,
                value: raw.to_string(),
            })
        };
        let claim = match (self.value, self.reason, self.quote, self.review_by) {
            (Some(value), Some(reason), Some(quote), Some(review_by_ms)) => Some(NewFactOverride {
                provider_id: self.provider.clone(),
                model_id: self.model.clone(),
                fact_key: self.key.clone(),
                value: decode("fact_override_era.value_json", &value)?,
                replaces: self
                    .replaces
                    .map(|v| decode("fact_override_era.replaces_json", &v))
                    .transpose()?
                    .unwrap_or(serde_json::Value::Null),
                reason,
                source_ref: self.source,
                quote,
                notes: self.notes,
                established_by: self.established_by,
                established_at_ms: self.established_at,
                review_by_ms,
            }),
            _ => None,
        };
        Ok(FactOverrideEra {
            provider_id: self.provider,
            model_id: self.model,
            fact_key: self.key,
            boundary_at_ms: self.boundary,
            claim,
        })
    }
}
