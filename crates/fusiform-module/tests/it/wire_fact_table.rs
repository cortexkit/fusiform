//! The wire crate's fact table, held against the real producer.
//!
//! `SERVED_FACTS` is the only place a consumer can learn which served fields
//! may change their request bytes. That distinction lived in fusiform's charter
//! and in one conversation with the consumer who named the three byte-affecting
//! fields — neither reachable from the artifact a consumer compiles against,
//! which made it a relationship recorded nowhere a check could see.
//!
//! This test lives in `fusiform-module` rather than beside the other vocabulary
//! tests because it needs both the producer and the wire crate, and
//! `fusiform-protocol` stays a leaf: a consumer compiling against it must not
//! pull in the store.
//!
//! # What this test could NOT have told me, stated because it is not obvious
//!
//! ASTRO's test for a check worth trusting: could this artifact have told me I
//! was wrong? Applied here, the answer differs by claim, and only one of the
//! three is genuinely checked from this side.
//!
//! **The key set: yes.** It comes from the real normalizer over real upstream
//! bytes. A fact appearing or vanishing reddens this, and neither the table nor
//! my belief about it can prevent that.
//!
//! **Rates are money: yes, structurally.** The key prefix determines the class,
//! so a rate classified otherwise is a contradiction the code can find.
//!
//! **The classification of everything else: NO.** Whether
//! `capability.attachment` is advisory or byte-affecting is a fact about a
//! CONSUMER'S RENDERER, and nothing in this repository can contradict me about
//! it. The three byte-affecting entries are marked as such because BROCA read
//! them out of their own source and told me; the advisory entries are advisory
//! because nobody has said otherwise, which is a weaker claim wearing the same
//! typeface.
//!
//! That asymmetry is not fixable from here and pretending otherwise would be
//! the defect this file exists to prevent. It is why the classification carries
//! attribution in `SERVED_FACTS` rather than reading as fusiform's own
//! determination, and why the standing question to consumers is whether the
//! byte-affecting set is still complete rather than whether it looks right.

use fusiform_core::normalize::normalize_models_dev_with;
use fusiform_store::ingest::fact_keys_of;

const FIXTURE: &[u8] = include_bytes!("../../../fusiform-core/fixtures/models-dev-excerpt.json");

