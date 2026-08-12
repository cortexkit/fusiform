//! Normalization run against real upstream bytes.
//!
//! The fixture is an excerpt cut from a live models.dev fetch on 2026-08-11
//! (source sha256 9436dd07…), not hand-written. A fixture written by the same
//! author as the parser in the same sitting encodes one belief twice, and a
//! non-vacuous assertion over it certifies whatever that belief got wrong —
//! which is exactly how the shared catalog crate's tier defect shipped with a
//! passing test. Every model kept here was selected because it exercises a
//! shape the real document actually contains.

use fusiform_core::normalize::{normalize_models_dev, NormalizedModel};
use fusiform_core::{
    ChargeBasis, Modality, NormalizeError, RateCondition, RateValue, TokenClass, UnpricedReason,
};

const FIXTURE: &[u8] = include_bytes!("../fixtures/models-dev-excerpt.json");

fn catalog() -> fusiform_core::NormalizeOutcome {
    normalize_models_dev(FIXTURE).expect("the measured document must normalize")
}

fn model<'a>(outcome: &'a fusiform_core::NormalizeOutcome, key: &str) -> &'a NormalizedModel {
    outcome
        .catalog
        .models()
        .find(|m| format!("{}/{}", m.key.provider_id, m.key.model_id) == key)
        .unwrap_or_else(|| panic!("fixture must contain {key}"))
}

fn rate<'a>(m: &'a NormalizedModel, class: TokenClass, condition: &RateCondition) -> &'a RateValue {
    &m.rates
        .iter()
        .find(|r| r.basis == ChargeBasis::PerMillionTokens { class } && &r.condition == condition)
        .unwrap_or_else(|| panic!("{} must have a {:?} rate at {:?}", m.key, class, condition))
        .value
}

#[test]
fn the_measured_document_normalizes() {
    let outcome = catalog();
    assert_eq!(outcome.catalog.model_count(), 14);
    // A provider with no models is still a provider: dropping it would lose the
    // fact that the upstream describes it.
    assert!(outcome
        .catalog
        .providers
        .iter()
        .any(|p| p.provider_id == "cerebras" && p.models.is_empty()));
}

/// Ordinary rates scale exactly, with no float anywhere in the path.
#[test]
fn rates_scale_to_exact_minor_units() {
    let outcome = catalog();
    let sonnet = model(&outcome, "anthropic/claude-sonnet-4-5");

    let RateValue::Priced { amount } = rate(sonnet, TokenClass::Input, &RateCondition::Always)
    else {
        panic!("input rate must be priced");
    };
    // $3.00 per million tokens, at nanodollar resolution.
    assert_eq!(amount.units, 3_000_000_000);
    assert_eq!(amount.exponent, 9);
    assert_eq!(amount.currency.as_str(), "USD");

    // The upstream states no currency anywhere, so every amount must carry the
    // named policy that supplied it rather than an unattributed "USD".
    assert!(matches!(
        amount.unit_provenance,
        fusiform_core::UnitProvenance::AssumedByPolicy { .. }
    ));

    // A sub-dollar rate, where a float path would visibly drift.
    let RateValue::Priced { amount } = rate(sonnet, TokenClass::CacheRead, &RateCondition::Always)
    else {
        panic!("cache_read rate must be priced");
    };
    assert_eq!(amount.units, 300_000_000);
}

/// A zero limit is absence, and it applies to text models too.
#[test]
fn zero_limits_become_absent_not_zero() {
    let outcome = catalog();

    // An image model declaring context 0, input 0, output 0.
    let image = model(&outcome, "openai/gpt-image-1");
    assert_eq!(image.limits.context_tokens, None);
    assert_eq!(image.limits.output_tokens, None);

    // The case that makes the rule unconditional: a model with a REAL output
    // limit and a zero context limit. Absence and capacity coexist on one row,
    // so a modality-gated rule would get this wrong.
    let green = model(&outcome, "greenpt/green-s");
    assert_eq!(green.limits.context_tokens, None);
    assert_eq!(green.limits.output_tokens, Some(8192));

    // And a real limit is preserved, so the rule is not simply erasing limits.
    let sonnet = model(&outcome, "anthropic/claude-sonnet-4-5");
    assert_eq!(sonnet.limits.context_tokens, Some(1_000_000));
    assert_eq!(sonnet.limits.output_tokens, Some(64_000));
}

