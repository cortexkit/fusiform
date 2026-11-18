use super::*;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::BoundaryKind;
use fusiform_store::NewEra;

fn store() -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    })
    .unwrap();
    (store, dir)
}

fn era(provider: &str, model: &str, key: &str, value: &str) -> NewEra {
    NewEra {
        source: SourceId::ModelsDev,
        provider_id: provider.to_string(),
        model_id: model.to_string(),
        fact_key: FactKey::from_stored(key.to_string()),
        value_json: value.to_string(),
        boundary_at: Timestamp(1_000),
        boundary_kind: BoundaryKind::Seed,
        observation_id: None,
    }
}

fn seed(store: &CatalogStore, provider: &str, model: &str, rate: Option<i64>, open: bool) {
    let mut eras = vec![
        era(provider, model, "existence", r#""present""#),
        era(provider, model, "model.family", r#""glm""#),
        era(
            provider,
            model,
            "model.open_weights",
            if open { "true" } else { "false" },
        ),
    ];
    if let Some(u) = rate {
        eras.push(era(
            provider,
            model,
            "rate.input",
            &format!(r#"{{"state":"priced","units":{u},"exponent":9,"currency":"USD"}}"#),
        ));
    }
    store.append_eras(&eras).unwrap();
}

/// Add one rate dimension to an existing row.
fn add_rate(store: &CatalogStore, provider: &str, model: &str, key: &str, value: &str) {
    store
        .append_eras(&[era(provider, model, key, value)])
        .unwrap();
}

fn facts_of(
    store: &CatalogStore,
    provider: &str,
    model: &str,
) -> BTreeMap<String, serde_json::Value> {
    let (m, _) = store
        .read_model(SourceId::ModelsDev, provider, model, None)
        .unwrap();
    let m = m.expect("the row must exist");
    m.facts
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                serde_json::from_str(v).unwrap_or(serde_json::Value::Null),
            )
        })
        .collect()
}

fn creators() -> crate::creators::Creators {
    [("glm".to_string(), "zai".to_string())]
        .into_iter()
        .collect()
}

/// An unpriced OPEN-weight row inherits the creator's rate, disclosed.
#[test]
fn an_unpriced_open_weight_row_inherits_and_says_whose_price_it_is() {
    let (store, _d) = store();
    seed(&store, "zai", "glm-x", Some(75_000_000), true);
    seed(&store, "ollama-cloud", "glm-x", None, true);

    let mut facts = facts_of(&store, "ollama-cloud", "glm-x");
    // Control: the reseller genuinely publishes nothing, so what follows is
    // about inheritance rather than about the fixture.
    assert!(
        !facts.contains_key("rate.input"),
        "control: the reseller must be unpriced"
    );

    inherit_rate_for(
        &store,
        SourceId::ModelsDev,
        &creators(),
        &mut facts,
        "glm-x",
    );

    let rate = facts.get("rate.input").expect("must inherit");
    assert_eq!(rate["units"], 75_000_000);
    assert_eq!(
        rate["inherited_from"]["provider_id"], "zai",
        "it must say whose price this is, or a consumer cannot tell it \
         from a published one"
    );
}

/// Closed weights do not inherit: two providers serving one closed model
/// are two offerings that share a name, not one artefact.
#[test]
fn a_closed_weight_row_does_not_inherit() {
    let (store, _d) = store();
    seed(&store, "zai", "closed-y", Some(50_000_000), false);
    seed(&store, "ollama-cloud", "closed-y", None, false);

    let mut facts = facts_of(&store, "ollama-cloud", "closed-y");
    inherit_rate_for(
        &store,
        SourceId::ModelsDev,
        &creators(),
        &mut facts,
        "closed-y",
    );
    assert!(
        !facts.contains_key("rate.input"),
        "a closed model must stay unpriced even though a creator price exists"
    );
}

/// Inheritance carries the creator's base rates and never its mode rates.
///
/// A mode is a way of calling the creator's own endpoint; the reseller may
/// not offer it at all, so the creator's price for it would read as the
/// reseller's price for a mode nobody established it has.
#[test]
fn inheritance_never_supplies_a_mode_rate() {
    let (store, _d) = store();
    seed(&store, "zai", "glm-m", Some(75_000_000), true);
    add_rate(
        &store,
        "zai",
        "glm-m",
        "rate.input.mode.fast",
        r#"{"state":"priced","units":150000000,"exponent":9,"currency":"USD"}"#,
    );
    seed(&store, "ollama-cloud", "glm-m", None, true);

    let mut facts = facts_of(&store, "ollama-cloud", "glm-m");
    inherit_rate_for(
        &store,
        SourceId::ModelsDev,
        &creators(),
        &mut facts,
        "glm-m",
    );

    // Control: inheritance did fire, so the absence below is the filter.
    assert_eq!(facts["rate.input"]["units"], 75_000_000);
    assert!(
        !facts.contains_key("rate.input.mode.fast"),
        "a mode rate must never be inherited: {facts:?}"
    );
}

