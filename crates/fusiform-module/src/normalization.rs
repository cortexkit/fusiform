use std::collections::BTreeMap;

use fusiform_core::NormalizeV2Outcome;

#[cfg(all(test, target_os = "linux"))]
mod v1_fixture;

pub(crate) fn log_findings(origin: &str, outcome: &NormalizeV2Outcome) -> BTreeMap<String, usize> {
    for finding in &outcome.findings {
        eprintln!(
            "fusiform: {origin} normalization: {:?} ({} rows; first model {:?})",
            finding.key, finding.occurrences, finding.first_model_id
        );
    }
    outcome.finding_counts()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finding_counts_every_row_in_the_body() {
        let body = br#"{"p":{"models":{"a":{"type":"chat","api":{},"capabilities":{},"new_key":1},"b":{"type":"chat","api":{},"capabilities":{},"new_key":2}}}}"#;
        let outcome = fusiform_core::normalize_models_dev(body).unwrap();
        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(
            log_findings("test", &outcome),
            BTreeMap::from([("unknown_key".to_owned(), 2)])
        );
    }
}
