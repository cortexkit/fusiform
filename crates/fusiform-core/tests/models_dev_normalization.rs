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
use fusiform_testkit::mutate;

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
    let text = String::from_utf8(FIXTURE.to_vec()).unwrap();
    let mutated = mutate(&text, "\"type\": \"context\"", "\"type\": \"volume\"");

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
    let mutated = mutate(&text, "\"type\": \"context\"", "\"kind\": \"context\"");

    match normalize_models_dev(mutated.as_bytes()) {
        Err(NormalizeError::TierMissingType { .. }) => {}
        other => panic!("a typeless tier must stop the parse, got {other:?}"),
    }
}

/// A tier spec that is MALFORMED rather than absent is refused too, and the
/// message must not name a cause it cannot know.
///
/// `RawTier`'s conversion drops the parse error (`.ok()`), so a malformed spec
/// and an absent one both arrive as `None`. Both fields of `RawTierSpec` are
/// optional, so the reachable failure is a wrong TYPE — `"size": "200000"` as a
/// string is the ordinary way an upstream drifts, and it is the case the old
/// message described as "carries a size with no type" while the type was
/// present and the size was what broke.
///
/// Fail-closed is the right direction and was already correct. What is asserted
/// here is that the refusal does not send a reader after the wrong field.
#[test]
fn a_malformed_tier_spec_is_refused_without_naming_a_cause_it_cannot_know() {
    let text = String::from_utf8(FIXTURE.to_vec()).unwrap();
    // A string where a number belongs: the spec stops parsing, and the `type`
    // key beside it is untouched and still correct.
    let mutated = mutate(&text, "\"size\": 200000", "\"size\": \"200000\"");

    match normalize_models_dev(mutated.as_bytes()) {
        Err(e @ NormalizeError::TierMissingType { .. }) => {
            let text = e.to_string();
            assert!(
                !text.contains("carries a size"),
                "the message must not assert the tier carries a size: the spec \
                 did not parse, so nothing about its contents is known: {text}"
            );
            assert!(
                text.contains("no readable type"),
                "the message must say what is known \u{2014} that no type could be \
                 read \u{2014} rather than why: {text}"
            );
        }
        other => panic!("a malformed tier spec must stop the parse, got {other:?}"),
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

    // The provider's API base URL is flagged, never carried as a value.
    //
    // A base URL decides which company receives the request, with credentials
    // attached, so a wrong one is not a degraded answer. It was previously
    // normalized into `api_base: Option<String>` and read by nothing — UNREAD
    // rather than quarantined, which is one refactor away from a wire it must
    // never reach.
    //
    // Asserted as a flag rather than by checking the value is absent, because
    // "the value is not there" would also pass if the field were dropped
    // entirely, and the flag is what tells a consumer the upstream HAS one.
    // Both directions, because a flag that is always true says nothing. The
    // fixture happens to carry both kinds, which is what caught the first
    // version of this assertion: I reached for `openai` and it publishes no
    // `api` at all.
    let provider = |id: &str| {
        outcome
            .catalog
            .providers
            .iter()
            .find(|p| p.provider_id == id)
            .unwrap_or_else(|| panic!("{id} is in the fixture"))
    };
    assert!(
        provider("anyapi").api_base_quarantined,
        "a provider publishing an api base must be flagged"
    );
    assert!(
        !provider("openai").api_base_quarantined,
        "a provider publishing none must not be; a flag that is always true \
         cannot tell a consumer anything"
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

/// A published zero limit and an absent limit are the same claim: unknown.
///
/// Both become `None`, so both serialize as `null` on the wire and a consumer
/// cannot distinguish them. That is deliberate — no model accepts zero tokens,
/// so a published zero is the upstream's spelling of "not stated" rather than a
/// capacity — and it is pinned here because it is INVISIBLE from the wire and
/// UNRECOVERABLE downstream.
///
/// BROCA traced the consumer half of this on 2026-08-13: their parser read a
/// published 0 as `Some(0)`, and their transform's `unwrap_or(0)` made
/// `Some(0)` and `None` arrive identically at the prompt-reduction seam. Fixing
/// that would not recover the distinction, because fusiform collapsed it first.
///
/// Measured case: `privatemode-ai/whisper-large-v3` published `context: 0` and
/// was corrected to 448 during the day.
#[test]
fn a_zero_limit_and_an_absent_limit_are_both_unknown() {
    let doc = |body: &str| {
        format!(r#"{{"p":{{"id":"p","name":"P","models":{{"m":{{"id":"m","name":"M"{body}}}}}}}}}"#)
    };
    let limits = |body: &str| {
        let text = doc(body);
        let m = normalize_models_dev(text.as_bytes())
            .unwrap_or_else(|e| panic!("{body:?} must normalize: {e}"))
            .catalog
            .models()
            .next()
            .expect("one model")
            .limits
            .clone();
        (m.context_tokens, m.output_tokens)
    };

    assert_eq!(limits(""), (None, None), "no limit block at all is unknown");
    assert_eq!(
        limits(r#","limit":{"context":0,"output":0}"#),
        (None, None),
        "a published zero is the upstream's spelling of 'not stated'"
    );

    // PER FIELD, not per model. 90 models mix real and zero limits, so a
    // row-level rule would discard a real limit alongside a zero one.
    assert_eq!(
        limits(r#","limit":{"context":8192,"output":0}"#),
        (Some(8192), None),
        "a real limit beside a zero one must survive"
    );
    assert_eq!(
        limits(r#","limit":{"context":0,"output":4096}"#),
        (None, Some(4096)),
        "and in the other direction"
    );
}

/// The normalized provider carries only what fusiform decided to keep.
///
/// An unread field is a decision nobody made. `name`, `doc` and `env` were all
/// normalized into the domain type and read by nothing, which is the state that
/// produced the `api_base` hazard: a value sitting in a serializable struct is
/// one refactor from a wire, and nothing about its name warns the person doing
/// the refactor.
///
/// This test is a FENCE at the producer rather than a value check. It fails
/// when a field is added, which is the moment the decision has to be made —
/// quarantine it, serve it, or do not parse it. Any of the three is fine; what
/// is not fine is a fourth field arriving with no decision behind it.
///
/// The mechanism is a total destructuring: adding a field to
/// `NormalizedProvider` makes this stop compiling.
#[test]
fn a_normalized_provider_carries_no_undecided_fields() {
    let outcome = catalog();
    let p = outcome.catalog.providers.first().expect("one provider");

    // Exhaustive by construction. A new field breaks the build here.
    let fusiform_core::normalize::NormalizedProvider {
        provider_id: _,
        npm_quarantined: _,
        api_base_quarantined: _,
        models: _,
    } = p;

    // And the quarantined values are not reachable from the whole catalog by
    // ANY rendering. Checked against the debug output rather than a serialized
    // one, because `NormalizedCatalog` does not implement `Serialize` at all --
    // which is a stronger property than this test first assumed and worth
    // stating: the domain type cannot be handed to serde by accident, so a wire
    // leak would take a deliberate mapping step.
    //
    // Debug is the widest rendering available, so a value absent from it is
    // absent from the type.
    let rendered = format!("{:?}", outcome.catalog);
    assert!(
        !rendered.contains("api.anyapi.ai"),
        "a quarantined base URL must not be reachable from the catalog at all"
    );
    assert!(
        !rendered.contains("@ai-sdk/"),
        "a quarantined npm adapter must not be reachable either"
    );

    // The control: a value that SHOULD be there is, so the two assertions above
    // are not passing because the rendering is empty.
    assert!(
        rendered.contains("claude-sonnet-4-5"),
        "the rendering must actually contain the catalog"
    );
}

/// Every tier the upstream publishes is a CONTEXT tier, and a tier that is not
/// must be refused rather than silently keyed as one.
///
/// # Why this is a fence and not a fact
///
/// `ServedFact::key` documents that `rate.*.above_context.<n>` means "the rate
/// when CONTEXT exceeds n", and grounds that on the upstream's own
/// discriminator: 370 of 370 tier rows carried `type: "context"` when
/// measured. A consumer pricing usage reads that sentence and computes money
/// from it.
///
/// If the upstream ever ships a tier keyed on something else — output size,
/// request count, a time window — and normalization treated it as a context
/// tier, the served key would state a threshold on the wrong axis and the
/// documented meaning would silently become false. The refusal is what keeps
/// the sentence true, so the refusal is what gets tested.
#[test]
fn a_tier_that_is_not_a_context_tier_is_refused() {
    // A payload identical to a real one except for the tier discriminator.
    let payload = r#"{
      "acme": {
        "id": "acme",
        "name": "Acme",
        "models": {
          "m1": {
            "id": "m1",
            "name": "M1",
            "cost": {
              "input": 5,
              "output": 30,
              "tiers": [
                { "input": 10, "output": 45, "tier": { "type": "output", "size": 272000 } }
              ]
            },
            "limit": { "context": 400000, "output": 128000 },
            "modalities": { "input": ["text"], "output": ["text"] }
          }
        }
      }
    }"#;

    let err = fusiform_core::normalize::normalize_models_dev(payload.as_bytes())
        .expect_err("a non-context tier must be refused, not keyed as a context tier");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("context") || msg.contains("tier"),
        "the refusal must name the tier type it rejected, or an operator \
         cannot tell which row to look at: {msg}"
    );

    // CONTROL: the same payload with a context tier normalizes, or the refusal
    // above proves only that the fixture is malformed.
    let ok = payload.replace(r#""type": "output""#, r#""type": "context""#);
    assert_ne!(ok, payload, "the control mutation must apply");
    let outcome = fusiform_core::normalize::normalize_models_dev(ok.as_bytes())
        .expect("a context tier must normalize");
    assert_eq!(
        outcome.catalog.models().count(),
        1,
        "control: the context-tier payload must produce the model"
    );
}
