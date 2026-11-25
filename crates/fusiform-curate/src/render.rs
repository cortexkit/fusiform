use std::collections::BTreeMap;

use fusiform_core::SourceId;
use fusiform_protocol::{UncertainFactWire, WithheldFactWire};

use crate::{
    creators::Creators, fact_overrides::FactOverrides, is_mode_rate, CurationSet, Facts,
    ServedState, RATE,
};

pub fn response_envelope(
    source: SourceId,
    resolved_at_ms: i64,
    catalog_version: i64,
    withheld: Vec<WithheldFactWire>,
    uncertain: Vec<UncertainFactWire>,
) -> ServedState {
    ServedState {
        next_page_token: None,
        source: source.as_str().to_string(),
        resolved_at_ms,
        catalog_version,
        models: BTreeMap::new(),
        withheld,
        uncertain,
        // Filled by the NAMED-read caller, which is the only path that knows a
        // specific model was excluded. A bulk read leaves it empty on purpose:
        // 805 retired models against 7,852 present, permanent in an append-only
        // history, so attaching them to every read would ship hundreds of
        // ancient retirements to callers who asked about none of them.
        retired: Vec::new(),
        // Filled by the alias paths in `serve_catalog_get`, which are the only
        // callers that serve one row's facts under another id.
        aliased: Vec::new(),
        curated: Vec::new(),
        login_methods: Vec::new(),
        quality_scores: None,
        overrides: Vec::new(),
        canonical_links: Vec::new(),
        speed_twins: Vec::new(),
        wire_corrections: Vec::new(),
        fact_overrides: Vec::new(),
        plane: None,
        billing_rules: Vec::new(),
    }
}

/// Parse a stored fact for the wire, and never substitute a MEANINGFUL value
/// when the parse fails.
///
/// Three call sites did this and two of them agreed. The third substituted
/// `Value::Null`, which in this catalog is not a neutral placeholder: the served
/// contract says a null limit means UNKNOWN CAPACITY and must never be defaulted
/// to a number. So a row this producer failed to parse would arrive at a
/// consumer as a statement fusiform makes deliberately about the upstream —
/// absent and unknown collapsed at the exact seam this catalog exists to keep
/// apart, in the producer.
///
/// The raw string is the honest substitute. It is visibly not a fact object, so
/// a consumer decoding it fails on the shape rather than believing a plausible
/// absence, and the bytes that failed to parse travel with the failure instead
/// of being replaced by a verdict.
///
/// Reaching here at all means a stored row is not the JSON this producer wrote,
/// which is corruption or a representation change rather than upstream data —
/// but the point is what it does when it happens, not how often.
pub fn fact_for_the_wire(value_json: &str) -> serde_json::Value {
    serde_json::from_str(value_json)
        .unwrap_or_else(|_| serde_json::Value::String(value_json.to_string()))
}

/// Fill in the currency provenance of a rate written before the field existed.
///
/// # Why absence cannot simply be left alone
///
/// `unit_provenance` began being stored in fusiform-protocol 0.11.0. The live
/// store holds 19,707 rate eras written before it, and an append-only store
/// never rewrites them — they are what fusiform believed when they were
/// written, and editing them would destroy the record this store exists to
/// keep.
///
/// So absence would have to mean something on the wire, and it CANNOT mean
/// nothing: `UnitProvenance::Unknown` is a real variant meaning "no statement
/// and no policy covers this", which is the state that stops a non-USD
/// provider being silently priced in dollars. An absent field would be
/// indistinguishable from that, and a consumer treating a pre-0.11.0 USD row
/// as unknown-provenance would refuse to price a rate that is perfectly well
/// established.
///
/// # Why filling it in is a fact rather than an invention
///
/// The population is CLOSED and its provenance is known with certainty. Every
/// rate row written before 0.11.0 came through one normalization path, whose
/// only currency branch is `AssumedByPolicy(models-dev-usd-v1)` — models.dev
/// publishes no currency field at all, so there has never been another way for
/// a rate to acquire one. After 0.11.0 every new row carries the field
/// explicitly, so the defaulted population can only shrink.
///
/// This is the one shape where a serve-time default is honest: not "we do not
/// know so here is a guess", but "we know, and the row predates the column".
///
/// The claim's truth rests on there being exactly one policy, which is
/// enforced by `only_one_currency_policy_has_ever_existed` — a second policy
/// makes the default ambiguous and must force a decision rather than silently
/// mislabel rows written under the other one.
pub fn name_the_provenance_of_a_pre_0_11_row(value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if obj.get("state").and_then(|s| s.as_str()) != Some("priced") {
        return;
    }
    if obj.contains_key("unit_provenance") {
        return;
    }
    obj.insert(
        "unit_provenance".to_string(),
        serde_json::json!({
            "kind": "assumed_by_policy",
            "policy": fusiform_core::PolicyId::models_dev_usd_v1().0,
        }),
    );
}

