//! Run the parser against a whole live document.
//!
//! The excerpt fixture proves the shapes someone thought to select; this runs
//! the shipped parser over every row of a complete fetch. Both matter, and they
//! do different jobs.
//!
//! The one real defect in this module — reading the legacy `context_over_200k`
//! key's NAME as its threshold — was caught by the excerpt, because a model
//! with a non-200k threshold had been deliberately selected into it. What the
//! excerpt could not supply was the scale: only the full document shows that
//! the rule would have rejected 162 of 288 real models, across seven distinct
//! thresholds including two BELOW the one the key is named for. A fixture tells
//! you a rule is wrong; the whole document tells you how wrong.
//!
//! Skipped when the payload is not present, so the suite stays runnable on a
//! machine with no capture. The env var names a file captured by
//! `fusiform-cli fetch --save`.

use std::collections::BTreeMap;

use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{ChargeBasis, RateValue};

fn payload() -> Option<Vec<u8>> {
    let path = std::env::var("FUSIFORM_FULL_PAYLOAD").ok()?;
    std::fs::read(path).ok()
}

#[test]
fn a_whole_live_document_normalizes_without_a_single_refusal() {
    let Some(bytes) = payload() else {
        eprintln!("skipped: set FUSIFORM_FULL_PAYLOAD to a captured models.dev document");
        return;
    };

    let outcome = match normalize_models_dev(&bytes) {
        Ok(outcome) => outcome,
        Err(e) => panic!("the live document must normalize, got: {e}"),
    };

    // Sanity floor: a document that parsed but produced almost nothing would
    // pass every assertion below while being useless.
    assert!(
        outcome.catalog.model_count() > 1_000,
        "expected thousands of models, got {}",
        outcome.catalog.model_count()
    );

    // Every non-fatal finding must be an unattributable charge unit. Anything
    // else appearing here is a shape this parser does not understand.
    for finding in &outcome.findings {
        assert!(
            matches!(
                finding,
                fusiform_core::NormalizeError::UnattributableChargeUnit { .. }
            ),
            "unexpected finding on live data: {finding}"
        );
    }

    // Report the shape of what was found, so a future change that quietly
    // starts dropping rows is visible in the test output rather than silent.
    let mut unattributable: BTreeMap<String, usize> = BTreeMap::new();
    for finding in &outcome.findings {
        if let fusiform_core::NormalizeError::UnattributableChargeUnit { key, .. } = finding {
            *unattributable.entry(key.clone()).or_default() += 1;
        }
    }
    eprintln!(
        "normalized {} models across {} providers",
        outcome.catalog.model_count(),
        outcome.catalog.providers.len()
    );
    eprintln!("unattributable charge keys: {unattributable:?}");
}

/// No rate anywhere in a live document may be a silently-rounded zero.
///
/// The money boundary refuses a nonzero rate that would round to zero, so this
/// asserts that refusal never fires on real data — if it did, the parse would
/// have failed above. What this checks is the other direction: that every
/// `StatedZero` corresponds to a literal zero rather than a rounding artifact.
#[test]
fn every_zero_rate_on_live_data_was_stated_as_zero() {
    let Some(bytes) = payload() else {
        eprintln!("skipped: set FUSIFORM_FULL_PAYLOAD to a captured models.dev document");
        return;
    };
    let outcome = normalize_models_dev(&bytes).expect("live document must normalize");

    let mut priced = 0usize;
    let mut stated_zero = 0usize;
    let mut unpriced = 0usize;

    for model in outcome.catalog.models() {
        for rate in &model.rates {
            match &rate.value {
                RateValue::Priced { amount, .. } => {
                    priced += 1;
                    // A priced rate is never zero: that is what the money
                    // boundary's zero guard exists to prevent.
                    assert_ne!(
                        amount.units, 0,
                        "{}: a priced rate must not be zero",
                        model.key
                    );
                    assert!(
                        amount.units > 0,
                        "{}: a rate must never be negative",
                        model.key
                    );
                }
                RateValue::StatedZero { .. } => stated_zero += 1,
                RateValue::Unpriced { .. } => unpriced += 1,
                RateValue::BilledAs { .. } => {
                    panic!(
                        "a read without auth_method must never carry billed_as: {}",
                        model.key
                    )
                }
            }
        }
    }

    eprintln!("rates: {priced} priced, {stated_zero} stated-zero, {unpriced} unpriced");
    assert!(priced > 1_000, "expected thousands of priced rates");
    assert!(
        stated_zero > 0,
        "the live document is known to contain literal zero rates"
    );
}

/// Every tiered rate on live data carries its threshold on its own key.
#[test]
fn tiered_rates_are_self_describing_on_live_data() {
    let Some(bytes) = payload() else {
        eprintln!("skipped: set FUSIFORM_FULL_PAYLOAD to a captured models.dev document");
        return;
    };
    let outcome = normalize_models_dev(&bytes).expect("live document must normalize");

    let mut thresholds: BTreeMap<u64, usize> = BTreeMap::new();
    for model in outcome.catalog.models() {
        for rate in &model.rates {
            if let fusiform_core::RateCondition::MinContextTokens { tokens } = rate.condition {
                *thresholds.entry(tokens).or_default() += 1;
                assert!(
                    matches!(rate.basis, ChargeBasis::PerMillionTokens { .. }),
                    "{}: a context threshold only makes sense on a token rate",
                    model.key
                );
            }
        }
    }

    eprintln!("context thresholds found: {thresholds:?}");
    // The measured document has several distinct thresholds. Finding only one
    // would mean the parser had collapsed them — the defect this suite caught.
    assert!(
        thresholds.len() > 1,
        "expected multiple distinct context thresholds, got {thresholds:?}"
    );
}