/// A row that publishes only mode rates has a price card, so it does not
/// inherit base rates from the creator.
#[test]
fn a_row_with_only_mode_rates_does_not_inherit() {
    let (store, _d) = store();
    seed(&store, "zai", "glm-n", Some(75_000_000), true);
    seed(&store, "ollama-cloud", "glm-n", None, true);
    add_rate(
        &store,
        "ollama-cloud",
        "glm-n",
        "rate.input.mode.fast",
        r#"{"state":"priced","units":90000000,"exponent":9,"currency":"USD"}"#,
    );

    let mut facts = facts_of(&store, "ollama-cloud", "glm-n");
    inherit_rate_for(
        &store,
        SourceId::ModelsDev,
        &creators(),
        &mut facts,
        "glm-n",
    );

    assert!(
        !facts.contains_key("rate.input"),
        "a row publishing a mode rate publishes a price card and must not \
         have its base filled from the creator: {facts:?}"
    );
    assert_eq!(
        facts["rate.input.mode.fast"]["units"], 90_000_000,
        "its own mode rate stays exactly as published"
    );
}

/// A published rate is never replaced. This fills a hole; it does not
/// correct anyone's price.
#[test]
fn a_published_rate_is_left_exactly_as_published() {
    let (store, _d) = store();
    seed(&store, "zai", "glm-x", Some(75_000_000), true);
    seed(&store, "ollama-cloud", "glm-x", Some(9_000_000), true);

    let mut facts = facts_of(&store, "ollama-cloud", "glm-x");
    inherit_rate_for(
        &store,
        SourceId::ModelsDev,
        &creators(),
        &mut facts,
        "glm-x",
    );
    assert_eq!(
        facts["rate.input"]["units"], 9_000_000,
        "the reseller's own price must survive"
    );
    assert!(
        facts["rate.input"].get("inherited_from").is_none(),
        "and must not be marked as inherited"
    );
}

/// Every dimension the creator publishes is inherited, not a chosen two.
///
/// Measured before writing this: all 11 rows that inherit today have a
/// creator publishing more than input and output — a cache read price,
/// usually. Carrying two of four served a partial picture with nothing
/// marking it partial, and in this catalog an absent rate means "nobody
/// published this", which was false for exactly those dimensions.
#[test]
fn every_rate_dimension_the_creator_publishes_is_inherited() {
    let (store, _d) = store();
    seed(&store, "zai", "glm-x", Some(75_000_000), true);
    add_rate(
        &store,
        "zai",
        "glm-x",
        "rate.output",
        r#"{"state":"priced","units":250000000,"exponent":9,"currency":"USD"}"#,
    );
    add_rate(
        &store,
        "zai",
        "glm-x",
        "rate.cache_read",
        r#"{"state":"priced","units":15000000,"exponent":9,"currency":"USD"}"#,
    );
    // A published ZERO is a published price, and must travel like one.
    add_rate(
        &store,
        "zai",
        "glm-x",
        "rate.cache_write",
        r#"{"state":"stated_zero"}"#,
    );
    add_rate(
        &store,
        "zai",
        "glm-x",
        "rate.cached_input_audio",
        r#"{"state":"priced","units":400000000,"exponent":9,"currency":"USD"}"#,
    );
    seed(&store, "ollama-cloud", "glm-x", None, true);

    let mut facts = facts_of(&store, "ollama-cloud", "glm-x");
    inherit_rate_for(
        &store,
        SourceId::ModelsDev,
        &creators(),
        &mut facts,
        "glm-x",
    );

    for key in [
        "rate.input",
        "rate.output",
        "rate.cache_read",
        "rate.cache_write",
        "rate.cached_input_audio",
    ] {
        let v = facts.get(key).unwrap_or_else(|| {
            panic!(
                "{key} must be inherited: the creator publishes it, and serving it \
                 absent would tell a consumer nobody published a price that \
                 somebody did. Got: {facts:?}"
            )
        });
        assert_eq!(
            v["inherited_from"]["provider_id"], "zai",
            "{key} must say whose price it is"
        );
    }
    assert_eq!(
        facts["rate.cache_write"]["state"], "stated_zero",
        "a published zero must arrive as stated_zero rather than being dropped \
         for not being `priced`"
    );
}

