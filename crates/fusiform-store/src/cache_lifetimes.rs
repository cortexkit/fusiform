//! Append-only history of prompt-cache lifetime rules.

use std::collections::BTreeSet;

use rusqlite::{params, OptionalExtension};

use crate::{CatalogError, CatalogStore};

/// A loaded lifetime rule for one provider/model pair.
#[derive(Clone, Debug, PartialEq)]
pub struct NewCacheLifetime {
    pub provider_id: String,
    pub model_id: String,
    pub rule: serde_json::Value,
}

/// A rule in force from its boundary, or a tombstone withdrawing it.
#[derive(Clone, Debug, PartialEq)]
pub struct CacheLifetimeEra {
    pub provider_id: String,
    pub model_id: String,
    pub boundary_at_ms: i64,
    pub rule: Option<serde_json::Value>,
}

type Newest = (i64, Option<serde_json::Value>);

fn per_model_claim(model_id: &str, mut rule: serde_json::Value) -> serde_json::Value {
    if let Some(fields) = rule.as_object_mut() {
        // Group membership is not part of a model's claim. Keep the field
        // with only this row's model so older readers can still decode it.
        fields.insert("model_ids".into(), serde_json::json!([model_id]));
    }
    rule
}

impl CatalogStore {
    /// All cache-lifetime eras for one model, oldest first, including tombstones.
    pub fn model_cache_lifetime_eras(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<Vec<CacheLifetimeEra>, CatalogError> {
        let raw = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT provider_id, model_id, boundary_at_ms, rule_json
                 FROM cache_lifetime_era WHERE provider_id = ?1 AND model_id = ?2
                 ORDER BY boundary_at_ms, id",
            )?;
            let rows = stmt.query_map(params![provider, model], raw_era)?;
            rows.collect::<Result<Vec<_>, _>>()
        })?;
        raw.into_iter().map(decode_era).collect()
    }

    /// Record changed, added and removed compiled rules at boot. Unlike the
    /// frozen migration backfill, new rules start when first served, not at a
    /// source's establishment date. Clock regressions cannot reorder history;
    /// newest id wins when boundaries tie. Returns the number of appended eras.
    pub fn append_cache_lifetimes(
        &self,
        rows: &[NewCacheLifetime],
        boot_at_ms: i64,
    ) -> Result<usize, CatalogError> {
        let served: BTreeSet<_> = rows
            .iter()
            .map(|r| (r.provider_id.as_str(), r.model_id.as_str()))
            .collect();
        Ok(self.inner.with_conn_fenced(|tx| {
            let newest = |provider: &str, model: &str| -> rusqlite::Result<Option<Newest>> {
                tx.query_row(
                    "SELECT boundary_at_ms, rule_json FROM cache_lifetime_era \
                     WHERE provider_id = ?1 AND model_id = ?2 \
                     ORDER BY boundary_at_ms DESC, id DESC LIMIT 1",
                    params![provider, model],
                    |r| {
                        let rule: Option<String> = r.get(1)?;
                        let claim = rule
                            .and_then(|v| serde_json::from_str(&v).ok())
                            .map(|rule| per_model_claim(model, rule));
                        Ok((r.get(0)?, claim))
                    },
                )
                .optional()
            };
            // Rules and tombstones share one fenced INSERT site.
            let insert = |provider: &str,
                          model: &str,
                          boundary: i64,
                          rule: Option<&serde_json::Value>|
             -> rusqlite::Result<()> {
                tx.execute(
                    "INSERT INTO cache_lifetime_era \
                     (provider_id, model_id, boundary_at_ms, rule_json, recorded_at_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        provider,
                        model,
                        boundary,
                        rule.map(|r| r.to_string()),
                        boot_at_ms
                    ],
                )?;
                Ok(())
            };
            let mut written = 0;
            for row in rows {
                let claim = per_model_claim(&row.model_id, row.rule.clone());
                let boundary = match newest(&row.provider_id, &row.model_id)? {
                    Some((_, Some(previous))) if previous == claim => continue,
                    Some((boundary, _)) => boot_at_ms.max(boundary),
                    None => boot_at_ms,
                };
                insert(&row.provider_id, &row.model_id, boundary, Some(&claim))?;
                written += 1;
            }
            let mut stmt =
                tx.prepare("SELECT DISTINCT provider_id, model_id FROM cache_lifetime_era")?;
            let keys = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);
            for (provider, model) in keys {
                if served.contains(&(provider.as_str(), model.as_str())) {
                    continue;
                }
                if let Some((boundary, Some(_))) = newest(&provider, &model)? {
                    insert(&provider, &model, boot_at_ms.max(boundary), None)?;
                    written += 1;
                }
            }
            Ok(written)
        })?)
    }

    /// One era in force per provider/model, including tombstones. Before the
    /// first serving boundary there is no rule to apply to upstream facts.
    pub fn cache_lifetimes_at(
        &self,
        at_ms: i64,
        provider_id: Option<&str>,
    ) -> Result<Vec<CacheLifetimeEra>, CatalogError> {
        let raw = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT provider_id, model_id, boundary_at_ms, rule_json \
                 FROM cache_lifetime_era c WHERE c.boundary_at_ms <= ?1 \
                   AND (?2 IS NULL OR c.provider_id = ?2) \
                   AND c.id = (SELECT d.id FROM cache_lifetime_era d \
                       WHERE d.provider_id = c.provider_id AND d.model_id = c.model_id \
                         AND d.boundary_at_ms <= ?1 \
                       ORDER BY d.boundary_at_ms DESC, d.id DESC LIMIT 1) \
                 ORDER BY c.provider_id, c.model_id",
            )?;
            let rows = stmt
                .query_map(params![at_ms, provider_id], raw_era)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        raw.into_iter().map(decode_era).collect()
    }
}

type RawEra = (String, String, i64, Option<String>);

fn raw_era(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawEra> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
}

fn decode_era(
    (provider_id, model_id, boundary_at_ms, rule): RawEra,
) -> Result<CacheLifetimeEra, CatalogError> {
    let rule = rule
        .map(|value| {
            serde_json::from_str(&value).map_err(|_| CatalogError::Decode {
                column: "cache_lifetime_era.rule_json",
                value,
            })
        })
        .transpose()?;
    Ok(CacheLifetimeEra {
        provider_id,
        model_id,
        boundary_at_ms,
        rule,
    })
}
