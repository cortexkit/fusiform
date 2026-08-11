//! Turning an observation into history.
//!
//! One poll produces one observation row and, if anything moved, a set of era
//! rows. This module decides which facts moved.
//!
//! The decision is a comparison against what the store currently holds, never
//! against the previous fetch's bytes. Comparing bytes would make a reformatted
//! upstream look like a catalog-wide change; comparing normalized values means
//! a change event corresponds to a fact a consumer could act on.
//!
//! # What counts as a change
//!
//! A fact is one `(model identity, fact key)` pair, and each is compared
//! independently. This granularity is load-bearing: if a model were one fact,
//! a provider adjusting a single rate would open an era for every field it
//! publishes, and a consumer asking "when did the cache-read price move" would
//! get the answer for "when did anything about this model move".
//!
//! # Disappearance is a fact, at both levels
//!
//! A model that stops appearing gets a tombstone era under the existence key,
//! never a deleted row. The distinction matters at read time: a deleted row
//! makes a point-in-time query for an instant when the model DID exist return
//! nothing, which reads identically to "never existed".
//!
//! A FACT that stops being published gets a tombstone too, and this is the
//! easier one to miss. A diff that only visits facts present in the new
//! document never sees a withdrawn one, so its last value stays current
//! forever — a provider that stops publishing a rate would leave fusiform
//! serving the old price as though it were still in force. That is worse than
//! serving nothing: a stale number is spendable.

use std::collections::{BTreeMap, BTreeSet};

use fusiform_core::normalize::{NormalizedCatalog, NormalizedModel};
use fusiform_core::{BoundaryKind, RateValue, SourceId, Timestamp};

use crate::{CatalogError, CatalogStore, FactKey, NewEra};

/// Whether a model is present in the source.
///
/// A closed two-state value under the existence key, serialized into the era's
/// value column like any other fact. Presence is history, not a row's absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existence {
    Present,
    /// The source stopped describing this model. Not "deleted" and not
    /// "unavailable" — those are claims about the provider, and fusiform only
    /// observed that a row stopped appearing.
    Absent,
}

impl Existence {
    fn as_json(self) -> &'static str {
        match self {
            Existence::Present => "\"present\"",
            Existence::Absent => "\"absent\"",
        }
    }
}

/// What one ingest would write.
///
/// Produced before anything is written so it can be inspected, counted, and
/// tested without a store mutation — and so a dry run is the same code path as
/// a real one rather than a parallel implementation that drifts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestPlan {
    pub eras: Vec<NewEra>,
    /// Models seen in this document for the first time.
    pub new_models: usize,
    /// Models that were present and no longer appear.
    pub disappeared_models: usize,
    /// Facts whose value moved on a model already known.
    pub changed_facts: usize,
}

impl IngestPlan {
    pub fn is_empty(&self) -> bool {
        self.eras.is_empty()
    }
}