/// The wire crate's fact table lists exactly the facts fusiform serves.
///
/// `SERVED_FACTS` is the only place a consumer can learn which fields may
/// change their request bytes — the distinction lived in fusiform's charter and
/// in one conversation, neither of which is reachable from the artifact a
/// consumer compiles against. That made it a relationship recorded nowhere a
/// check could see, which is the failure mode this test exists to prevent
/// recurring.
///
/// A table nobody checks drifts, and a drifted table is worse than none: it
/// tells a consumer a fact is advisory while the producer serves it as
/// byte-affecting. So the table is held against the real producer — the
/// normalizer running over real upstream bytes, plus the existence fact that
/// ingest writes from presence logic.
#[test]
fn the_wire_fact_table_matches_what_the_producer_emits() {
    use fusiform_protocol::{FactClass, SERVED_FACTS};

    // Normalized with the shipped input-audio evidence, as production ingest
    // does, so `rate.input_audio` is produced where the deployed producer
    // would produce it.
    // The older excerpt predates model-level request shapes.
    let mut fixture: serde_json::Value = serde_json::from_slice(FIXTURE).unwrap();
    fixture["anthropic"]["models"]["claude-sonnet-4-5"]["provider"] =
        serde_json::json!({"shape": "responses"});
    let outcome = normalize_models_dev_with(
        &serde_json::to_vec(&fixture).unwrap(),
        &fusiform_module::input_audio::evidence(),
    )
    .expect("the fixture must normalize");

    let mut produced: Vec<String> = Vec::new();
    for model in outcome.catalog.models() {
        for key in fact_keys_of(model) {
            produced.push(key.as_str().to_string());
        }
    }
    // Written by plan_ingest from presence logic rather than from a model's
    // fields, so it never appears in fact_keys_of.
    produced.push("existence".to_string());
    produced.sort();
    produced.dedup();

    let tabled: Vec<&str> = SERVED_FACTS.iter().map(|f| f.key).collect();

    // Every produced fact must be in the table. A fact a consumer receives and
    // cannot classify is exactly the gap this closes.
    for key in &produced {
        if key.contains(".above_context.") {
            // Tiered keys carry an upstream threshold; the base class is what
            // the table describes, checked below.
            let base = key.split_once(".above_context.").unwrap().0;
            assert!(
                tabled.contains(&base),
                "tiered fact {key:?} has base {base:?}, which the wire table does not list"
            );
            continue;
        }
        if key.contains(".mode.") {
            // Mode rates carry the upstream's mode name; the base class is
            // what the table describes, as for tiers.
            let base = key.split_once(".mode.").unwrap().0;
            assert!(
                tabled.contains(&base),
                "mode fact {key:?} has base {base:?}, which the wire table does not list"
            );
            continue;
        }
        assert!(
            tabled.contains(&key.as_str()),
            "the producer emits {key:?} and the wire table does not list it — a \
             consumer receiving it cannot tell whether it affects their request bytes"
        );
    }

    // And every tabled fact must be produced, or the table promises something
    // that never arrives. A curated-only fact is produced by the curated rows
    // rather than by the normalizer; see `CURATED_ONLY`.
    let mut curated: Vec<String> = fusiform_module::curated_models::models()
        .values()
        .flat_map(|row| row.facts.keys().cloned())
        .collect();
    let (store, _dir) = super::curated_wire::store();
    let response = super::curated_wire::get(&store, serde_json::json!({"include_retired":true}));
    curated.extend(response.models.values().flat_map(|facts| {
        facts
            .keys()
            .filter(|key| {
                CURATED_ONLY.contains(&key.as_str()) || LINEAGE_ONLY.contains(&key.as_str())
            })
            .cloned()
    }));
    // An addition-only fact is produced by a shipped addition row.
    curated.extend(
        fusiform_module::fact_overrides::overrides()
            .values()
            .filter(|row| row.replaces.is_null())
            .map(|row| row.fact_key.clone()),
    );
    // Grades are intentionally empty in the shipped file. Exercise the actual
    // serving path with fixture grades rather than promising unproduced keys.
    let quality = super::golden_payload::render_synthetic_quality_scores();
    curated.extend(
        quality["response"]["models"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|facts| {
                facts
                    .as_object()
                    .unwrap()
                    .keys()
                    .filter(|key| QUALITY_ONLY.contains(&key.as_str()))
                    .cloned()
            }),
    );
    let unproduced = unproduced_tabled_facts(&produced, &curated, &tabled);
    assert!(unproduced.is_empty(), "{}", unproduced.join("\n"));

    // The fields BROCA enumerated from their own render path, named
    // individually rather than counted: a count would pass if one were swapped
    // for another.
    //
    // `limit.output` is deliberately NOT here. It was byte-affecting on my
    // belief that a consumer rendered it as a request parameter, and BROCA
    // enumerated `resolve_frozen` and found zero reads of it — their output cap
    // comes from the caller's max_tokens. That correction is the reason this
    // list is short: it is what a consumer read out of their source, not what
    // the field names suggest.
    for key in ["limit.context", "capability.reasoning"] {
        let fact = SERVED_FACTS
            .iter()
            .find(|f| f.key == key)
            .unwrap_or_else(|| panic!("{key} must be in the table"));
        assert_eq!(
            fact.class,
            FactClass::ByteAffecting,
            "{key} is rendered into a provider request by a consumer; \
             classifying it as advisory tells them a wrong value is cosmetic"
        );
    }

    let structured_output = SERVED_FACTS
        .iter()
        .find(|f| f.key == "capability.structured_output")
        .expect("capability.structured_output is served");
    assert_eq!(
        structured_output.class,
        FactClass::Advisory,
        "the upstream model claim does not establish route-specific enforcement"
    );
    for key in ["wire.npm", "wire.api", "wire.shape"] {
        assert_eq!(
            SERVED_FACTS
                .iter()
                .find(|fact| fact.key == key)
                .unwrap()
                .class,
            FactClass::Advisory,
            "connection claims leave transport interpretation to the consumer"
        );
    }
    for key in QUALITY_ONLY.iter().chain(LINEAGE_ONLY) {
        assert_eq!(
            SERVED_FACTS
                .iter()
                .find(|fact| fact.key == *key)
                .unwrap()
                .class,
            FactClass::Advisory
        );
    }

    // Every rate is money. A rate classified as advisory would tell a consumer
    // that a wrong value cannot cost anything.
    for fact in SERVED_FACTS.iter().filter(|f| f.key.starts_with("rate.")) {
        assert_eq!(
            fact.class,
            FactClass::Money,
            "{} is a rate and must be classified as money",
            fact.key
        );
    }
}

