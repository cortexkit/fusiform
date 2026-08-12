//! Recording that fusiform's own record was wrong.
//!
//! # What an operator supplies, and what they must not
//!
//! A correction is a human diagnosis. Only a person knows that a parser
//! misread a field, which facts it touched, and when the bad reading started
//! and stopped. So the operator supplies **which facts, what window, and why**.
//!
//! They do **not** supply a value. That is the whole design of this module.
//!
//! The reason is in the design note's definition of the interval:
//! `affected_until` is *when the fix deployed and fusiform stopped recording
//! the bad value*. By the time anyone writes a correction, the parser is fixed
//! and a subsequent poll has already re-normalized the document and written the
//! right value as an ordinary observed era. The present is healed by the fix;
//! what remains wrong is the record of the past.
//!
//! So a correction MARKS A WINDOW. It never edits a value. An operator-supplied
//! value would be indistinguishable, in the store, from an operator quietly
//! changing what the catalog asserts — and there would be no way afterwards to
//! tell a repair from an opinion.
//!
//! # The case where marking the past is not enough
//!
//! If the value currently in force was itself established inside the bad
//! window, the fix has not healed anything: reads at *now* fall outside the
//! corrected interval and return the bad value unmarked. Writing the correction
//! anyway would produce a store that looks repaired and still serves the defect
//! as current.
//!
//! That case is refused, with the instruction that resolves it: poll first. A
//! poll re-normalizes the live document through the fixed parser and writes the
//! right value, after which the correction marks a window that no longer
//! contains the present.

use std::collections::BTreeMap;

use fusiform_core::{BoundaryKind, Correction, FieldId, SourceId, Timestamp};

use crate::{CatalogError, CatalogStore, FactKey, NewEra};

/// What a correction would do, resolved against the store before anything is
/// written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectionPlan {
    /// One entry per fact, each carrying the value that stays in force.
    pub rows: Vec<PlannedCorrection>,
    /// The extent every row will carry.
    pub fields: Vec<FieldId>,
    pub affected_from: Timestamp,
    pub affected_until: Timestamp,
    pub reason: String,
}

/// One fact a correction would mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCorrection {
    pub provider_id: String,
    pub model_id: String,
    pub fact_key: FactKey,
    /// The value that remains in force. Read from the store, never supplied.
    pub value_json: String,
    /// When the era carrying that value was established.
    ///
    /// Shown so an operator can see the fix landed before they commit: a
    /// boundary inside the corrected window is the refused case below.
    pub current_since: Timestamp,
}

/// Why a correction cannot be written.
///
/// Each variant carries what an operator needs to resolve it without reading
/// this file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorrectionRefusal {
    /// A named field has no single fact key.
    NotAddressable { field: FieldId },
    /// The fact has no history at all, so there is nothing to correct.
    ///
    /// Almost always a typo in the model or provider id, and writing the
    /// correction anyway would create a fact whose only era is a correction to
    /// a value that was never recorded.
    NoSuchFact {
        provider_id: String,
        model_id: String,
        fact_key: FactKey,
    },
    /// The value in force was already in force during the window.
    ///
    /// Either it was established inside the window — so the fix has not reached
    /// the current value, and marking the past leaves the present serving the
    /// defect unmarked — or it began before the window and spans it, meaning
    /// nothing changed and there is no repaired value for the correction to sit
    /// behind.
    PresentStillAffected {
        fact_key: FactKey,
        current_since: Timestamp,
    },
    /// An identical correction is already recorded.
    ///
    /// Refused rather than deduplicated, because a second identical correction
    /// is either a double-submit or a misunderstanding of what the first one
    /// did, and both are worth stopping.
    AlreadyRecorded { fact_key: FactKey },
}

impl std::fmt::Display for CorrectionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAddressable { field } => write!(
                f,
                "{field:?} names no single fact; tier and charge-unit corrections \
                 carry a threshold this command cannot address"
            ),
            Self::NoSuchFact {
                provider_id,
                model_id,
                fact_key,
            } => write!(
                f,
                "{provider_id}/{model_id} has no history for {}; check the \
                 provider and model ids",
                fact_key.as_str()
            ),
            Self::PresentStillAffected {
                fact_key,
                current_since,
            } => write!(
                f,
                "{}: the value in force was recorded at {}, which is before the \
                 end of the window being corrected — so the same value was in \
                 force during the window and is still being served. Either the \
                 fix has not reached the current value, or nothing changed and \
                 there is nothing to correct. Poll first, then correct.",
                fact_key.as_str(),
                current_since.0
            ),
            Self::AlreadyRecorded { fact_key } => write!(
                f,
                "{}: an identical correction is already recorded",
                fact_key.as_str()
            ),
        }
    }
}