/// A stated zero rate is not a missing rate.
#[test]
fn a_stated_zero_rate_is_distinct_from_no_rate() {
    let outcome = catalog();

    // Every rate on this model is exactly 0 in the upstream.
    let flash = model(&outcome, "zhipuai/glm-4.5-flash");
    assert_eq!(
        rate(flash, TokenClass::Input, &RateCondition::Always),
        &RateValue::StatedZero
    );
    assert_eq!(
        rate(flash, TokenClass::CacheWrite, &RateCondition::Always),
        &RateValue::StatedZero
    );

    // A model with no cost object at all produces no rate rows — the absence is
    // expressed by there being nothing, not by a fabricated zero.
    let uncosted = model(&outcome, "anyapi/cohere/command-r-plus-08-2024");
    assert!(
        uncosted.rates.is_empty(),
        "a model with no cost object must not gain invented rates"
    );

    // The two are therefore distinguishable, which is the point: a cap that
    // treats them alike either blocks a free model or lets an unpriced one run.
    assert!(!flash.rates.is_empty());
}

/// Context tiers are read from `tier.type`, never from the size field's name.
#[test]
fn context_tiers_produce_conditional_rates() {
    let outcome = catalog();
    let gemini = model(&outcome, "impossibl/google/gemini-3.1-pro-preview");

    // Base rate, unconditional.
    let RateValue::Priced { amount } = rate(gemini, TokenClass::Input, &RateCondition::Always)
    else {
        panic!("base input rate must be priced");
    };
    assert_eq!(amount.units, 2_000_000_000);

    // Above the declared threshold, a different rate — carried on the rate's
    // own key, so a pricing consumer never joins capability rows to price rows.
    let over = RateCondition::MinContextTokens { tokens: 200_000 };
    let RateValue::Priced { amount } = rate(gemini, TokenClass::Input, &over) else {
        panic!("over-threshold input rate must be priced");
    };
    assert_eq!(amount.units, 4_000_000_000);

    // A threshold that is not the legacy 200k, proving the parser reads the
    // declared size rather than assuming the one the legacy key is named for.
    let luna = model(&outcome, "openai/gpt-5.6-luna");
    let over_luna = RateCondition::MinContextTokens { tokens: 272_000 };
    let RateValue::Priced { amount } = rate(luna, TokenClass::Output, &over_luna) else {
        panic!("luna over-threshold output rate must be priced");
    };
    assert_eq!(amount.units, 1_800_000_000);
}

/// A tier whose type is not `context` stops the parse.
///
/// This is the defect that shipped in the shared catalog crate: `tier.size` was
/// read as a context threshold with no check on `tier.type`, so any future tier
/// dimension — a rate tier, a volume tier — would have been silently priced as
/// a context threshold. The fixture cannot contain this case because the
/// upstream has never published one, so the input is a deliberate mutation of
/// real bytes.
#[test]
fn an_unknown_tier_type_is_refused_not_guessed() {
    let mutated = String::from_utf8(FIXTURE.to_vec())
        .unwrap()
        .replace("\"type\": \"context\"", "\"type\": \"volume\"");
    assert!(
        mutated.contains("\"volume\""),
        "the mutation must actually apply, or this test proves nothing"
    );

    match normalize_models_dev(mutated.as_bytes()) {
        Err(NormalizeError::UnknownTierType { tier_type, .. }) => {
            assert_eq!(tier_type, "volume");
        }
        other => panic!("an unknown tier type must stop the parse, got {other:?}"),
    }
}