/// Compare a freshly normalized catalog against the store and produce the eras
/// that would record the difference.
///
/// `boundary_kind` is supplied by the caller rather than inferred, because the
/// same diff is correct for a first-ever seed and for an ordinary observed
/// change — what differs is the quality of the boundary instant, and that is a
/// fact about how the document arrived, which this function cannot see.
pub fn plan_ingest(
    store: &CatalogStore,
    catalog: &NormalizedCatalog,
    at: Timestamp,
    boundary_kind: BoundaryKind,
    observation_id: Option<i64>,
) -> Result<IngestPlan, CatalogError> {
    let source = catalog.source;
    let current = store.current_values(source)?;

    let mut eras = Vec::new();
    let mut new_models = 0usize;
    let mut changed_facts = 0usize;
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();

    for model in catalog.models() {
        let identity = (model.key.provider_id.clone(), model.key.model_id.clone());
        seen.insert(identity.clone());

        let facts = facts_of(model);
        let published: BTreeSet<FactKey> = facts.iter().map(|(k, _)| k.clone()).collect();
        let known = current.get(&identity);

        // A model whose existence key is absent from the store, or present with
        // a tombstone, is arriving (or returning). Either way its existence
        // moved and gets an era.
        let was_present = known
            .and_then(|f| f.get(&FactKey::existence()))
            .map(|v| v == Existence::Present.as_json())
            .unwrap_or(false);
        if !was_present {
            new_models += 1;
            eras.push(NewEra {
                source,
                provider_id: identity.0.clone(),
                model_id: identity.1.clone(),
                fact_key: FactKey::existence(),
                value_json: Existence::Present.as_json().to_string(),
                boundary_at: at,
                boundary_kind: boundary_kind.clone(),
                observation_id,
            });
        }

        for (fact_key, value_json) in facts {
            let unchanged = known
                .and_then(|f| f.get(&fact_key))
                .map(|held| *held == value_json)
                .unwrap_or(false);
            if unchanged {
                continue;
            }
            // Only count a change on a model that was already present; a new
            // model's facts are not "changes", they are its first values, and
            // conflating them makes every fresh install look like a mass
            // repricing.
            if was_present {
                changed_facts += 1;
            }
            eras.push(NewEra {
                source,
                provider_id: identity.0.clone(),
                model_id: identity.1.clone(),
                fact_key,
                value_json,
                boundary_at: at,
                boundary_kind: boundary_kind.clone(),
                observation_id,
            });
        }

        // Facts the store holds for this model that the document no longer
        // publishes. Each gets a tombstone naming what the withdrawal means for
        // that kind of fact, so the last published value stops being current.
        if let Some(held) = known {
            for (fact_key, current_value) in held {
                if published.contains(fact_key) || fact_key == &FactKey::existence() {
                    continue;
                }
                let tombstone = withdrawal_value(fact_key);
                if current_value == &tombstone {
                    // Already withdrawn. A second tombstone would claim the
                    // fact was withdrawn twice.
                    continue;
                }
                changed_facts += 1;
                eras.push(NewEra {
                    source,
                    provider_id: identity.0.clone(),
                    model_id: identity.1.clone(),
                    fact_key: fact_key.clone(),
                    value_json: tombstone,
                    boundary_at: at,
                    boundary_kind: boundary_kind.clone(),
                    observation_id,
                });
            }
        }
    }

    // Models the store knows as present that this document does not describe.
    let mut disappeared_models = 0usize;
    for (identity, facts) in &current {
        if seen.contains(identity) {
            continue;
        }
        let currently_present = facts
            .get(&FactKey::existence())
            .map(|v| v == Existence::Present.as_json())
            .unwrap_or(false);
        if !currently_present {
            // Already tombstoned. Writing a second tombstone would claim the
            // model disappeared twice.
            continue;
        }
        disappeared_models += 1;
        eras.push(NewEra {
            source,
            provider_id: identity.0.clone(),
            model_id: identity.1.clone(),
            fact_key: FactKey::existence(),
            value_json: Existence::Absent.as_json().to_string(),
            boundary_at: at,
            boundary_kind: boundary_kind.clone(),
            observation_id,
        });
    }

    Ok(IngestPlan {
        eras,
        new_models,
        disappeared_models,
        changed_facts,
    })
}