/// Resolve a correction against the store without writing anything.
///
/// Every refusal is collected rather than returned on the first failure: an
/// operator correcting six facts wants all six problems at once, not six
/// round trips.
#[allow(clippy::too_many_arguments)]
pub fn plan_correction(
    store: &CatalogStore,
    source: SourceId,
    provider_id: &str,
    model_id: &str,
    fields: &[FieldId],
    affected_from: Timestamp,
    affected_until: Timestamp,
    reason: &str,
    now: Timestamp,
) -> Result<Result<CorrectionPlan, Vec<CorrectionRefusal>>, CatalogError> {
    let mut refusals = Vec::new();
    let mut rows = Vec::new();

    // Resolve every field to a fact key first, so a typo in one field does not
    // hide problems with the others.
    let mut keys: BTreeMap<FactKey, FieldId> = BTreeMap::new();
    for field in fields {
        match FactKey::for_field(field.clone()) {
            Some(key) => {
                keys.insert(key, field.clone());
            }
            None => refusals.push(CorrectionRefusal::NotAddressable {
                field: field.clone(),
            }),
        }
    }

    for fact_key in keys.keys() {
        // The value in force now, read rather than supplied.
        let current = store.recorded_value_at(source, provider_id, model_id, fact_key, now)?;
        let Some(current) = current else {
            refusals.push(CorrectionRefusal::NoSuchFact {
                provider_id: provider_id.to_string(),
                model_id: model_id.to_string(),
                fact_key: fact_key.clone(),
            });
            continue;
        };

        // The era in force must have begun at or after the end of the window.
        //
        // `affected_until` is when the fix landed and fusiform stopped recording
        // the bad value, so the era carrying the good value begins there or
        // later. Anything earlier means the era in force was ALSO in force
        // during the window — the same value, still being served — and marking
        // that window wrong while serving the identical value now is incoherent.
        //
        // This covers two cases that look different and are one:
        //
        // - The era began INSIDE the window. The fix has not reached the current
        //   value; a read at now falls outside the corrected interval and still
        //   returns the defect, unmarked.
        // - The era began BEFORE the window and spans it. Nothing changed at
        //   all, so either the value is still wrong (and the correction is
        //   premature) or it was never wrong.
        //
        // The first version of this check tested only the first case, and the
        // second slipped through: a fact whose value has not moved since the
        // seed would have accepted a correction and then refused reads inside a
        // window whose value it was still serving. Found by driving a real
        // correction against a live daemon, not by reading.
        //
        // At-or-after rather than strictly after, because the good era begins
        // exactly at `affected_until` in the ordinary case. Reads use a closed
        // interval, so a read at exactly that instant refuses even though the
        // good value took effect then — over-inclusive by one instant, in the
        // direction the design asks for: an over-inclusive partition costs
        // review time, an under-inclusive one converts an unknown into a false
        // clean bill.
        if current.boundary_at < affected_until {
            refusals.push(CorrectionRefusal::PresentStillAffected {
                fact_key: fact_key.clone(),
                current_since: current.boundary_at,
            });
            continue;
        }

        // An identical correction already recorded.
        let existing = store.corrections_covering(
            source,
            provider_id,
            model_id,
            fact_key,
            // Any instant inside the window finds a correction covering it.
            affected_from,
        )?;
        if existing.iter().any(|c| {
            c.affected_from == affected_from
                && c.affected_until == affected_until
                && c.reason == reason
        }) {
            refusals.push(CorrectionRefusal::AlreadyRecorded {
                fact_key: fact_key.clone(),
            });
            continue;
        }

        rows.push(PlannedCorrection {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
            fact_key: fact_key.clone(),
            value_json: current.value_json,
            current_since: current.boundary_at,
        });
    }

    if !refusals.is_empty() {
        return Ok(Err(refusals));
    }

    Ok(Ok(CorrectionPlan {
        rows,
        fields: keys.into_values().collect(),
        affected_from,
        affected_until,
        reason: reason.to_string(),
    }))
}

/// Write a planned correction.
///
/// Takes a plan rather than the raw arguments, so the rows that are written are
/// the rows that were shown. Re-resolving here would let the store change
/// between preview and commit and write something the operator never saw.
pub fn apply_correction(
    store: &CatalogStore,
    source: SourceId,
    plan: &CorrectionPlan,
    now: Timestamp,
) -> Result<usize, CatalogError> {
    let correction = Correction {
        fields: plan.fields.clone(),
        affected_from: plan.affected_from,
        affected_until: plan.affected_until,
        reason: plan.reason.clone(),
    };

    let eras: Vec<NewEra> = plan
        .rows
        .iter()
        .map(|row| NewEra {
            source,
            provider_id: row.provider_id.clone(),
            model_id: row.model_id.clone(),
            fact_key: row.fact_key.clone(),
            // The value already in force. A correction marks a window; it does
            // not change what the catalog currently asserts.
            value_json: row.value_json.clone(),
            boundary_at: now,
            boundary_kind: BoundaryKind::Corrected(correction.clone()),
            observation_id: None,
        })
        .collect();

    // One transaction. A half-applied correction would mark some facts of a
    // single defect and leave the rest reading clean, which is worse than
    // marking none: a consumer partitioning on the extent would believe the
    // unmarked facts were checked and found good.
    store.append_eras(&eras)
}