/// A tier carrying a size with no type stops the parse too.
///
/// The type key is renamed rather than deleted, so the mutation does not depend
/// on the fixture's whitespace. An earlier version of this test replaced a
/// string with a trailing comma that the fixture does not contain, so the
/// mutation silently did not apply and the test passed an unmutated document —
/// which is why the guard below is not optional.
#[test]
fn a_tier_without_a_type_is_refused() {
    let text = String::from_utf8(FIXTURE.to_vec()).unwrap();
    let mutated = text.replace("\"type\": \"context\"", "\"kind\": \"context\"");
    assert_ne!(mutated, text, "the mutation must actually apply");

    match normalize_models_dev(mutated.as_bytes()) {
        Err(NormalizeError::TierMissingType { .. }) => {}
        other => panic!("a typeless tier must stop the parse, got {other:?}"),
    }
}

/// The two tier encodings must agree ON RATES, and disagreement stops the parse.
///
/// The mutation is structural rather than textual. A `str::replace` against
/// pretty-printed JSON depends on the fixture's indentation, so reformatting the
/// file silently turns the mutation into a no-op and the test passes an
/// unmutated document — which happened once already in this file, and again
/// when the fixture was regenerated.
#[test]
fn a_legacy_tier_whose_rates_no_row_reproduces_is_refused() {
    let mut doc: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();

    // Move the legacy block's input rate away from the tier row's, so the two
    // encodings disagree about money and fusiform cannot tell which is right.
    let legacy = doc
        .get_mut("impossibl")
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.get_mut("google/gemini-3.1-pro-preview"))
        .and_then(|m| m.get_mut("cost"))
        .and_then(|c| c.get_mut("context_over_200k"))
        .and_then(|t| t.as_object_mut())
        .expect("the fixture must carry this legacy block");

    let before = legacy.get("input").cloned().expect("a legacy input rate");
    legacy.insert("input".to_string(), serde_json::json!(7));
    assert_ne!(
        before,
        serde_json::json!(7),
        "the mutation must actually change the value"
    );

    let mutated = serde_json::to_vec(&doc).unwrap();
    match normalize_models_dev(&mutated) {
        Err(NormalizeError::OrphanedLegacyTier { model, .. }) => {
            assert!(model.contains("gemini-3.1-pro-preview"), "got {model}");
        }
        other => panic!("a disagreeing legacy tier must stop the parse, got {other:?}"),
    }

    // And the unmutated fixture normalizes, so the failure above is the
    // mutation's doing rather than a fixture that was already broken.
    assert!(normalize_models_dev(FIXTURE).is_ok());
}

/// A tier threshold that is not the number the legacy key is NAMED for must
/// normalize fine.
///
/// Measured 2026-08-11: of the 288 models carrying `context_over_200k`, only
/// 126 have a 200,000 tier. The other 162 threshold at 272,000, 256,000,
/// 262,144 or 512,000 while still carrying the legacy key. An implementation
/// that checks the key's name against the tier size rejects all 162 — which is
/// what this parser did until this fixture ran, because reading a threshold out
/// of a field's name is exactly the defect it was written to avoid.
#[test]
fn a_legacy_key_with_a_non_200k_threshold_is_accepted() {
    let outcome = catalog();

    // This model carries `context_over_200k` and thresholds at 272,000.
    let luna = model(&outcome, "openai/gpt-5.6-luna");
    let over = RateCondition::MinContextTokens { tokens: 272_000 };
    assert!(
        matches!(
            rate(luna, TokenClass::Input, &over),
            RateValue::Priced { .. }
        ),
        "a legacy key with a non-200k threshold must still normalize"
    );

    // And the legacy block contributes no rate of its own: the tiered rates come
    // from the tier rows, so the model has exactly one over-threshold input rate.
    let over_threshold_inputs = luna
        .rates
        .iter()
        .filter(|r| {
            r.basis
                == ChargeBasis::PerMillionTokens {
                    class: TokenClass::Input,
                }
                && r.condition != RateCondition::Always
        })
        .count();
    assert_eq!(
        over_threshold_inputs, 1,
        "the legacy encoding must corroborate, never add a duplicate rate"
    );
}