/// Served facts that only fusiform's curated rows state.
///
/// models.dev publishes no such field, so the normalizer must never produce
/// one: a value inferred for an upstream row would be fusiform's guess served
/// as a statement. `model.kind` is absent on every upstream row, which means
/// "not stated", never "chat".
pub(crate) const CURATED_ONLY: &[&str] = &[
    "model.kind",
    "route.reasoning_wire_ids",
    "route.reasoning_default",
    "successor",
];

/// Served facts that only fusiform's sourced fact-override additions state.
///
/// Like `CURATED_ONLY`, models.dev publishes no such field and the
/// normalizer must never produce one. Unlike it, these are served on upstream
/// rows, so the golden fixture must always carry them.
pub(crate) const ADDITION_ONLY: &[&str] = &["capability.server_side_fallback"];

/// Operator judgments are joined at read time, never inferred from upstream.
const QUALITY_ONLY: &[&str] = &["quality.elo", "quality.eq"];

/// `catalog.get` computes `model.lineage` on current reads, never the normalizer.
const LINEAGE_ONLY: &[&str] = &["model.lineage"];

/// Tabled facts nothing emits, and curated-only facts the normalizer emits.
///
/// A tabled fact must come from the normalizer, or be declared curated-only,
/// addition-only, quality-only or lineage-only and be carried by at least one
/// curated row, shipped addition row or actual served response (all passed in `curated`).
fn unproduced_tabled_facts(
    normalized: &[String],
    curated: &[String],
    tabled: &[&str],
) -> Vec<String> {
    let mut problems = Vec::new();
    let not_upstream = || {
        CURATED_ONLY
            .iter()
            .chain(ADDITION_ONLY)
            .chain(QUALITY_ONLY)
            .chain(LINEAGE_ONLY)
    };
    for key in not_upstream() {
        if normalized.iter().any(|p| p == key) {
            problems.push(format!(
                "the normalizer emits {key:?}, which only curated rows may state"
            ));
        }
    }
    for key in tabled {
        let produced = if not_upstream().any(|k| k == key) {
            curated.iter().any(|p| p == key)
        } else {
            normalized.iter().any(|p| p == key)
        };
        if !produced {
            problems.push(format!(
                "the wire table lists {key:?} and the producer never emits it"
            ));
        }
    }
    problems
}

#[test]
fn planted_unproduced_or_inferred_curated_only_fact_is_reported() {
    let strings = |keys: &[&str]| keys.iter().map(|k| k.to_string()).collect::<Vec<_>>();
    // Control: an upstream fact from the normalizer and the curated-only fact
    // from a curated row are both produced.
    assert!(unproduced_tabled_facts(
        &strings(&["existence"]),
        &strings(&["model.kind"]),
        &["existence", "model.kind"],
    )
    .is_empty());
    // A curated-only fact no curated row carries promises something that
    // never arrives.
    assert_eq!(
        unproduced_tabled_facts(&strings(&["existence"]), &[], &["existence", "model.kind"]),
        ["the wire table lists \"model.kind\" and the producer never emits it"]
    );
    // A curated row cannot stand in for an upstream fact.
    assert_eq!(
        unproduced_tabled_facts(&[], &strings(&["existence"]), &["existence"]),
        ["the wire table lists \"existence\" and the producer never emits it"]
    );
    // The normalizer inferring a curated-only fact is reported.
    assert_eq!(
        unproduced_tabled_facts(
            &strings(&["model.kind"]),
            &strings(&["model.kind"]),
            &["model.kind"]
        ),
        ["the normalizer emits \"model.kind\", which only curated rows may state"]
    );
    assert!(
        unproduced_tabled_facts(&[], &strings(&["model.lineage"]), &["model.lineage"],).is_empty()
    );
    assert_eq!(
        unproduced_tabled_facts(&[], &[], &["model.lineage"]),
        ["the wire table lists \"model.lineage\" and the producer never emits it"]
    );
    assert_eq!(
        unproduced_tabled_facts(
            &strings(&["model.lineage"]),
            &strings(&["model.lineage"]),
            &["model.lineage"],
        ),
        ["the normalizer emits \"model.lineage\", which only curated rows may state"]
    );
}

