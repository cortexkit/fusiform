use std::collections::{BTreeMap, BTreeSet};

use fusiform_core::NormalizeError;

pub(crate) fn log_findings(origin: &str, findings: &[NormalizeError]) -> BTreeMap<String, usize> {
    count_findings(findings, |message| {
        // Model IDs and member names are upstream text and may contain newlines.
        eprintln!(
            "fusiform: {origin} normalization: {}",
            message.escape_debug()
        );
    })
}

fn count_findings(
    findings: &[NormalizeError],
    mut log: impl FnMut(&str),
) -> BTreeMap<String, usize> {
    let mut seen = BTreeSet::new();
    let mut counts = BTreeMap::new();
    for finding in findings {
        let kind = match finding {
            NormalizeError::NotAProviderMap => "not_a_provider_map",
            NormalizeError::DocumentParse { .. } => "document_parse",
            NormalizeError::BadRate { .. } => "bad_rate",
            NormalizeError::UnknownTierType { .. } => "unknown_tier_type",
            NormalizeError::TierMissingType { .. } => "tier_missing_type",
            NormalizeError::OrphanedLegacyTier { .. } => "orphaned_legacy_tier",
            NormalizeError::UnattributableChargeUnit { .. } => "unattributable_charge_unit",
            NormalizeError::RefusedBaseSchedule { .. } => "refused_base_schedule",
            NormalizeError::RefusedModelMember { .. } => "refused_model_member",
            NormalizeError::RefusedMode { .. } => "refused_mode",
        };
        let message = finding.to_string();
        if seen.insert((kind, message.clone())) {
            log(&message);
            *counts.entry(kind.to_owned()).or_default() += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finding_is_logged_once_per_body() {
        let bad = NormalizeError::RefusedBaseSchedule {
            model: "p/bad".into(),
            member: "cost.input".into(),
        };
        let findings = [bad.clone(), bad];
        for _ in 0..2 {
            let mut log = Vec::new();
            let counts = count_findings(&findings, |line| log.push(line.to_owned()));
            assert_eq!(
                counts,
                BTreeMap::from([("refused_base_schedule".to_owned(), 1)])
            );
            assert_eq!(
                log,
                ["p/bad: cannot read cost.input; base and tier rates refused"]
            );
        }
    }
}
