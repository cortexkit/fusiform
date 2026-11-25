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

use fusiform_core::normalize::{NormalizedCatalog, NormalizedModel, RefusedFactScope};
use fusiform_core::{BoundaryKind, RateValue, SourceId, Timestamp};

use crate::{CatalogError, CatalogStore, FactKey, NewEra};

pub use fusiform_core::upstream_facts::Existence;
use fusiform_core::upstream_facts::{facts_of, json_rate, FactConflict};

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
    store.raw_conn(|conn| plan_ingest_in(conn, catalog, at, boundary_kind, observation_id))
}

pub(crate) fn plan_ingest_in(
    conn: &rusqlite::Connection,
    catalog: &NormalizedCatalog,
    at: Timestamp,
    boundary_kind: BoundaryKind,
    observation_id: Option<i64>,
) -> rusqlite::Result<IngestPlan> {
    let current = current_values_in(conn, catalog.source)?;
    let models = catalog.models().map(|model| {
        let identity = (model.key.provider_id.clone(), model.key.model_id.clone());
        (identity, facts_of(model).values, Some(&model.refused))
    });
    Ok(plan_model_eras(
        catalog.source,
        &current,
        models,
        at,
        boundary_kind,
        observation_id,
    ))
}

type ModelIdentity = (String, String);
type ProjectedFacts = Vec<(FactKey, String)>;
type ProjectedModel<'a> = (
    ModelIdentity,
    ProjectedFacts,
    Option<&'a BTreeSet<RefusedFactScope>>,
);

/// Complete facts for models touched by a release. `None` means the model is
/// absent; models outside the map were not observed to change in this release.
pub type TouchedModels = BTreeMap<ModelIdentity, Option<ProjectedFacts>>;

pub(crate) fn plan_release_ingest(
    conn: &rusqlite::Connection,
    models: &TouchedModels,
    at: Timestamp,
    observation_id: i64,
) -> rusqlite::Result<IngestPlan> {
    let source = SourceId::ModelsDev;
    let mut current = CurrentValues::new();
    let sql = format!("{CURRENT_VALUES_SQL} AND e.provider_id = ?2 AND e.model_id = ?3");
    let mut stmt = conn.prepare_cached(&sql)?;
    for identity in models.keys() {
        let mut rows = stmt.query(rusqlite::params![source.as_str(), identity.0, identity.1])?;
        while let Some(row) = rows.next()? {
            current
                .entry(identity.clone())
                .or_default()
                .insert(FactKey::from_stored(row.get(2)?), row.get(3)?);
        }
    }
    let present = models.iter().filter_map(|(identity, facts)| {
        facts
            .as_ref()
            .map(|facts| (identity.clone(), facts.clone(), None))
    });
    Ok(plan_model_eras(
        source,
        &current,
        present,
        at,
        BoundaryKind::Observed,
        Some(observation_id),
    ))
}