/// Decode one upstream row and apply the curation that precedes inheritance.
/// A dated read supplies its resolved override rows but no current tables.
pub fn render_row<'a>(
    response: &mut ServedState,
    identity: (&str, &str),
    stored: impl IntoIterator<Item = (&'a str, &'a str)>,
    withheld: &[&str],
    requested_prefixes: Option<&Vec<String>>,
    overrides: Option<&FactOverrides>,
    current: Option<&CurationSet<'_>>,
) -> Facts {
    let (provider, model) = identity;
    let mut facts = BTreeMap::new();
    for (key, raw) in stored {
        let mut value = fact_for_the_wire(raw);
        // Withdrawn connection claims, display names and sparse model types
        // stay in history as null eras, not advertised unknown values.
        if key.starts_with("wire.") && value.is_null() {
            continue;
        }
        if matches!(key, "model.name" | "provider.name" | "model.type") && value.is_null() {
            continue;
        }
        name_the_provenance_of_a_pre_0_11_row(&mut value);
        facts.insert(key.to_string(), value);
    }
    if let Some("decision" | "extraction") = facts.get("model.type").and_then(|v| v.as_str()) {
        facts.insert("model.kind".to_string(), facts["model.type"].clone());
    }
    let protocol_family = match facts
        .get("wire.shape")
        .and_then(|claim| claim["value"].as_str())
    {
        Some("images") => Some("openai.images"),
        Some("interactions") => Some("google.interactions"),
        Some("speech") => Some("elevenlabs.text_to_speech"),
        _ => None,
    };
    let mut endpoint_withheld = false;
    if let Some(rows) = overrides {
        // An override must still match the upstream value it replaces (or
        // fill an absent fact). Check the value at the requested instant.
        response
            .fact_overrides
            .extend(crate::fact_overrides::apply_rows(
                rows,
                provider,
                model,
                &mut facts,
                requested_prefixes,
                withheld,
            ));
    }
    if let Some(current) = current {
        response
            .fact_overrides
            .extend(crate::fact_overrides::apply_rows(
                current.fact_overrides,
                provider,
                model,
                &mut facts,
                requested_prefixes,
                withheld,
            ));
        if let Some(correction) = crate::wire_corrections::apply(
            current.wire_corrections,
            current.complete_providers,
            provider,
            model,
            &mut facts,
            requested_prefixes,
        ) {
            endpoint_withheld = correction.withheld;
            response.wire_corrections.push(correction);
        }
        if let Some(correction) = crate::wire_corrections::apply_provider_api(
            current.wire_corrections,
            provider,
            model,
            &mut facts,
            requested_prefixes,
        ) {
            response.wire_corrections.push(correction);
        }
        if !endpoint_withheld {
            let npm = facts
                .get("wire.npm")
                .and_then(|claim| claim["value"].as_str());
            let shape = facts
                .get("wire.shape")
                .and_then(|claim| claim["value"].as_str());
            if let Some(family) = protocol_family.or_else(|| {
                npm.and_then(|npm| crate::wire_families::lookup(current.wire_families, npm, shape))
            }) {
                facts.insert("wire.family".to_string(), serde_json::json!(family));
            }
        }
        if let Some(link) = apply_canonical_link(
            current.canonical_links,
            provider,
            model,
            &mut facts,
            requested_prefixes,
        ) {
            response.canonical_links.push(link);
        }
    }
    if current.is_none() && !endpoint_withheld {
        if let Some(family) = protocol_family {
            facts.insert("wire.family".to_owned(), serde_json::json!(family));
        }
    }
    facts
}

/// The exact creator row needed to price an otherwise unpriced open-weight row.
pub fn inheritance_source<'a>(creators: &'a Creators, facts: &Facts) -> Option<(&'a str, String)> {
    let publishes_a_rate = facts.keys().any(|k| k.starts_with(RATE));
    if publishes_a_rate {
        return None;
    }

    if facts.get("model.open_weights").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let family = facts.get("model.family").and_then(|v| v.as_str())?;
    // A family without a sourced creator stays unpriced.
    let creator = creators.get(family)?;
    Some((creator, family.to_owned()))
}