/// A cost key with no establishable unit becomes explicitly unpriced.
#[test]
fn an_unattributable_charge_key_is_unpriced_and_reported() {
    let outcome = catalog();

    // `input_audio` sits in the same flat namespace as per-token rates with
    // nothing to say whether it bills per token, per second or per minute.
    let finding = outcome
        .findings
        .iter()
        .find(|f| matches!(f, NormalizeError::UnattributableChargeUnit { key, .. } if key == "input_audio"))
        .expect("input_audio must be reported as unattributable");
    let NormalizeError::UnattributableChargeUnit { model, .. } = finding else {
        unreachable!()
    };
    assert!(model.contains("gemini-flash-latest"));

    // And it is recorded as an unpriced row rather than dropped, so a consumer
    // knows the charge exists and refuses to price it.
    let gemini = model_by(&outcome, "google/gemini-flash-latest");
    assert!(gemini.rates.iter().any(|r| r.value
        == RateValue::Unpriced {
            reason: UnpricedReason::UnknownChargeBasis
        }));
}

fn model_by<'a>(outcome: &'a fusiform_core::NormalizeOutcome, key: &str) -> &'a NormalizedModel {
    model(outcome, key)
}

/// Renderer-selecting fields are recorded as quarantined and never as data.
#[test]
fn renderer_selecting_fields_are_quarantined() {
    let outcome = catalog();

    // A per-model provider override carrying literal headers and body
    // parameters. Both models come from the same provider, so the flag is
    // proven to track the model's own field rather than its provider's.
    let qwen = model(&outcome, "opencode-go/qwen3.7-plus");
    assert!(
        qwen.quarantined.provider_override,
        "this model carries a provider override and must be flagged"
    );
    let deepseek = model(&outcome, "opencode-go/deepseek-v4-flash");
    assert!(
        !deepseek.quarantined.provider_override,
        "this model carries no override; flagging it would make the flag meaningless"
    );

    // An experimental block with named modes.
    let opus = model(&outcome, "anyapi/anthropic/claude-opus-4-6");
    assert_eq!(
        opus.quarantined.experimental_modes,
        vec!["fast".to_string()]
    );

    // The provider's npm adapter is flagged, never served as a field.
    let anthropic = outcome
        .catalog
        .providers
        .iter()
        .find(|p| p.provider_id == "anthropic")
        .unwrap();
    assert!(anthropic.npm_quarantined);
}

/// An experimental mode's rates are never mixed into the base rates.
///
/// Measured: 38 models carry `experimental`, all 38 have per-mode `cost`, and
/// 33 also carry a base rate — with mode rates up to 6.67x the base. A consumer
/// pricing a mode request against the base rate under-charges by that factor,
/// so the two must not be reachable through the same key.
#[test]
fn experimental_mode_rates_never_reach_the_base_rates() {
    let outcome = catalog();
    let opus = model(&outcome, "anyapi/anthropic/claude-opus-4-6");

    // The mode block prices input at 30.0; if it leaked into the base rates,
    // this model would have an unconditional input rate it does not publish.
    let has_base_input = opus.rates.iter().any(|r| {
        r.basis
            == ChargeBasis::PerMillionTokens {
                class: TokenClass::Input,
            }
            && r.condition == RateCondition::Always
    });
    assert!(
        !has_base_input,
        "this model publishes no base input rate; a mode rate must not supply one"
    );

    // The mode is still visible as quarantined, so its existence is not lost.
    assert!(!opus.quarantined.experimental_modes.is_empty());
}