/// A provider that published ANY rate keeps its own card, untouched.
///
/// # The production defect this pins
///
/// The gate asked "is rate.input priced?" as a proxy for "does this
/// provider publish a price card?". `stated_zero` is a PUBLISHED price --
/// the provider saying a dimension is free -- so a subscription provider
/// stating every dimension zero read as unpriced, and inheritance filled
/// the dimensions around it from someone else's card:
///
///     alibaba-token-plan/deepseek-v4-flash
///       input/output/cache_read/cache_write  stated_zero
///       reasoning                            priced 600000000  <- INHERITED
///
/// A router ranks that model as costing money for reasoning; a ledger
/// records a charge that does not exist. Worse than the gap inheritance was
/// built to fill, because an absent rate is visibly absent and a fabricated
/// one is indistinguishable from a real price.
///
/// A partial card is still a card: what is missing from it is the
/// provider's statement about what they do not charge for, and it stays
/// VISIBLY missing rather than being filled from elsewhere.
#[test]
fn a_provider_that_published_any_rate_keeps_its_own_card() {
    let (store, _d) = store();
    seed(&store, "zai", "glm-x", Some(75_000_000), true);
    add_rate(
        &store,
        "zai",
        "glm-x",
        "rate.reasoning",
        r#"{"state":"priced","units":600000000,"exponent":9,"currency":"USD"}"#,
    );

    // The subscription shape: every dimension published, all free.
    seed(&store, "ollama-cloud", "glm-x", None, true);
    for key in ["rate.input", "rate.output"] {
        add_rate(
            &store,
            "ollama-cloud",
            "glm-x",
            key,
            r#"{"state":"stated_zero"}"#,
        );
    }

    let mut facts = facts_of(&store, "ollama-cloud", "glm-x");
    // Control: the fixture really does state zero rather than omitting.
    assert_eq!(
        facts["rate.input"]["state"], "stated_zero",
        "control: a stated zero must be present, or this tests nothing"
    );

    inherit_rate_for(
        &store,
        SourceId::ModelsDev,
        &creators(),
        &mut facts,
        "glm-x",
    );

    assert_eq!(
        facts["rate.input"]["state"], "stated_zero",
        "a published zero must survive"
    );
    assert!(
        !facts.contains_key("rate.reasoning"),
        "and NO dimension may be imported beside it: the provider published \
         a card, and a dimension absent from it is their statement rather \
         than a hole to fill. Got: {facts:?}"
    );
}

/// A partial card is still a card.
///
/// Separated from the case above because the shapes differ and only one of
/// them existed in the catalog: a provider publishing output and nothing
/// else. Under the old gate its missing input was filled from the creator,
/// producing a blended card belonging to nobody.
#[test]
fn a_partial_card_is_not_topped_up_from_the_creator() {
    let (store, _d) = store();
    seed(&store, "zai", "glm-x", Some(75_000_000), true);

    seed(&store, "ollama-cloud", "glm-x", None, true);
    add_rate(
        &store,
        "ollama-cloud",
        "glm-x",
        "rate.output",
        r#"{"state":"priced","units":4000000,"exponent":9,"currency":"USD"}"#,
    );

    let mut facts = facts_of(&store, "ollama-cloud", "glm-x");
    inherit_rate_for(
        &store,
        SourceId::ModelsDev,
        &creators(),
        &mut facts,
        "glm-x",
    );

    assert_eq!(
        facts["rate.output"]["units"], 4_000_000,
        "the provider's own price survives"
    );
    assert!(
        !facts.contains_key("rate.input"),
        "and the dimension they did not publish stays VISIBLY absent rather \
         than being filled from another provider's card"
    );
}