/// A digest over exactly the facts this module would store.
///
/// This is the hash consumers are notified on, and computing it from
/// `facts_of` rather than from the document is the point: the change SIGNAL
/// and the era SET are then derived from one function, so they cannot disagree.
/// A digest computed independently — over the raw bytes, or over a
/// re-serialization of the catalog — would drift from the diff the moment
/// either changed, and the failure mode is silent: a consumer told "nothing
/// changed" while eras were written, or woken for a change that produced none.
///
/// It also inherits every normalization decision for free. Sorted modalities,
/// zero limits as absence, the three rate states kept distinct: each is already
/// applied in `facts_of`, so a reformatted upstream produces an identical
/// digest without this function knowing why.
///
/// Fed in sorted order, so the digest depends on the fact set rather than on
/// the order the normalizer happened to emit providers in.
pub fn catalog_digest(catalog: &NormalizedCatalog) -> String {
    let mut entries: Vec<(String, String, String, String)> = Vec::new();
    for model in catalog.models() {
        for (key, value) in facts_of(model) {
            entries.push((
                model.key.provider_id.clone(),
                model.key.model_id.clone(),
                key.as_str().to_string(),
                value,
            ));
        }
    }
    entries.sort();

    let mut hasher = blake3::Hasher::new();
    for (provider, model, key, value) in entries {
        // Length-prefixed rather than delimiter-separated: a provider id
        // containing the delimiter would otherwise let two different fact sets
        // hash identically, and provider ids are upstream-controlled strings.
        for field in [&provider, &model, &key, &value] {
            hasher.update(&(field.len() as u64).to_le_bytes());
            hasher.update(field.as_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// Every fact a model publishes, as `(key, value)` pairs.
///
/// The value is JSON so a fact's shape can evolve without a schema migration,
/// and because era comparison is then string equality on a canonical rendering
/// rather than a per-type comparison that must be written for each new fact.
///
/// Only served facts appear here. Fields that decide HOW a request is spoken to
/// a provider — the SDK adapter, per-model header and body overrides, named
/// experimental modes — are deliberately absent, because fusiform describes what
/// models are and never how to talk to them. Their absence is structural rather
/// than a rule someone must remember: they are not in this function, so no code
/// path can write them into an era, and a field added to the normalizer later
/// does not silently acquire history.
fn facts_of(model: &NormalizedModel) -> Vec<(FactKey, String)> {
    let mut facts = vec![
        // Limits. `null` is a real value here: a limit that stops being
        // published is a change, and encoding it as absence would make the era
        // disappear rather than record that the upstream stopped saying.
        (
            FactKey::limit("context"),
            json_opt_u64(model.limits.context_tokens),
        ),
        (
            FactKey::limit("output"),
            json_opt_u64(model.limits.output_tokens),
        ),
        // Capabilities that a consumer branches on.
        (
            FactKey::capability("reasoning"),
            json_opt_bool(model.capabilities.reasoning),
        ),
        (
            FactKey::capability("tool_call"),
            json_opt_bool(model.capabilities.tool_call),
        ),
        (
            FactKey::capability("attachment"),
            json_opt_bool(model.capabilities.attachment),
        ),
        (
            FactKey::capability("input_modalities"),
            json_modalities(&model.capabilities.input_modalities),
        ),
        (
            FactKey::capability("output_modalities"),
            json_modalities(&model.capabilities.output_modalities),
        ),
    ];

    // Rates, keyed per token class and condition. A rate row's key carries
    // every discriminator needed to select it, so a consumer never joins
    // capability rows to price rows.
    for rate in &model.rates {
        if let fusiform_core::ChargeBasis::PerMillionTokens { class } = rate.basis {
            let key = match rate.condition {
                fusiform_core::RateCondition::Always => FactKey::rate(class),
                fusiform_core::RateCondition::MinContextTokens { tokens } => {
                    FactKey::rate_above_context(class, tokens)
                }
            };
            facts.push((key, json_rate(&rate.value)));
        }
    }

    facts
}

/// What it means for a fact of this kind to stop being published.
///
/// Not one tombstone value for everything, because the withdrawal means
/// different things. A rate that vanishes is `Unpriced(MissingRate)` — the
/// source describes the model and publishes no rate for it, which is precisely
/// that variant's definition, and it is emphatically not zero. A limit or
/// capability that vanishes is `null`, the same value the normalizer already
/// emits for "not stated", so a withdrawal and a never-stated field agree
/// rather than being two encodings of one condition.
fn withdrawal_value(key: &FactKey) -> String {
    if key.as_str().starts_with("rate.") {
        json_rate(&RateValue::Unpriced {
            reason: fusiform_core::UnpricedReason::MissingRate,
        })
    } else {
        "null".to_string()
    }
}

fn json_opt_u64(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

fn json_opt_bool(v: Option<bool>) -> String {
    match v {
        Some(b) => b.to_string(),
        None => "null".to_string(),
    }
}

fn json_modalities(mods: &[fusiform_core::Modality]) -> String {
    // Sorted so a reordering upstream does not read as a capability change. The
    // upstream's array order is not information — it carries no ranking — and
    // treating it as information would manufacture change events.
    let mut names: Vec<String> = mods.iter().map(modality_name).collect();
    names.sort();
    serde_json::to_string(&names).expect("a list of strings always serializes")
}

fn modality_name(m: &fusiform_core::Modality) -> String {
    use fusiform_core::Modality::*;
    match m {
        Text => "text".to_string(),
        Image => "image".to_string(),
        Audio => "audio".to_string(),
        Video => "video".to_string(),
        Pdf => "pdf".to_string(),
        Other(s) => s.clone(),
    }
}

/// Render a rate for storage, keeping the three value states distinct.
///
/// A priced amount, a stated zero, and an unpriced reason must render
/// differently, because the whole point of the three-state design is that a
/// consumer can tell them apart. Rendering a stated zero as `0` would make it
/// indistinguishable from a priced zero, which the money boundary refuses to
/// produce precisely so this distinction survives.
fn json_rate(value: &RateValue) -> String {
    match value {
        RateValue::Priced { amount } => format!(
            r#"{{"state":"priced","units":{},"exponent":{},"currency":"{}"}}"#,
            amount.units,
            amount.exponent,
            amount.currency.as_str()
        ),
        RateValue::StatedZero => r#"{"state":"stated_zero"}"#.to_string(),
        RateValue::Unpriced { reason } => format!(
            r#"{{"state":"unpriced","reason":"{}"}}"#,
            unpriced_reason_str(*reason)
        ),
    }
}

fn unpriced_reason_str(r: fusiform_core::UnpricedReason) -> &'static str {
    use fusiform_core::UnpricedReason::*;
    match r {
        MissingRate => "missing_rate",
        NoCatalogCoverage => "no_catalog_coverage",
        UnknownChargeBasis => "unknown_charge_basis",
    }
}

/// Every fact's current value, keyed by model identity then fact.
///
/// Named rather than written inline: the shape appears in a public signature
/// and in the diff loop, and a nested-map type spelled out twice is two places
/// to change when it moves.
pub type CurrentValues = BTreeMap<(String, String), BTreeMap<FactKey, String>>;

/// The read behind [`CatalogStore::current_values`], run on every poll.
///
/// Public so a test can assert on the query the store actually prepares. A test
/// carrying its own copy of this SQL would verify that the COPY is
/// index-backed, and the two could drift apart without anything failing.
///
/// The correlated MAX is chosen over a grouped join, and the choice is
/// measured rather than assumed. On a 126,048-row store (2026-08-11, release
/// build, the two queries interleaved to remove cold-cache ordering bias):
/// correlated 45ms, grouped join 77ms. A first, non-interleaved measurement
/// showed the opposite and was wrong.
///
/// What makes it a difference in KIND rather than tuning: the correlated form
/// resolves each per-fact MAX from a covering index, while the same query
/// written so SQLite cannot use that index did not finish in 300 seconds on
/// the same data.
pub const CURRENT_VALUES_SQL: &str = "SELECT e.provider_id, e.model_id, e.fact_key, e.value_json \
     FROM era e \
     WHERE e.source = ?1 \
       AND e.boundary_at_ms = ( \
           SELECT MAX(e2.boundary_at_ms) FROM era e2 \
           WHERE e2.source = e.source \
             AND e2.provider_id = e.provider_id \
             AND e2.model_id = e.model_id \
             AND e2.fact_key = e.fact_key \
       )";

impl CatalogStore {
    /// Every fact's current value, for one source.
    ///
    /// "Current" is the era with the greatest boundary for each fact, computed
    /// in the query rather than tracked in a flag. A flag needs a write on
    /// every change and is wrong from the moment one of those writes is missed;
    /// a MAX cannot disagree with itself.
    pub fn current_values(&self, source: SourceId) -> Result<CurrentValues, CatalogError> {
        let rows = self.with_conn_rows(source)?;
        let mut out: CurrentValues = BTreeMap::new();
        for (provider_id, model_id, fact_key, value_json) in rows {
            out.entry((provider_id, model_id))
                .or_default()
                .insert(FactKey::from_stored(fact_key), value_json);
        }
        Ok(out)
    }

    fn with_conn_rows(
        &self,
        source: SourceId,
    ) -> Result<Vec<(String, String, String, String)>, CatalogError> {
        let rows = self.raw_conn(|conn| {
            let mut stmt = conn.prepare(CURRENT_VALUES_SQL)?;
            let mapped = stmt.query_map(rusqlite::params![source.as_str()], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(rows)
    }
}