/// Unrecognised modalities survive, and known ones parse.
#[test]
fn modalities_are_preserved_exactly() {
    let outcome = catalog();

    let veo = model(&outcome, "poe/google/veo-3");
    assert_eq!(
        veo.capabilities.output_modalities,
        Some(vec![Modality::Video])
    );

    let sdxl = model(&outcome, "poe/stabilityai/stablediffusionxl");
    assert_eq!(
        sdxl.capabilities.output_modalities,
        Some(vec![Modality::Image])
    );
    assert_eq!(
        sdxl.capabilities.input_modalities,
        Some(vec![Modality::Text, Modality::Image])
    );

    let gemini = model(&outcome, "google/gemini-flash-latest");
    assert!(gemini
        .capabilities
        .input_modalities
        .as_ref()
        .expect("this model publishes a modality block")
        .contains(&Modality::Audio));
}

/// A model with no modality block gets `None`, not an empty list.
///
/// An empty list is a positive claim that the model accepts nothing. A missing
/// block is the upstream saying nothing at all, and the two must not collapse:
/// a consumer selecting models by modality would filter out every affected
/// model with nothing looking wrong.
///
/// Latent rather than live — measured 2026-08-11, zero of 6,254 models omit the
/// block. Fixed at the parse boundary anyway, because the day one does is the
/// day the wrong reading ships as data.
#[test]
fn a_missing_modality_block_is_unknown_rather_than_empty() {
    let mut doc: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    let model_obj = doc
        .get_mut("anthropic")
        .and_then(|p| p.get_mut("models"))
        .and_then(|m| m.get_mut("claude-sonnet-4-5"))
        .and_then(|m| m.as_object_mut())
        .expect("the fixture carries this model");
    let removed = model_obj.remove("modalities");
    assert!(
        removed.is_some(),
        "the fixture must have had a modality block, or this proves nothing"
    );

    let outcome = normalize_models_dev(&serde_json::to_vec(&doc).unwrap()).unwrap();
    let sonnet = model(&outcome, "anthropic/claude-sonnet-4-5");
    assert_eq!(
        sonnet.capabilities.input_modalities, None,
        "a missing block is unknown, never an empty list"
    );
    assert_eq!(sonnet.capabilities.output_modalities, None);

    // And a model that DOES publish one is unaffected, so this is not just
    // everything becoming None.
    let other = model(&outcome, "openai/gpt-5.6-luna");
    assert!(other.capabilities.input_modalities.is_some());
}

/// An absent capability key and an explicit `null` both normalize to unknown.
///
/// Both spellings mean the upstream said nothing, so collapsing them is
/// correct — unlike the modality lists, where an absent block and an empty list
/// are different claims (`[]` states the model accepts no modality, which is a
/// positive assertion manufactured from silence).
///
/// Pinned because I told BROCA that fusiform serves `null` under exactly one
/// condition — the upstream omitting the key — and that was narrower than the
/// truth. There are two conditions and they agree. Their tripwire asserts
/// `is_boolean`, so it fires on both, and this test is the other half of that
/// pair: it fails if fusiform ever starts distinguishing them, which would make
/// their coverage claim and mine disagree.
#[test]
fn an_absent_capability_and_an_explicit_null_are_both_unknown() {
    let doc = |body: &str| {
        format!(r#"{{"p":{{"id":"p","name":"P","models":{{"m":{{"id":"m","name":"M"{body}}}}}}}}}"#)
    };

    let read = |body: &str| {
        let text = doc(body);
        normalize_models_dev(text.as_bytes())
            .unwrap_or_else(|e| panic!("{body:?} must normalize: {e}"))
            .catalog
            .models()
            .next()
            .expect("one model")
            .capabilities
            .reasoning
    };

    assert_eq!(read(""), None, "an absent key is unknown");
    assert_eq!(
        read(r#","reasoning":null"#),
        None,
        "an explicit null is unknown too — a future source may spell it this way"
    );
    assert_eq!(read(r#","reasoning":true"#), Some(true));
    assert_eq!(
        read(r#","reasoning":false"#),
        Some(false),
        "an explicit false is a real claim and must not become unknown"
    );
}