/// History explains an inherited rate instead of denying it.
///
/// # The disagreement this pins
///
/// `catalog.get` serves a price on this row; `catalog.history` for the
/// same fact found no eras and rendered "check the fact key" — sending an
/// operator to hunt a typo in a key that had just been answered. Two
/// surfaces disagreeing about whether a fact exists, both correct.
///
/// Found by driving the shipped CLI against production, not by a test.
#[test]
fn history_names_where_an_inherited_rate_comes_from() {
    let (store, _d) = store();
    seed(&store, "zai", "glm-x", Some(75_000_000), true);
    seed(&store, "ollama-cloud", "glm-x", None, true);

    let origin = inheritance_origin(
        &store,
        SourceId::ModelsDev,
        "ollama-cloud",
        "glm-x",
        &FactKey::from_stored("rate.input".to_string()),
    )
    .expect("an inherited rate must name its origin");
    assert_eq!(origin.provider_id, "zai");

    // The creator's OWN row publishes the rate, so its history is real and
    // must not be explained away as inherited.
    assert!(
        inheritance_origin(
            &store,
            SourceId::ModelsDev,
            "zai",
            "glm-x",
            &FactKey::from_stored("rate.input".to_string()),
        )
        .is_none(),
        "a provider's own published rate must not report an origin"
    );

    // A non-rate fact is never inherited, so it must not acquire an
    // explanation either — without this the helper could answer for every
    // empty history in the catalog.
    assert!(
        inheritance_origin(
            &store,
            SourceId::ModelsDev,
            "ollama-cloud",
            "glm-x",
            &FactKey::from_stored("limit.context".to_string()),
        )
        .is_none(),
        "only rates are inherited"
    );
}

/// The ROUTE must carry the explanation, not just the helper.
///
/// Testing `inheritance_origin` directly cannot see a route that never
/// calls it — a mutation replacing the route's whole branch with `None`
/// SURVIVED the helper test. That is the same gap that produced the
/// `--rates` defect: the function was right and the route feeding it was
/// wrong, and a function tested through its own front door cannot see its
/// caller.
#[test]
fn the_history_route_serves_the_origin() {
    let (store, _d) = store();
    seed(&store, "zai", "glm-x", Some(75_000_000), true);
    seed(&store, "ollama-cloud", "glm-x", None, true);

    let response = serve_history(
        &store,
        br#"{"provider_id":"ollama-cloud","model_id":"glm-x","fact_key":"rate.input"}"#,
    )
    .expect("the request must be served");

    assert!(
        response.eras.is_empty(),
        "control: this row genuinely has no eras, which is why the \
         explanation is needed"
    );
    let origin = response.inherited_from.expect(
        "the route must explain an empty history that is served by \
         inheritance, or an operator is told to check a fact key that \
         catalog.get just answered",
    );
    assert_eq!(origin.provider_id, "zai");

    // Control: a row with real history must NOT carry the explanation, or
    // this passes against a route that attaches it unconditionally.
    let creator = serve_history(
        &store,
        br#"{"provider_id":"zai","model_id":"glm-x","fact_key":"rate.input"}"#,
    )
    .expect("the request must be served");
    assert!(!creator.eras.is_empty(), "control: the creator has history");
    assert!(
        creator.inherited_from.is_none(),
        "a published rate's history must not be explained as inherited"
    );
}

/// The BULK path must inherit under a rate filter too.
///
/// The test below drives the single-model path (provider AND model named).
/// A pricing consumer reading a whole provider takes the bulk path, and a
/// mutation removing the widening from THAT call site survived — so the two
/// paths were never compared under a filter.
#[test]
fn the_bulk_path_inherits_under_a_rate_filter() {
    let (store, _d) = store();
    seed(&store, "zai", "glm-x", Some(75_000_000), true);
    seed(&store, "ollama-cloud", "glm-x", None, true);

    let bulk = serve_catalog_get(
        &store,
        br#"{"provider_id":"ollama-cloud","fact_prefixes":["rate."]}"#,
    )
    .expect("the request must be served");

    let facts = bulk
        .models
        .get("ollama-cloud/glm-x")
        .expect("the model must be present in a bulk read");
    assert!(
        facts.contains_key("rate.input"),
        "a bulk read with a rate filter must inherit exactly as the \
         single-model read does: a pricing consumer reading a whole \
         provider is the ordinary case. Got: {facts:?}"
    );

    // Control: the single-model path on the same store, so a failure here
    // names WHICH path is wrong rather than saying inheritance is broken.
    let single = serve_catalog_get(
        &store,
        br#"{"provider_id":"ollama-cloud","model_id":"glm-x","fact_prefixes":["rate."]}"#,
    )
    .expect("the request must be served");
    assert!(
        single.models["ollama-cloud/glm-x"].contains_key("rate.input"),
        "control: the single-model path must still inherit"
    );
}