fn plan_model_eras<'a>(
    source: SourceId,
    current: &CurrentValues,
    models: impl IntoIterator<Item = ProjectedModel<'a>>,
    at: Timestamp,
    boundary_kind: BoundaryKind,
    observation_id: Option<i64>,
) -> IngestPlan {
    let mut eras = Vec::new();
    let mut new_models = 0usize;
    let mut changed_facts = 0usize;
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();

    for (identity, facts, refused) in models {
        seen.insert(identity.clone());

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
            // Near-identical to the withdrawal era below — one value differs — and
            // deliberately not extracted.
            //
            // The drift a helper would prevent is already compile-caught: a new
            // field on `NewEra` breaks BOTH struct literals (E0063), which is
            // how the domain-to-wire fences elsewhere in this workspace work.
            // What a helper would NOT prevent is a semantic change at one site,
            // and that is the divergence these two are allowed to have: an
            // arrival and a withdrawal could legitimately want different
            // boundary kinds one day, and routing both through one call makes
            // that harder to express rather than safer.
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
                .map(|held| states_the_same_upstream_claim(held, &value_json))
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
                // No claim was read for these keys, so keep the held value
                // and its original boundary rather than writing a tombstone.
                if refused.is_some_and(|scopes| scopes.iter().any(|scope| scope.covers(fact_key))) {
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
    for (identity, facts) in current {
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

    IngestPlan {
        eras,
        new_models,
        disappeared_models,
        changed_facts,
    }
}

/// A digest over exactly the facts this module would store.
///
/// The poll loop compares this digest to skip diffing a matching document.
/// The digest and the diff both use `facts_of` for the incoming projection.
/// If a diff runs, only written fact changes advance the catalog version.
///
/// It also inherits every normalization decision for free. Sorted modalities,
/// zero limits as absence, the three rate states kept distinct: each is already
/// applied in `facts_of`, so a reformatted upstream produces an identical
/// digest without this function knowing why.
///
/// Fed in sorted order, so the digest depends on the fact set rather than on
/// the order the normalizer happened to emit providers in.
pub fn catalog_digest(catalog: &NormalizedCatalog) -> String {
    catalog_digest_with_conflicts(catalog).0
}

/// Hash the upstream facts and return the conflicting keys the projection omitted.
pub fn catalog_digest_with_conflicts(catalog: &NormalizedCatalog) -> (String, Vec<FactConflict>) {
    // Sort only model references. Fact tuples live for one model identity at a
    // time instead of retaining a second copy of the entire catalog.
    let mut models: Vec<_> = catalog.models().collect();
    models.sort_unstable_by(|a, b| {
        (&a.key.provider_id, &a.key.model_id).cmp(&(&b.key.provider_id, &b.key.model_id))
    });
    let mut hasher = blake3::Hasher::new();
    let mut conflicts = Vec::new();
    for group in models
        .chunk_by(|a, b| a.key.provider_id == b.key.provider_id && a.key.model_id == b.key.model_id)
    {
        let model = group[0];
        // Sort duplicate identities' facts together too, even for a catalog
        // constructed outside the normalizer.
        let mut facts: Vec<_> = group
            .iter()
            .flat_map(|model| {
                let projected = facts_of(model);
                conflicts.extend(projected.conflicts);
                projected.values
            })
            .collect();
        facts.sort();
        for (key, value) in facts {
            hash_projection(&mut hasher, model, key.as_str(), &value);
        }
        // Otherwise an unreadable rate and a later absent cost block could
        // compare equal, hiding a real withdrawal on the digest fast path.
        for scope in group
            .iter()
            .flat_map(|model| &model.refused)
            .collect::<BTreeSet<_>>()
        {
            let (tag, value) = match scope {
                RefusedFactScope::Model => ("!refused-model", ""),
                RefusedFactScope::Fact(key) => ("!refused-fact", key.as_str()),
                RefusedFactScope::BaseAndTierRates => ("!refused-base-and-tier", ""),
                RefusedFactScope::ModeRates(None) => ("!refused-all-modes", ""),
                RefusedFactScope::ModeRates(Some(mode)) => ("!refused-mode", mode.as_str()),
            };
            hash_projection(&mut hasher, model, tag, value);
        }
    }
    (hasher.finalize().to_hex().to_string(), conflicts)
}

fn hash_projection(hasher: &mut blake3::Hasher, model: &NormalizedModel, key: &str, value: &str) {
    // Length-prefix upstream IDs too, since any separator can occur in them.
    for field in [
        model.key.provider_id.as_str(),
        model.key.model_id.as_str(),
        key,
        value,
    ] {
        hasher.update(&(field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
}

/// The complete set of fact keys a model contributes to the store.
///
/// Public so a test can assert the SET rather than spot-check members. The fact
/// set is fusiform's served vocabulary: a field silently joining it is a
/// contract change no consumer asked for, and a field silently leaving it makes
/// a consumer's read start returning nothing.
///
/// Does NOT include `existence`, which [`plan_ingest`] writes from its own
/// presence logic rather than from a model's fields. Callers wanting every
/// served fact want [`SERVED_FACT_NAMESPACE`] plus this.
pub fn fact_keys_of(model: &NormalizedModel) -> Vec<FactKey> {
    facts_of(model).values.into_iter().map(|(k, _)| k).collect()
}

/// Every fact key fusiform serves, as a closed set.
///
/// One definition, because there were two. This list lived in a test file while
/// another test derived "the served facts" from [`fact_keys_of`], which omits
/// `existence` — so a coverage assertion silently skipped the one fact whose
/// correction expresses a model being wrongly recorded as present. A mutation
/// mapping `FieldId::Existence` to the wrong key survived because of it.
///
/// Tiered and mode rate keys are not listed: they carry an upstream threshold
/// or mode name and are checked structurally, since neither is fusiform's to
/// enumerate.
pub const SERVED_FACT_NAMESPACE: &[&str] = &[
    "existence",
    "model.name",
    "provider.name",
    // Artefact facts: what the model IS, independent of who serves it. Two
    // providers serving the same weights agree on these and disagree on rates,
    // which is what makes them the join a consumer needs to relate rows.
    "model.family",
    "model.open_weights",
    "model.canonical_id",
    // A lifecycle label is upstream's claim about this row, not proof that
    // sibling providers have the same availability or lifecycle.
    "model.status",
    "model.type",
    "wire.npm",
    "wire.api",
    "wire.shape",
    "limit.context",
    "limit.output",
    "capability.reasoning",
    "capability.reasoning_options",
    "capability.structured_output",
    "capability.temperature",
    "capability.tool_call",
    "capability.attachment",
    "capability.input_modalities",
    "capability.output_modalities",
    "rate.input",
    "rate.output",
    "rate.cache_read",
    "rate.cache_write",
    "rate.reasoning",
    // Audio input per million tokens, produced only for providers whose own
    // pricing documentation establishes per-token audio billing.
    "rate.input_audio",
    "rate.output_audio",
];

/// The facts a model contributes, as `(key, JSON value)` pairs.
///
/// JSON lets a fact's value evolve without a database migration. Published
/// wire facts (`wire.npm`, `wire.api` and `wire.shape`) are kept verbatim. HTTP
/// headers and request body overrides are omitted because consumers build the
/// requests they send.
///
/// Public for the same reason as [`fact_keys_of`], one level down: a test that
/// asserts on values it renders itself proves nothing about what is stored. The
/// modality distinction — `null` for unpublished, `[]` for published-empty —
/// survived in the domain type while the storage boundary flattened it, and no
/// test could see that because none could reach the stored value.
pub fn facts_with_values(model: &NormalizedModel) -> Vec<(FactKey, String)> {
    facts_of(model).values
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
            inherited_from: None,
        })
    } else {
        "null".to_string()
    }
}

/// Whether two stored values state the same thing ABOUT THE UPSTREAM.
///
/// # Why this is not string equality, measured the expensive way
///
/// It was, until 2026-08-16. Adding `unit_provenance` to the stored rate
/// representation made every stored rate differ textually from every freshly
/// normalized one, so the first poll after that binary shipped wrote 17,455
/// eras — one per priced rate — each with `boundary_kind = observed` and an
/// observation window implying the provider had moved its price.
///
/// Nothing had moved. `anthropic/claude-sonnet-4-5` `rate.input` reads
/// `units: 3000000000` on both sides of that boundary. The store recorded
/// fusiform's own serialization change as an upstream event, in a history
/// whose entire purpose is to say what the upstream did.
///
/// # The rule
///
/// An era boundary is a claim about the SOURCE. Fields fusiform adds are
/// annotations about fusiform's own handling, and they must never open one.
/// `unit_provenance` is the case in hand: the upstream publishes no currency
/// at all, so provenance describes a policy this code applied, and a change to
/// that policy is a change to fusiform rather than to the provider.
///
/// Compared as parsed JSON rather than text so key order and whitespace cannot
/// open a boundary either — the hazard this file's own opening comment warns
/// about for the upstream payload, eleven lines above the code that had it.
pub fn states_the_same_upstream_claim(held: &str, incoming: &str) -> bool {
    if held == incoming {
        return true;
    }
    let (Ok(a), Ok(b)) = (
        serde_json::from_str::<serde_json::Value>(held),
        serde_json::from_str::<serde_json::Value>(incoming),
    ) else {
        // Unparseable on either side: fall back to the literal comparison
        // already made above, which said they differ.
        return false;
    };
    upstream_claim_of(a) == upstream_claim_of(b)
}

/// A stored value with everything fusiform added to it removed.
///
/// The diff uses this so historical producer annotations do not make unchanged
/// upstream values look changed. The digest hashes the incoming projected
/// strings directly; producer annotations must stay out of that projection.
pub fn upstream_claim_of(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = value.as_object_mut() {
        for annotation in PRODUCER_ANNOTATIONS {
            obj.remove(*annotation);
        }
    }
    value
}

/// Fields fusiform attaches to a stored value that are NOT the upstream's
/// claim, and so must not count as changes when comparing stored values.
///
/// `unit_provenance` is here from the incident of 2026-08-16: it was written
/// into storage for one day, and because the diff compared serialized strings
/// the first poll afterwards recorded a price change for all 17,455 priced
/// rates. Storage no longer carries it — the serve path attaches it — so this
/// list guards against the next one rather than the last one.
const PRODUCER_ANNOTATIONS: &[&str] = &["unit_provenance"];

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
    /// Resolve each per-fact maximum in SQL and insert rows directly into the
    /// model maps; a second catalog-sized row buffer would only be discarded.
    pub fn current_values(&self, source: SourceId) -> Result<CurrentValues, CatalogError> {
        self.raw_conn(|conn| current_values_in(conn, source))
    }
}

fn current_values_in(
    conn: &rusqlite::Connection,
    source: SourceId,
) -> rusqlite::Result<CurrentValues> {
    let mut out = BTreeMap::new();
    let sql = format!("{CURRENT_VALUES_SQL} ORDER BY e.provider_id, e.model_id, e.fact_key");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params![source.as_str()])?;
    while let Some(row) = rows.next()? {
        crate::serve::insert_streamed_fact(
            &mut out,
            crate::serve::row_text(row, 0)?,
            crate::serve::row_text(row, 1)?,
            FactKey::from_stored(row.get(2)?),
            row.get(3)?,
        );
    }
    Ok(out)
}
