use std::collections::BTreeMap;

use fusiform_curate as curate;
use serde_json::Value;

use crate::{CurationSet, Upstream, VendorDocument};

type Documents = BTreeMap<String, Value>;

/// Check vendor documents before an operator writes a draft. A login allowlist
/// must name a model upstream publishes now, so the writer cannot publish a typo.
pub fn validate_published(documents: &Documents, upstream: &Upstream) -> Result<(), String> {
    validate_vendor_documents(documents, Some(upstream), false)
}

/// Check vendor documents from a stored release without enforcing newer
/// authoring rules for retired fields or requiring current upstream models.
pub fn validate_received(documents: &Documents) -> Result<(), String> {
    validate_vendor_documents(documents, None, true)
}

pub(crate) fn validate_new_release_documents(documents: &Documents) -> Result<(), String> {
    // A login allowlist only restricts a model while it is served. An upstream
    // withdrawal after approval must not prevent a release from being built.
    validate_vendor_documents(documents, None, false)
}

fn validate_vendor_documents(
    documents: &Documents,
    upstream: Option<&Upstream>,
    stored: bool,
) -> Result<(), String> {
    use VendorDocument::*;

    let curated = documents
        .get("curated-models")
        .map(curated)
        .transpose()?
        .unwrap_or_default();
    for (name, raw) in documents {
        let doc = raw.to_string();
        let rejects = match document_kind(name)? {
            ModelAliases if stored => curate::aliases::load_stored_reporting_rejects(&doc).1,
            ModelAliases => curate::aliases::load_reporting_rejects(&doc).1,
            AttachmentLimits => curate::attachment_limits::load(&doc).1,
            BillingPlanes => curate::billing_planes::load_reporting_rejects(&doc).1,
            CacheLifetimes => curate::cache_lifetimes::load_reporting_rejects(&doc).1,
            CanonicalLinks => curate::canonical_links::load_reporting_rejects(&doc).1,
            ModelCreators => curate::creators::parse_document(&doc).1,
            CuratedModels => continue,
            CuratedProviders => {
                // The provider vector uses this document; no serve route does.
                // Retain it intact in the signed curation history.
                if raw["schema"] != "curated-providers/v1" || !raw["providers"].is_array() {
                    return Err(format!(
                        "{name}: expected curated-providers/v1 and a providers array"
                    ));
                }
                continue;
            }
            FactOverrides => curate::fact_overrides::load_reporting_rejects(&doc).1,
            InputAudioPerToken => curate::input_audio::load_reporting_rejects(&doc).1,
            OutputAudioPerToken => {
                curate::input_audio::load_for_schema(&doc, "output-audio-per-token/v1").1
            }
            LoginMethods => {
                curate::login_methods::load_where(&doc, &curated.models, |provider, model| {
                    upstream.is_none_or(|upstream| {
                        upstream
                            .get(provider)
                            .is_some_and(|row| row.models.contains_key(model))
                    })
                })
                .1
            }
            ModelLineage => curate::model_lineage::load_reporting_rejects(&doc).1,
            SpeedTwins => curate::speed_twins::load_reporting_rejects(&doc).1,
            TemperatureRanges => curate::temperature_ranges::load_reporting_rejects(&doc).1,
            WireCorrections if stored => {
                curate::wire_corrections::load_stored_reporting_rejects(&doc).1
            }
            WireCorrections => curate::wire_corrections::load_reporting_rejects(&doc).1,
            WireFamilies => curate::wire_families::load_reporting_rejects(&doc).1,
            PlanPrices => curate::plan_prices::load_reporting_rejects(&doc).1,
        };
        accepted(name, ((), rejects))?;
    }
    Ok(())
}

fn document_kind(name: &str) -> Result<VendorDocument, String> {
    VendorDocument::from_name(name)
        .ok_or_else(|| format!("unrecognized published vendor document {name:?}"))
}

fn accepted<T>(document: &str, (rows, rejects): (T, Vec<String>)) -> Result<T, String> {
    if rejects.is_empty() {
        Ok(rows)
    } else {
        Err(format!("{document}: {}", rejects.join("; ")))
    }
}

fn curated(value: &Value) -> Result<curate::curated_models::Table, String> {
    accepted(
        "curated-models",
        curate::curated_models::load_table_reporting_rejects(&value.to_string()),
    )
}

/// Check the keys, grade values and workload names in a curation set.
pub fn validate_judgments(set: &CurationSet) -> Result<(), String> {
    let model_keys = set
        .grades
        .keys()
        .chain(set.effort_grades.keys())
        .chain(set.workload_efforts.keys())
        .chain(set.decommissioned.keys())
        .map(|model| (model, false))
        .chain(set.kind_grades.keys().map(|model| (model, true)));
    for (model, task) in model_keys {
        if model.is_empty()
            || !task && model.starts_with("local:")
            || model.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(format!("invalid published model key {model:?}"));
        }
    }
    let valid_number =
        |number: &serde_json::Number| number.as_f64().is_some_and(|n| n > 0.0 && n.is_finite());
    for (model, grade) in &set.grades {
        if grade
            .graded_effort
            .as_deref()
            .is_some_and(|effort| effort.trim().is_empty())
            || !valid_number(&grade.elo)
            || !valid_number(&grade.eq)
        {
            return Err(format!("invalid chat grade for {model}"));
        }
    }
    for (model, efforts) in &set.effort_grades {
        if efforts.iter().any(|(effort, grade)| {
            effort.trim().is_empty() || !valid_number(&grade.elo) || !valid_number(&grade.eq)
        }) {
            return Err(format!("invalid effort grade for {model}"));
        }
    }
    for (model, kinds) in &set.kind_grades {
        if kinds.iter().any(|(kind, grade)| {
            !matches!(
                kind.as_str(),
                "transcription" | "speech" | "image" | "video" | "search" | "decision"
            ) || !valid_number(grade)
        }) {
            return Err(format!("invalid task grade for {model}"));
        }
    }
    for (model, workloads) in &set.workload_efforts {
        if workloads.iter().any(|(name, effort)| {
            !fusiform_protocol::WORKLOADS.contains(&name.as_str())
                || effort
                    .default
                    .as_deref()
                    .is_some_and(|effort| effort.trim().is_empty())
                || effort
                    .minimum
                    .as_deref()
                    .is_some_and(|effort| effort.trim().is_empty())
        }) {
            return Err(format!("invalid workload effort for {model}"));
        }
    }
    Ok(())
}