/// A point-in-time read never inherits.
///
/// `at` asks what the catalog HELD at an instant. An inherited rate is a
/// derivation fusiform makes today, from a creator table that may not have
/// existed then, so serving it into a historical answer puts a
/// present-tense claim inside a past-tense one. Every other inheritance
/// test reads the current catalog, so removing the `at` condition from the
/// gate passed the whole package until this test existed.
#[test]
fn a_point_in_time_read_does_not_inherit() {
    let (store, _d) = store();
    seed(&store, "zai", "glm-x", Some(75_000_000), true);
    seed(&store, "ollama-cloud", "glm-x", None, true);

    // Control: the current read of the same row inherits, so the assertion
    // below cannot pass against a store where inheritance never fires.
    let now = serve_catalog_get(
        &store,
        br#"{"provider_id":"ollama-cloud","model_id":"glm-x"}"#,
    )
    .expect("the current read must be served");
    assert!(
        now.models["ollama-cloud/glm-x"].contains_key("rate.input"),
        "control: the current read must inherit"
    );

    let then = serve_catalog_get(
        &store,
        br#"{"provider_id":"ollama-cloud","model_id":"glm-x","at_ms":2000}"#,
    )
    .expect("the point-in-time read must be served");
    let facts = then
        .models
        .get("ollama-cloud/glm-x")
        .expect("the model existed at that instant");
    assert!(
        !facts.contains_key("rate.input"),
        "a point-in-time read must serve what the catalog held, which had no \
         rate for this row; an inherited rate here is today's derivation \
         presented as history. Got: {facts:?}"
    );
}

/// A rate filter must not switch inheritance off.
///
/// # This is the test that was missing, and production proved it
///
/// The gates read `model.open_weights` and `model.family` from the fact map
/// the caller receives. `--rates` is exactly what a PRICING consumer
/// passes, and it filtered the gate's own inputs away, so the rule silently
/// did not fire. Verified against the running binary two days after
/// placement: the same model inherited with no filter and came back bare
/// with `--rates`.
///
/// Every unit test above passed throughout, because they call
/// `inherit_rate_for` with a map nobody filtered. The defect lived in the
/// seam between the function and the route that feeds it — so this test
/// drives `serve_catalog_get`, which is the only surface a consumer has.
///
/// The failure direction is the bad one: a gate with missing inputs reads
/// as "not eligible", which is indistinguishable from a model that
/// correctly does not inherit. The feature looked ABSENT rather than
/// broken, and absent is what it looked like before it existed.
#[test]
fn a_rate_filter_does_not_switch_inheritance_off() {
    let (store, _d) = store();
    // `glm` -> `zai` is in the SHIPPED table, so this drives the real
    // mapping rather than a fixture that agrees with itself.
    seed(&store, "zai", "glm-x", Some(75_000_000), true);
    seed(&store, "ollama-cloud", "glm-x", None, true);

    let filtered = serve_catalog_get(
        &store,
        br#"{"provider_id":"ollama-cloud","model_id":"glm-x","fact_prefixes":["rate."]}"#,
    )
    .expect("the request must be served");

    let facts = filtered
        .models
        .get("ollama-cloud/glm-x")
        .expect("the model must be present");

    let rate = facts.get("rate.input").unwrap_or_else(|| {
        panic!(
            "a rate filter must not switch inheritance off: the gates read \
             model.* facts, and filtering them away made the rule silently \
             not fire in production. Got: {facts:?}"
        )
    });
    assert_eq!(rate["units"], 75_000_000);
    assert_eq!(rate["inherited_from"]["provider_id"], "zai");

    // And the widening must not leak: the caller asked for rates, so the
    // facts fetched to gate the rule are dropped again before serving.
    assert!(
        !facts.contains_key("model.family"),
        "a --rates response must carry only rate facts, or fusiform is \
         answering a different question from the one asked: {facts:?}"
    );

    // Control: the same request WITHOUT a filter must also inherit, or this
    // test would pass against a build that inherits only under a filter.
    let unfiltered = serve_catalog_get(
        &store,
        br#"{"provider_id":"ollama-cloud","model_id":"glm-x"}"#,
    )
    .expect("the request must be served");
    assert!(
        unfiltered.models["ollama-cloud/glm-x"].contains_key("rate.input"),
        "control: inheritance must work unfiltered too"
    );
}

/// An uncurated family inherits nothing: staying unpriced is the honest
/// answer when nobody has established who published the weights.
#[test]
fn an_uncurated_family_inherits_nothing() {
    let (store, _d) = store();
    seed(&store, "zai", "glm-x", Some(75_000_000), true);
    seed(&store, "ollama-cloud", "glm-x", None, true);

    let mut facts = facts_of(&store, "ollama-cloud", "glm-x");
    inherit_rate_for(
        &store,
        SourceId::ModelsDev,
        &crate::creators::Creators::new(),
        &mut facts,
        "glm-x",
    );
    assert!(
        !facts.contains_key("rate.input"),
        "no curated creator means no inheritance"
    );
}