/// `limit.output` is byte-affecting, because a named consumer renders it.
///
/// Its class follows a consumer's render path, not how the field sounds, and
/// that path has changed once already. Until 2026-09-25 BROCA took the output
/// cap only from the caller's max_tokens and had zero reads of this fact, so it
/// was advisory. BROCA v0.3.125 (placed 2026-09-25) made the catalog maximum
/// the default max_tokens for any send whose caller sets no cap, so a wrong
/// value now changes request bytes.
///
/// Pinned so a move in either direction is deliberate: moving it back needs
/// BROCA to stop rendering it, not a reader deciding it looks advisory.
#[test]
fn limit_output_is_byte_affecting_because_broca_renders_it_as_the_default_cap() {
    use fusiform_protocol::{FactClass, SERVED_FACTS};

    let fact = SERVED_FACTS
        .iter()
        .find(|f| f.key == "limit.output")
        .expect("limit.output is served");

    assert_eq!(
        fact.class,
        FactClass::ByteAffecting,
        "limit.output is BROCA's default max_tokens since v0.3.125, so a wrong \
         value changes request bytes. Reclassifying it advisory needs BROCA to \
         stop rendering it first."
    );
}

/// The module-surface doc's tool list names exactly the tools the module serves.
///
/// Found stale once: the served-tool list in the documentation named three
/// tools for several hours after `catalog.correct` went live. The list was
/// written when the read surface was the whole surface, and adding a tool did
/// not feel like changing the doc — so a reader citing it would have believed
/// the served surface was read-only, which is the most consequential property
/// in that list to be wrong about.
///
/// Checked in both directions: a served tool the doc omits, and a name in the
/// doc's list that the module no longer serves. The tool list is checkable, so
/// a test checks it rather than a human re-reading; the prose around it stays
/// a human's job.
#[test]
fn the_module_surface_doc_lists_exactly_the_served_tools() {
    const DOC: &str = include_str!("../../../../docs/module-surface.md");
    let named = documented_tools(DOC);
    // Both checks below iterate, and an iteration over nothing asserts
    // nothing: an extraction that stopped matching would pass in silence.
    assert!(
        named.len() >= 3,
        "found {} backticked names in the doc's 'Tools, as served:' paragraph, \
         which cannot be right; the extraction has stopped matching",
        named.len()
    );
    let (missing, phantom) = tool_list_drift(DOC, fusiform_protocol::TOOLS);
    assert!(
        missing.is_empty(),
        "served tools absent from docs/module-surface.md: {missing:?}"
    );
    assert!(
        phantom.is_empty(),
        "docs/module-surface.md names tools the module does not serve: {phantom:?}"
    );
}

/// The paragraph that states the served tools, joined onto one line because
/// the doc is hard-wrapped and reading only its first line would drop names.
fn tool_paragraph(doc: &str) -> String {
    doc.split("\n\n")
        .find(|p| p.trim_start().starts_with("Tools, as served:"))
        .expect("docs/module-surface.md states a paragraph beginning 'Tools, as served:'")
        .replace('\n', " ")
}

fn documented_tools(doc: &str) -> Vec<String> {
    tool_paragraph(doc)
        .split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// Served tools the doc omits, and documented names the module does not serve.
fn tool_list_drift<'a>(doc: &str, tools: &'a [&str]) -> (Vec<&'a str>, Vec<String>) {
    let named = documented_tools(doc);
    let missing = tools
        .iter()
        .copied()
        .filter(|tool| !named.iter().any(|n| n == tool))
        .collect();
    let phantom = named
        .into_iter()
        .filter(|name| !tools.contains(&name.as_str()))
        .collect();
    (missing, phantom)
}

#[test]
fn planted_undocumented_tool_is_reported() {
    let (missing, phantom) = tool_list_drift(
        "Tools, as served: `catalog.get`, `catalog.ghost`.",
        &["catalog.get", "catalog.lost"],
    );
    assert_eq!(missing, ["catalog.lost"]);
    assert_eq!(phantom, ["catalog.ghost"]);
}