/// Copy the creator's non-mode rates, preserving the source of each price.
pub fn inherit_rates<'a>(
    facts: &mut Facts,
    creator: &str,
    family: &str,
    origin: impl IntoIterator<Item = (&'a str, &'a str)>,
) {
    // EVERY rate dimension the creator publishes, not a chosen two.
    //
    // This began as `["rate.input", "rate.output"]` because those are what a
    // router ranks on. Measured against the live store: all 11 rows that
    // inherit have a creator publishing MORE than those two — a cache read
    // price, usually — so every inheriting row was served a partial picture
    // with nothing saying it was partial.
    //
    // A consumer pricing a cached request then sees an inherited input rate and
    // an absent cache rate, and absent means "nobody published this" in this
    // catalog's vocabulary. That reading is false: somebody did publish it, and
    // fusiform declined to carry it for no reason a consumer could see.
    //
    // If the creator's price is a defensible stand-in for one dimension it is a
    // stand-in for all of them, and the rule is easier to state with no
    // exceptions: WHERE THE RESELLER PUBLISHED NOTHING, THE CREATOR'S PRICE
    // STANDS IN, DISCLOSED. Tiered keys ride along under the same rule; none
    // exist among the inheriting rows today, so this is the rule applying
    // uniformly rather than a case anyone has exercised.
    //
    // MODE RATES DO NOT RIDE ALONG. A mode is a way of calling one provider's
    // endpoint (a service tier, a speed flag), and whether a reseller offers
    // that mode at all is something only the reseller's own row can say. The
    // creator's price for a mode the reseller may not have would read as the
    // reseller's price for it.
    for (key, raw) in origin {
        if !key.starts_with(RATE) || is_mode_rate(key) {
            continue;
        }
        // A key absent from `facts` means the upstream published nothing —
        // and that reading depends on an invariant held in another crate.
        //
        // A fact WITHHELD by a correction is also absent here: the store drops
        // it from the map and reports it in `withheld` instead. If that could
        // happen on this path, inheritance would fill the slot fusiform is
        // actively refusing to answer, and the response would carry both a
        // price and a notice saying that price was withheld — contradictory,
        // with the consumer having no reason to read the notice.
        //
        // It cannot, for a structural reason rather than a lucky one:
        // `affected_until` can never exceed the instant a correction is
        // recorded, so corrected intervals are bounded in the past and never
        // intersect a current store read. Dated reads do not call inheritance,
        // so a withheld value is never replaced by a creator's price.
        //
        // Fenced in fusiform-store/tests/it/correction_never_denies_now.rs. If
        // that bound is ever relaxed, this loop needs the withheld key set
        // passed in; nothing here would fail loudly on its own.
        //
        // PER KEY, never once for the whole set.
        //
        // The gate above reads `rate.input` alone, and this loop used to write
        // two keys on the strength of it — so a reseller that published an
        // output price and no input price would have had its OWN price
        // overwritten by the creator's. No row in the catalog has that shape
        // today (measured: 0), which is exactly why it would have gone
        // unnoticed. Overwriting a published price is the one thing this rule
        // promises never to do.
        if facts.contains_key(key) {
            continue;
        }

        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        // Whatever state the creator published, including `stated_zero`.
        // "The originator states this is free" is a published price and carries
        // the same standing as a number; skipping it would reintroduce the
        // partial picture one level down.
        name_the_provenance_of_a_pre_0_11_row(&mut value);
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                "inherited_from".to_string(),
                serde_json::json!({
                    "provider_id": creator,
                    "family": family,
                    "basis": "open_weights",
                }),
            );
        }
        facts.insert(key.to_string(), value);
    }
}

/// Fill a null or absent `model.canonical_id` and return its source disclosure.
fn apply_canonical_link(
    links: &crate::canonical_links::CanonicalLinks,
    provider: &str,
    model: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
) -> Option<fusiform_protocol::CanonicalLinkWire> {
    const KEY: &str = "model.canonical_id";
    if prefixes.is_some_and(|prefixes| !prefixes.iter().any(|p| KEY.starts_with(p)))
        || facts.get(KEY).is_some_and(|id| !id.is_null())
    {
        return None;
    }
    let row = links.get(&(provider.to_string(), model.to_string()))?;
    facts.insert(KEY.to_string(), serde_json::json!(row.canonical_id));
    Some(fusiform_protocol::CanonicalLinkWire {
        model: format!("{provider}/{model}"),
        canonical_id: row.canonical_id.clone(),
        source_ref: row.source_ref.clone(),
        quote: row.quote.clone(),
        lab_source_ref: row.lab_source_ref.clone(),
        lab_quote: row.lab_quote.clone(),
        review_by_ms: row.review_by_ms,
    })
}
