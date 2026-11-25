//! Sourced fact overrides on served catalog reads.
//!
//! The shipped row serves `limit.output` 65,536 for google/gemini-4-argon in
//! place of models.dev's 32,768. These tests drive the real serve path over a
//! store seeded from the embedded models.dev copy, which publishes 32,768.

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::normalize::normalize_models_dev;
use fusiform_core::{BoundaryKind, ObservationOutcome, SourceId, Timestamp};
use fusiform_module::route::{serve_catalog_get, serve_history, CatalogGetResponse};
use fusiform_store::ingest::plan_ingest;
use fusiform_store::{CatalogStore, NewObservation};
use serde_json::{json, Value};

const TARGET: &str = "google/gemini-4-argon";
const ALIAS: &str = "google/antigravity-gemini-4-argon";
const CONTROL: &str = "google/gemini-3.8-flash";

/// The embedded seed's google entry cut to the overridden model and one
/// control, with Argon's output limit set to `output`.
fn upstream(output: Value) -> Vec<u8> {
    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    let mut google = seed["google"].clone();
    google["models"] = json!({});
    for id in ["gemini-4-argon", "gemini-3.8-flash"] {
        let row = seed["google"]["models"][id].clone();
        assert!(row.is_object(), "the seed must carry google/{id}");
        google["models"][id] = row;
    }
    assert_eq!(
        google["models"]["gemini-4-argon"]["limit"]["output"],
        json!(32_768),
        "the embedded seed must publish the value the override replaces"
    );
    google["models"]["gemini-4-argon"]["limit"]["output"] = output;
    serde_json::to_vec(&json!({ "google": google })).unwrap()
}

fn ingest(store: &CatalogStore, bytes: &[u8], at: i64) {
    let observation = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(at),
            outcome: ObservationOutcome::Changed { snapshot_seq: at },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let catalog = normalize_models_dev(bytes).unwrap().catalog;
    let (kind, observation) = if at == 1_000 {
        (BoundaryKind::Seed, None)
    } else {
        (BoundaryKind::Observed, Some(observation))
    };
    let plan = plan_ingest(store, &catalog, Timestamp(at), kind, observation).unwrap();
    store.append_eras(&plan.eras).unwrap();
}

fn store_publishing(output: Value) -> (CatalogStore, tempfile::TempDir) {
    store_document(&upstream(output))
}

fn store_document(document: &[u8]) -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    ingest(&store, document, 1_000);
    (store, dir)
}

fn get(store: &CatalogStore, args: Value) -> CatalogGetResponse {
    serve_catalog_get(store, &serde_json::to_vec(&args).unwrap()).unwrap()
}

fn assert_no_added_reasoning_rate(reasoning: Option<bool>) {
    let mut row = json!({"id":"speech", "name":"Speech", "cost":{"output":10}});
    if let Some(reasoning) = reasoning {
        row["reasoning"] = json!(reasoning);
    }
    let document = json!({"openai":{"id":"openai", "name":"OpenAI", "models":{"speech":row}}});
    let (store, _dir) = store_document(&serde_json::to_vec(&document).unwrap());
    let overrides = fusiform_module::fact_overrides::overrides();
    let boundary =
        overrides[&("openai".into(), "*".into(), "rate.reasoning".into())].established_at_ms;
    fusiform_module::fact_overrides::apply_to_store(&store, overrides, boundary).unwrap();
    for at_ms in [None, Some(boundary)] {
        let read = get(
            &store,
            json!({"provider_id":"openai", "model_id":"speech", "at_ms":at_ms,
            "fact_prefixes":["capability.reasoning", "rate."]}),
        );
        let facts = &read.models["openai/speech"];
        assert_eq!(facts["rate.output"]["state"], "priced");
        assert_eq!(
            facts.get("capability.reasoning").and_then(Value::as_bool),
            reasoning
        );
        assert!(
            !facts.contains_key("rate.reasoning"),
            "{reasoning:?} at {at_ms:?}"
        );
        assert!(read.fact_overrides.is_empty());
    }
}

#[test]
fn reasoning_rate_is_not_added_when_capability_is_false() {
    assert_no_added_reasoning_rate(Some(false));
}

#[test]
fn reasoning_rate_is_not_added_when_capability_is_unknown() {
    assert_no_added_reasoning_rate(None);
}

#[test]
fn reasoning_billed_as_output_on_current_and_dated_reads() {
    let upstream = json!({"openai":{"id":"openai", "name":"OpenAI", "models":{
        "output-only":{"id":"output-only", "name":"Output only", "reasoning":true, "cost":{"output":10}},
        "priced":{"id":"priced", "name":"Priced", "reasoning":true, "cost":{"output":10, "reasoning":1}},
        "zero":{"id":"zero", "name":"Zero", "reasoning":true, "cost":{"output":10, "reasoning":0}},
        "unpriced":{"id":"unpriced", "name":"Unpriced", "reasoning":true, "cost":{"output":10}}
    }}});
    let (store, _dir) = store_document(&serde_json::to_vec(&upstream).unwrap());
    let unpriced = json!({"state":"unpriced", "reason":"missing_rate"});
    store
        .append_eras(&[fusiform_store::NewEra {
            source: SourceId::ModelsDev,
            provider_id: "openai".into(),
            model_id: "unpriced".into(),
            fact_key: fusiform_store::FactKey::from_stored("rate.reasoning".into()),
            value_json: unpriced.to_string(),
            boundary_at: Timestamp(1_000),
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        }])
        .unwrap();
    let args = json!({"provider_id":"openai", "fact_prefixes":["rate.reasoning"]});
    let value = json!({"state":"billed_as", "key":"rate.output"});
    let check = |read: &CatalogGetResponse| {
        assert_eq!(
            read.models["openai/output-only"].get("rate.reasoning"),
            Some(&value)
        );
        assert_eq!(
            read.models["openai/priced"]["rate.reasoning"]["state"],
            "priced"
        );
        assert_eq!(
            read.models["openai/priced"]["rate.reasoning"]["units"],
            1_000_000_000
        );
        assert_eq!(
            read.models["openai/zero"]["rate.reasoning"],
            json!({"state":"stated_zero"})
        );
        assert_eq!(read.models["openai/unpriced"]["rate.reasoning"], unpriced);
        assert_eq!(read.fact_overrides.len(), 1);
        assert_eq!(read.fact_overrides[0].model, "openai/output-only");
        assert_eq!(read.fact_overrides[0].model_id, "*");
        assert!(read
            .models
            .values()
            .all(|facts| !facts.contains_key("capability.reasoning")));
    };
    check(&get(&store, args.clone()));

    let mut rows = fusiform_module::fact_overrides::overrides().clone();
    let wildcard_key = ("openai".into(), "*".into(), "rate.reasoning".into());
    let mut rule = rows.remove(&wildcard_key).unwrap();
    let boundary = 2_000;
    rule.established_at_ms = boundary;
    rule.review_by_ms = boundary + 60 * 86_400_000;
    let rows = [(wildcard_key, rule)].into_iter().collect();
    fusiform_module::fact_overrides::apply_to_store(&store, &rows, boundary).unwrap();

    let mut dated = args;
    dated["at_ms"] = json!(boundary);
    check(&get(&store, dated.clone()));
    dated["model_id"] = json!("output-only");
    assert_eq!(
        get(&store, dated.clone()).models["openai/output-only"]["rate.reasoning"],
        value
    );
    dated["at_ms"] = json!(boundary - 1);
    assert!(!get(&store, dated).models["openai/output-only"].contains_key("rate.reasoning"));

    let history = json!({"provider_id":"openai", "model_id":"output-only",
        "fact_key":"rate.reasoning", "served":true});
    let read = serve_history(&store, history.to_string().as_bytes()).unwrap();
    assert!(
        read.eras.is_empty(),
        "the reasoning rate was never stored upstream"
    );
    let segments = read.served.unwrap();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].value, None);
    assert_eq!(segments[0].until_ms, Some(boundary));
    assert_eq!(segments[1].from_ms, boundary);
    assert_eq!(segments[1].value.as_ref(), Some(&value));
    assert_eq!(
        segments[1].source,
        Some(fusiform_protocol::HistorySource::Override)
    );
}

const FLASH: &str = "google/gemini-3.1-flash-image";
const FLASH_MODEL: &str = "gemini-3.1-flash-image";

fn flash_document(output: i64) -> Vec<u8> {
    let model = json!({"id":FLASH_MODEL, "name":"Flash Image",
        "cost":{"input":2, "output":output}, "limit":{"context":1000, "output":500}});
    serde_json::to_vec(&json!({"google":{"id":"google", "name":"Google",
        "models":{FLASH_MODEL:model}}}))
    .unwrap()
}

fn flash_store() -> (CatalogStore, tempfile::TempDir) {
    let (store, dir) = store_publishing(json!(32_768));
    ingest(&store, &flash_document(60), 2_000);
    (store, dir)
}

fn flash_overrides() -> fusiform_module::fact_overrides::FactOverrides {
    fusiform_module::fact_overrides::overrides()
        .iter()
        .filter(|((provider, model, _), _)| provider == "google" && model == FLASH_MODEL)
        .map(|(key, row)| (key.clone(), row.clone()))
        .collect()
}

// Preserve the retired text claim as a fixture: historical reads must still
// reproduce the split that was served before upstream corrected its text rate.
fn historical_flash_overrides() -> fusiform_module::fact_overrides::FactOverrides {
    let mut rows = flash_overrides();
    let mut text = rows[&(
        "google".into(),
        FLASH_MODEL.into(),
        "rate.output_image".into(),
    )]
        .clone();
    text.fact_key = "rate.output".into();
    text.value["units"] = json!(3_000_000_000i64);
    text.replaces = json!({"state":"priced", "units":60_000_000_000i64,
        "exponent":9, "currency":"USD"});
    text.reason = "The upstream output rate is the image price. Text and thinking output have a separate $3 per million token price, so using $60 for text overstates its cost.".into();
    text.notes = Some("Only replaces the exact recorded $60/M upstream value. The Antigravity alias inherits the target's split rates; the Vertex row is untouched.".into());
    rows.insert(
        ("google".into(), FLASH_MODEL.into(), "rate.output".into()),
        text,
    );
    rows
}

pub(super) fn historical_flash_store() -> (CatalogStore, tempfile::TempDir) {
    let (store, dir) = store_document(&flash_document(60));
    let rows = historical_flash_overrides();
    let boundary = rows.values().map(|r| r.established_at_ms).max().unwrap();
    fusiform_module::fact_overrides::apply_to_store(&store, &rows, boundary).unwrap();
    (store, dir)
}

#[test]
fn retired_flash_text_override_stays_historical_after_upstream_correction() {
    let (store, _dir) = historical_flash_store();
    let historical = historical_flash_overrides();
    let boundary = historical
        .values()
        .map(|r| r.established_at_ms)
        .max()
        .unwrap();
    let current = flash_overrides();
    assert_eq!(current.len(), 1);
    assert_eq!(
        current.values().next().unwrap().fact_key,
        "rate.output_image"
    );
    let corrected_at = boundary + 100;
    ingest(&store, &flash_document(3), corrected_at);
    assert_eq!(
        fusiform_module::fact_overrides::apply_to_store(&store, &current, corrected_at).unwrap(),
        1
    );
    for (at, disclosures) in [(corrected_at - 1, 2), (corrected_at, 1)] {
        let response = get(
            &store,
            json!({"provider_id":"google", "model_id":FLASH_MODEL, "at_ms":at}),
        );
        assert_eq!(
            response.models[FLASH]["rate.output"]["units"],
            3_000_000_000i64
        );
        assert_eq!(
            response.models[FLASH]["rate.output_image"]["units"],
            60_000_000_000i64
        );
        assert_eq!(disclosed(&response, FLASH).len(), disclosures);
    }
    ingest(&store, &flash_document(60), corrected_at + 100);
    for args in [
        json!({"provider_id":"google", "model_id":FLASH_MODEL}),
        json!({"provider_id":"google", "model_id":FLASH_MODEL, "at_ms":corrected_at + 100}),
    ] {
        let response = get(&store, args);
        assert_eq!(
            response.models[FLASH]["rate.output"]["units"],
            60_000_000_000i64
        );
        assert_eq!(
            disclosed_fact(&response, FLASH, "rate.output_image").len(),
            1
        );
        assert!(disclosed_fact(&response, FLASH, "rate.output").is_empty());
    }
}

#[test]
fn dated_flash_image_overrides_split_text_and_image_after_establishment() {
    let (store, _dir) = flash_store();
    let rows = historical_flash_overrides();
    assert_eq!(rows.len(), 2);
    let boundary = rows.values().map(|r| r.established_at_ms).max().unwrap();
    assert_eq!(
        fusiform_module::fact_overrides::apply_to_store(&store, &rows, boundary + 1).unwrap(),
        2
    );
    let before = get(
        &store,
        json!({"provider_id":"google", "model_id":FLASH_MODEL, "at_ms":boundary - 1}),
    );
    assert_eq!(
        before.models[FLASH]["rate.output"]["units"],
        60_000_000_000i64
    );
    assert!(!before.models[FLASH].contains_key("rate.output_image"));
    assert!(before.fact_overrides.is_empty());
    for response in [
        get(
            &store,
            json!({"provider_id":"google", "model_id":FLASH_MODEL, "at_ms":boundary}),
        ),
        get(
            &store,
            json!({"provider_id":"google", "model_id":FLASH_MODEL, "at_ms":1_794_440_193_000i64}),
        ),
        get(&store, json!({"provider_id":"google", "at_ms":boundary})),
        paged(&store, json!({"at_ms":boundary, "fact_prefixes":["rate."]})),
    ] {
        assert_eq!(
            response.models[FLASH]["rate.output"]["units"],
            3_000_000_000i64
        );
        assert_eq!(
            response.models[FLASH]["rate.output_image"]["units"],
            60_000_000_000i64
        );
        assert_eq!(disclosed(&response, FLASH).len(), 2);
        assert!(response.aliased.is_empty());
        assert!(response.billing_rules.is_empty());
    }
}

fn override_era_count(store: &CatalogStore) -> i64 {
    rusqlite::Connection::open(store.path())
        .unwrap()
        .query_row("SELECT COUNT(*) FROM fact_override_era", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn dated_overrides_diff_claims_not_provenance_and_tombstone_removals() {
    let (store, _dir) = flash_store();
    let mut rows = historical_flash_overrides();
    let boundary = rows.values().map(|r| r.established_at_ms).max().unwrap();
    let apply =
        |rows: &_| fusiform_module::fact_overrides::apply_to_store(&store, rows, boundary + 100);
    assert_eq!(apply(&rows).unwrap(), 2);
    assert_eq!(apply(&rows).unwrap(), 0);
    let original_quote = rows.values().next().unwrap().quote.clone();
    for row in rows.values_mut() {
        row.quote = "new source wording".into();
        row.reason = "new explanation".into();
        row.source_ref = "https://example.com/rechecked".into();
        row.notes = None;
        row.established_at_ms += 10;
        row.review_by_ms += 10;
    }
    assert_eq!(apply(&rows).unwrap(), 0);
    assert_eq!(override_era_count(&store), 2);
    let recorded = store.fact_overrides_at(boundary, None).unwrap();
    assert_eq!(recorded[0].claim.as_ref().unwrap().quote, original_quote);

    let output_key = (
        "google".to_string(),
        FLASH_MODEL.to_string(),
        "rate.output".to_string(),
    );
    rows.get_mut(&output_key).unwrap().value["units"] = json!(4_000_000_000i64);
    assert_eq!(apply(&rows).unwrap(), 1);
    assert_eq!(apply(&rows).unwrap(), 0);
    assert_eq!(override_era_count(&store), 3);
    for (at, units) in [
        (boundary + 99, 3_000_000_000i64),
        (boundary + 100, 4_000_000_000),
    ] {
        let response = get(
            &store,
            json!({"provider_id":"google", "model_id":FLASH_MODEL,"at_ms":at}),
        );
        assert_eq!(response.models[FLASH]["rate.output"]["units"], units);
    }
    rows.remove(&output_key);
    assert_eq!(
        fusiform_module::fact_overrides::apply_to_store(&store, &rows, boundary + 200).unwrap(),
        1
    );
    assert_eq!(
        fusiform_module::fact_overrides::apply_to_store(&store, &rows, boundary + 300).unwrap(),
        0
    );
    assert_eq!(override_era_count(&store), 4);
    let at_removal = store.fact_overrides_at(boundary + 200, None).unwrap();
    assert!(at_removal[0].claim.is_none());
    let before_removal = get(
        &store,
        json!({"provider_id":"google", "model_id":FLASH_MODEL,"at_ms":boundary + 199}),
    );
    assert_eq!(
        before_removal.models[FLASH]["rate.output"]["units"],
        4_000_000_000i64
    );
    assert_eq!(
        disclosed_fact(&before_removal, FLASH, "rate.output").len(),
        1
    );
    let response = get(
        &store,
        json!({"provider_id":"google", "model_id":FLASH_MODEL,"at_ms":boundary + 200}),
    );
    assert_eq!(
        response.models[FLASH]["rate.output"]["units"],
        60_000_000_000i64
    );
    assert_eq!(disclosed(&response, FLASH).len(), 1);
    // Current reads never switch to a stored claim, even after its removal.
    let current = get(
        &store,
        json!({"provider_id":"google", "model_id":FLASH_MODEL}),
    );
    assert_eq!(
        current.models[FLASH]["rate.output"]["units"],
        60_000_000_000i64
    );
    assert_eq!(
        disclosed_fact(&current, FLASH, "rate.output_image").len(),
        1
    );
    assert!(disclosed_fact(&current, FLASH, "rate.output").is_empty());
}

#[test]
fn dated_override_guards_use_upstream_at_the_requested_instant() {
    let (store, _dir) = flash_store();
    let rows = historical_flash_overrides();
    let boundary = rows.values().map(|r| r.established_at_ms).max().unwrap();
    fusiform_module::fact_overrides::apply_to_store(&store, &rows, boundary).unwrap();
    ingest(&store, &flash_document(70), boundary + 100);
    let conn = rusqlite::Connection::open(store.path()).unwrap();
    // Upstream's explicit null is a published fact too; it blocks an addition.
    conn.execute("INSERT INTO era (source, provider_id, model_id, fact_key, value_json,
        boundary_at_ms, boundary_kind) VALUES ('models.dev','google',?1,'rate.output_image','null',?2,'seed')",
        rusqlite::params![FLASH_MODEL, boundary + 100]).unwrap();
    for args in [
        json!({"provider_id":"google", "model_id":FLASH_MODEL,"at_ms":boundary + 100}),
        json!({"at_ms":boundary + 100,"fact_prefixes":["rate.output_image"]}),
    ] {
        let response = get(&store, args);
        assert!(response.fact_overrides.is_empty());
        assert_eq!(response.models[FLASH]["rate.output_image"], Value::Null);
    }
    let moved = get(
        &store,
        json!({"provider_id":"google", "model_id":FLASH_MODEL,"at_ms":boundary + 100}),
    );
    assert_eq!(
        moved.models[FLASH]["rate.output"]["units"],
        70_000_000_000i64
    );
    let earlier = get(
        &store,
        json!({"provider_id":"google", "model_id":FLASH_MODEL,"at_ms":boundary}),
    );
    assert_eq!(
        earlier.models[FLASH]["rate.output"]["units"],
        3_000_000_000i64
    );
    assert_eq!(disclosed(&earlier, FLASH).len(), 2);
    let excluded = get(
        &store,
        json!({"provider_id":"google", "at_ms":boundary,"fact_prefixes":["limit."]}),
    );
    assert!(excluded.fact_overrides.is_empty());
    let only_addition = paged(
        &store,
        json!({"at_ms":boundary,"fact_prefixes":["rate.output_image"]}),
    );
    assert_eq!(disclosed(&only_addition, FLASH).len(), 1);
}

#[test]
fn dated_override_reappearance_and_clock_rollback_use_latest_boundary() {
    let (store, _dir) = flash_store();
    let mut rows = historical_flash_overrides();
    let boundary = rows.values().map(|r| r.established_at_ms).max().unwrap();
    fusiform_module::fact_overrides::apply_to_store(&store, &rows, boundary).unwrap();
    fusiform_module::fact_overrides::apply_to_store(&store, &Default::default(), boundary + 100)
        .unwrap();
    assert_eq!(
        fusiform_module::fact_overrides::apply_to_store(&store, &rows, boundary - 100).unwrap(),
        2
    );
    let eras = store.fact_overrides_at(boundary + 100, None).unwrap();
    assert!(eras
        .iter()
        .all(|e| e.boundary_at_ms == boundary + 100 && e.claim.is_some()));
    assert_eq!(override_era_count(&store), 6);
    assert_eq!(
        fusiform_module::fact_overrides::apply_to_store(&store, &rows, boundary + 200).unwrap(),
        0
    );
    // Changing only the upstream guard is a new claim too.
    rows.values_mut()
        .find(|r| r.fact_key == "rate.output")
        .unwrap()
        .replaces["units"] = json!(70_000_000_000i64);
    assert_eq!(
        fusiform_module::fact_overrides::apply_to_store(&store, &rows, boundary + 200).unwrap(),
        1
    );
    assert_eq!(override_era_count(&store), 7);
    let response = get(
        &store,
        json!({"provider_id":"google", "model_id":FLASH_MODEL, "at_ms":boundary + 200}),
    );
    assert_eq!(
        response.models[FLASH]["rate.output"]["units"],
        60_000_000_000i64
    );
    assert_eq!(disclosed(&response, FLASH).len(), 1);
}

/// Every page of a paged read, merged.
fn paged(store: &CatalogStore, mut args: Value) -> CatalogGetResponse {
    args["paged"] = json!(true);
    let mut merged = get(store, args.clone());
    while let Some(token) = merged.next_page_token.take() {
        args["page_token"] = json!(token);
        let page = get(store, args.clone());
        merged.models.extend(page.models);
        merged.fact_overrides.extend(page.fact_overrides);
        merged.next_page_token = page.next_page_token;
    }
    merged
}

fn disclosed<'a>(
    response: &'a CatalogGetResponse,
    model: &str,
) -> Vec<&'a fusiform_protocol::FactOverrideWire> {
    response
        .fact_overrides
        .iter()
        .filter(|row| row.model == model)
        .collect()
}

fn disclosed_fact<'a>(
    response: &'a CatalogGetResponse,
    model: &str,
    key: &str,
) -> Vec<&'a fusiform_protocol::FactOverrideWire> {
    disclosed(response, model)
        .into_iter()
        .filter(|row| row.fact_key == key)
        .collect()
}

fn assert_overridden(response: &CatalogGetResponse, model: &str, label: &str) {
    assert_eq!(
        response.models[model]["limit.output"],
        json!(65_536),
        "{label}: {model}"
    );
    let rows = disclosed_fact(response, model, "limit.output");
    // An unfiltered Argon read also carries its independent temperature
    // replacement and reasoning additions. Every disclosed row must
    // still correspond to a served fact.
    for row in disclosed(response, model) {
        assert_eq!(response.models[model].get(&row.fact_key), Some(&row.value));
        assert!(matches!(
            row.fact_key.as_str(),
            "limit.output"
                | "capability.temperature"
                | "capability.reasoning_default"
                | "rate.reasoning"
        ));
    }
    assert_eq!(
        rows.len(),
        1,
        "{label}: {model} {:?}",
        response.fact_overrides
    );
    let row = rows[0];
    assert_eq!(
        (row.provider_id.as_str(), row.model_id.as_str()),
        ("google", "gemini-4-argon"),
        "{label}: the disclosure names the row the override is declared for"
    );
    assert_eq!(row.fact_key, "limit.output");
    assert_eq!(row.value, json!(65_536));
    assert_eq!(row.replaces, json!(32_768));
    assert!(row
        .source_ref
        .contains("https://ai.google.dev/gemini-api/docs/generate-content/latest-model"));
    assert!(row
        .quote
        .contains("One streaming request returns up to 65,536 tokens."));
    assert_eq!(row.established_by, "fusiform");
    assert_eq!(row.review_by_ms - row.established_at_ms, 60 * 86_400_000);
}

#[test]
fn a_current_read_serves_the_override_and_discloses_it() {
    let (store, _dir) = store_publishing(json!(32_768));
    for (label, response) in [
        (
            "named",
            get(
                &store,
                json!({"provider_id":"google","model_id":"gemini-4-argon"}),
            ),
        ),
        ("provider", get(&store, json!({"provider_id":"google"}))),
        ("bulk", get(&store, json!({}))),
        (
            "limits",
            get(&store, json!({"fact_prefixes":["limit.output"]})),
        ),
        ("paged", paged(&store, json!({}))),
        (
            "apikey plane",
            get(
                &store,
                json!({"provider_id":"google","model_id":"gemini-4-argon","auth_method":"apikey"}),
            ),
        ),
    ] {
        assert_overridden(&response, TARGET, label);
        // The control model has no output-limit replacement; its separately
        // sourced temperature replacement may still be disclosed.
        assert!(
            disclosed_fact(&response, CONTROL, "limit.output").is_empty(),
            "{label}"
        );
        // Each disclosure describes a value the response actually serves.
        for row in &response.fact_overrides {
            assert_eq!(
                response.models[&row.model][&row.fact_key], row.value,
                "{label}"
            );
        }
    }
}

#[test]
fn the_antigravity_alias_serves_the_overridden_value() {
    let (store, _dir) = store_publishing(json!(32_768));
    for (label, response) in [
        (
            "named alias",
            get(
                &store,
                json!({"provider_id":"google","model_id":"antigravity-gemini-4-argon"}),
            ),
        ),
        (
            "named alias, limits",
            get(
                &store,
                json!({"provider_id":"google","model_id":"antigravity-gemini-4-argon","fact_prefixes":["limit."]}),
            ),
        ),
        ("provider", get(&store, json!({"provider_id":"google"}))),
        ("bulk", get(&store, json!({}))),
        ("paged", paged(&store, json!({}))),
        (
            "apikey plane",
            get(
                &store,
                json!({"provider_id":"google","model_id":"antigravity-gemini-4-argon","auth_method":"apikey"}),
            ),
        ),
    ] {
        assert_overridden(&response, ALIAS, label);
        assert!(
            response.aliased.iter().any(|row| row.model == ALIAS),
            "{label}: the entry is served as an alias"
        );
    }
    // The named alias read carries only the alias entry, so its disclosures
    // are re-keyed rather than naming the target.
    let named = get(
        &store,
        json!({"provider_id":"google","model_id":"antigravity-gemini-4-argon"}),
    );
    assert_eq!(named.fact_overrides.len(), 3);
    assert!(named.fact_overrides.iter().all(|row| row.model == ALIAS));
}

#[test]
fn a_filter_excluding_the_fact_serves_and_discloses_nothing() {
    let (store, _dir) = store_publishing(json!(32_768));
    for args in [
        json!({"fact_prefixes":["rate."]}),
        json!({"provider_id":"google","model_id":"gemini-4-argon","fact_prefixes":["rate."]}),
        json!({"provider_id":"google","model_id":"antigravity-gemini-4-argon","fact_prefixes":["limit.context"]}),
        // The plane view widens its intermediate filter, then narrows to this.
        json!({"provider_id":"google","model_id":"gemini-4-argon","auth_method":"apikey","fact_prefixes":["rate."]}),
    ] {
        let response = get(&store, args.clone());
        assert!(
            response
                .fact_overrides
                .iter()
                .all(|row| row.fact_key == "rate.reasoning"),
            "{args}"
        );
        assert!(
            response
                .models
                .values()
                .all(|facts| !facts.contains_key("limit.output")),
            "{args}"
        );
    }
}

#[test]
fn an_at_ms_read_serves_the_stored_value_without_disclosure() {
    let (store, _dir) = store_publishing(json!(32_768));
    for response in [
        get(
            &store,
            json!({"provider_id":"google","model_id":"gemini-4-argon","at_ms":1_000}),
        ),
        get(&store, json!({"at_ms":1_000})),
        paged(&store, json!({"at_ms":1_000})),
    ] {
        assert_eq!(response.models[TARGET]["limit.output"], json!(32_768));
        assert!(response.fact_overrides.is_empty());
    }
    // Nothing was stored: the fact's history holds only the upstream value.
    let history = serve_history(
        &store,
        &serde_json::to_vec(
            &json!({"provider_id":"google","model_id":"gemini-4-argon","fact_key":"limit.output"}),
        )
        .unwrap(),
    )
    .unwrap();
    let values: Vec<_> = history.eras.iter().map(|era| era.value.clone()).collect();
    assert_eq!(values, [json!(32_768)]);
}

#[test]
fn an_upstream_value_other_than_replaces_is_served_as_published() {
    // Upstream already publishes something else, including the override's own
    // value: the output row does not apply and no output override is disclosed.
    // Select that fact so the independent temperature correction is out of scope.
    for published in [json!(49_152), json!(65_536), json!(1_048_576)] {
        let (store, _dir) = store_publishing(published.clone());
        for args in [
            json!({"provider_id":"google","model_id":"gemini-4-argon","fact_prefixes":["limit.output"]}),
            json!({"provider_id":"google","model_id":"antigravity-gemini-4-argon","fact_prefixes":["limit.output"]}),
            json!({"fact_prefixes":["limit.output"]}),
        ] {
            let response = get(&store, args.clone());
            assert!(response.fact_overrides.is_empty(), "{published} {args}");
            for model in [TARGET, ALIAS] {
                if let Some(facts) = response.models.get(model) {
                    assert_eq!(facts["limit.output"], published, "{model} {args}");
                }
            }
        }
    }

    // Upstream moves after the override shipped: the current read serves the
    // new upstream value, while the past instant still serves what was
    // recorded then, undisclosed either way.
    let (store, _dir) = store_publishing(json!(32_768));
    ingest(&store, &upstream(json!(49_152)), 2_000);
    let now = get(
        &store,
        json!({"provider_id":"google","model_id":"gemini-4-argon","fact_prefixes":["limit.output"]}),
    );
    assert_eq!(now.models[TARGET]["limit.output"], json!(49_152));
    assert!(now.fact_overrides.is_empty());
    let then = get(
        &store,
        json!({"provider_id":"google","model_id":"gemini-4-argon","at_ms":1_500}),
    );
    assert_eq!(then.models[TARGET]["limit.output"], json!(32_768));
    assert!(then.fact_overrides.is_empty());
}

const FALLBACK: &str = "capability.server_side_fallback";
const OPUS: &str = "anthropic/claude-opus-5-5";
const HAIKU: &str = "anthropic/claude-haiku-5-5";
const SONNET: &str = "anthropic/claude-sonnet-5-5";
const OPUS_ALIAS: &str = "google/antigravity-claude-opus-5-5";
const MID_SYSTEM: &str = "capability.mid_conversation_system";

fn anthropic_store(planted: Option<Value>) -> (CatalogStore, tempfile::TempDir) {
    anthropic_store_with_capability("server_side_fallback", planted)
}

/// Seeded routes for testing Claude API additions and their route boundaries.
/// `planted` records a capability for Opus 5.5 as if upstream had published it.
fn anthropic_store_with_capability(
    capability: &str,
    planted: Option<Value>,
) -> (CatalogStore, tempfile::TempDir) {
    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    let mut upstream = json!({});
    for (provider, ids) in [
        (
            "anthropic",
            vec![
                "claude-opus-5-5",
                "claude-haiku-5-5",
                "claude-sonnet-5-5",
                "claude-sonnet-5",
                "claude-opus-4-7",
                "claude-sonnet-4-5",
            ],
        ),
        // Only so the Antigravity alias of Opus 5.5 has its provider.
        ("google", vec!["gemini-3.8-flash"]),
        ("opencode-go", vec!["claude-haiku-5-5"]),
        ("amazon-bedrock", vec!["anthropic.claude-fable-5"]),
        ("google-vertex-anthropic", vec!["claude-sonnet-5@default"]),
    ] {
        upstream[provider] = seed[provider].clone();
        upstream[provider]["models"] = json!({});
        for id in ids {
            let row = seed[provider]["models"][id].clone();
            assert!(row.is_object(), "the seed must carry {provider}/{id}");
            upstream[provider]["models"][id] = row;
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1_000),
            outcome: ObservationOutcome::Changed {
                snapshot_seq: 1_000,
            },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let catalog = normalize_models_dev(&serde_json::to_vec(&upstream).unwrap())
        .unwrap()
        .catalog;
    let mut plan =
        plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    assert!(
        plan.eras
            .iter()
            .all(|era| !fusiform_module::fact_overrides::ADDED_FACTS
                .contains(&era.fact_key.as_str())),
        "the normalizer now produces an addition-only fact; the addition rows need review"
    );
    if let Some(value) = planted {
        plan.eras.push(fusiform_store::NewEra {
            source: SourceId::ModelsDev,
            provider_id: "anthropic".into(),
            model_id: "claude-opus-5-5".into(),
            fact_key: fusiform_store::FactKey::capability(capability),
            value_json: value.to_string(),
            boundary_at: Timestamp(1_000),
            boundary_kind: BoundaryKind::Seed,
            observation_id: None,
        });
    }
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

#[test]
fn a_current_read_serves_the_documented_server_side_fallback() {
    let (store, _dir) = anthropic_store(None);
    for (label, response) in [
        (
            "named",
            get(
                &store,
                json!({"provider_id":"anthropic","model_id":"claude-opus-5-5"}),
            ),
        ),
        ("provider", get(&store, json!({"provider_id":"anthropic"}))),
        ("bulk", get(&store, json!({}))),
        ("paged", paged(&store, json!({}))),
        // The store holds no fact under this key, so a read filtered to it
        // must still reach the rows an addition applies to.
        (
            "filtered bulk",
            get(&store, json!({"fact_prefixes":[FALLBACK]})),
        ),
        (
            "filtered named",
            get(
                &store,
                json!({"provider_id":"anthropic","model_id":"claude-opus-5-5","fact_prefixes":[FALLBACK]}),
            ),
        ),
    ] {
        assert_eq!(response.models[OPUS][FALLBACK], json!(true), "{label}");
        let rows = disclosed_fact(&response, OPUS, FALLBACK);
        assert_eq!(rows.len(), 1, "{label}: {:?}", response.fact_overrides);
        let row = rows[0];
        assert_eq!(row.fact_key, FALLBACK, "{label}");
        assert_eq!(row.value, json!(true), "{label}");
        assert_eq!(
            row.replaces,
            Value::Null,
            "{label}: an addition replaces nothing"
        );
        assert!(row.source_ref.contains(
            "https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback"
        ));
        assert!(row.quote.contains("server-side-fallback-2026-07-01"));
        assert_eq!(row.review_by_ms - row.established_at_ms, 60 * 86_400_000);
        // The disclosure serializes `replaces` as an explicit null.
        let wire = serde_json::to_value(row).unwrap();
        assert!(
            wire.as_object().unwrap().contains_key("replaces"),
            "{label}"
        );
        assert_eq!(wire["replaces"], Value::Null, "{label}");

        // Named reads carry only Opus 5.5; every other read carries Haiku 5.5.
        if !label.starts_with("named") && !label.starts_with("filtered named") {
            assert_eq!(response.models[HAIKU][FALLBACK], json!(false), "{label}");
            assert_eq!(
                disclosed_fact(&response, HAIKU, FALLBACK).len(),
                1,
                "{label}"
            );
        }
        // Sonnet 5.5 has no fallback row, so no fallback value or disclosure.
        if let Some(facts) = response.models.get(SONNET) {
            assert!(!facts.contains_key(FALLBACK), "{label}");
        }
        assert!(
            disclosed_fact(&response, SONNET, FALLBACK).is_empty(),
            "{label}"
        );
        for row in &response.fact_overrides {
            assert_eq!(
                response.models[&row.model][&row.fact_key], row.value,
                "{label}"
            );
        }
    }
    // A filtered read serves only the key it asked for.
    let filtered = get(&store, json!({"fact_prefixes":[FALLBACK]}));
    assert!(filtered
        .models
        .values()
        .all(|facts| facts.keys().all(|key| key == FALLBACK)));
}

#[test]
fn a_haiku_read_serves_false_with_the_exclusion_quoted() {
    let (store, _dir) = anthropic_store(None);
    let response = get(
        &store,
        json!({"provider_id":"anthropic","model_id":"claude-haiku-5-5"}),
    );
    assert_eq!(response.models[HAIKU][FALLBACK], json!(false));
    let rows = disclosed_fact(&response, HAIKU, FALLBACK);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].replaces.is_null());
    assert!(rows[0]
        .quote
        .contains("Claude Haiku 5.5 has no server-side fallback"));
}

#[test]
fn an_at_ms_read_serves_no_server_side_fallback() {
    let (store, _dir) = anthropic_store(None);
    for response in [
        get(
            &store,
            json!({"provider_id":"anthropic","model_id":"claude-opus-5-5","at_ms":1_000}),
        ),
        get(&store, json!({"at_ms":1_000})),
        paged(&store, json!({"at_ms":1_000})),
    ] {
        for model in [OPUS, HAIKU] {
            if let Some(facts) = response.models.get(model) {
                assert!(!facts.contains_key(FALLBACK), "{model}");
                assert!(facts.contains_key("limit.context"), "{model} is served");
            }
        }
        assert!(response.models.contains_key(OPUS));
        assert!(response.fact_overrides.is_empty());
    }
}

#[test]
fn an_upstream_value_for_the_key_suppresses_the_addition() {
    // Any stored value wins, including one that disagrees with the row and a
    // null, which is upstream recording the key with no value.
    for published in [json!(false), json!(true), Value::Null] {
        let (store, _dir) = anthropic_store(Some(published.clone()));
        for args in [
            json!({"provider_id":"anthropic","model_id":"claude-opus-5-5"}),
            json!({"provider_id":"anthropic"}),
            json!({"fact_prefixes":[FALLBACK]}),
        ] {
            let response = get(&store, args.clone());
            assert_eq!(response.models[OPUS][FALLBACK], published, "{args}");
            assert!(
                disclosed_fact(&response, OPUS, FALLBACK).is_empty(),
                "{published} {args}"
            );
            // Rows upstream still says nothing about keep their addition.
            if let Some(facts) = response.models.get(HAIKU) {
                assert_eq!(facts[FALLBACK], json!(false), "{args}");
            }
        }
    }
}

#[test]
fn the_antigravity_route_to_opus_does_not_carry_server_side_fallback() {
    // The route reaches Opus 5.5 through Google-managed Vertex, which the docs
    // do not list among the platforms with server-side fallback.
    let (store, _dir) = anthropic_store(None);
    for args in [
        json!({"provider_id":"google","model_id":"antigravity-claude-opus-5-5"}),
        json!({"provider_id":"google"}),
        json!({}),
    ] {
        let response = get(&store, args.clone());
        let facts = &response.models[OPUS_ALIAS];
        assert!(facts.contains_key("limit.context"), "{args}");
        assert!(!facts.contains_key(FALLBACK), "{args}");
        assert!(
            disclosed_fact(&response, OPUS_ALIAS, FALLBACK).is_empty(),
            "{args}"
        );
    }
}

#[test]
fn a_current_read_serves_the_documented_mid_conversation_system() {
    let (store, _dir) = anthropic_store_with_capability("mid_conversation_system", None);
    for response in [
        get(
            &store,
            json!({"provider_id":"anthropic","model_id":"claude-opus-5-5"}),
        ),
        get(&store, json!({"provider_id":"anthropic"})),
        get(&store, json!({})),
        paged(&store, json!({})),
        get(&store, json!({"fact_prefixes":[MID_SYSTEM]})),
        get(
            &store,
            json!({"provider_id":"anthropic","model_id":"claude-opus-5-5","fact_prefixes":[MID_SYSTEM]}),
        ),
    ] {
        assert_eq!(response.models[OPUS][MID_SYSTEM], json!(true));
        let rows = disclosed_fact(&response, OPUS, MID_SYSTEM);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, json!(true));
        assert!(rows[0].replaces.is_null());
        assert!(rows[0]
            .source_ref
            .contains("/mid-conversation-system-messages"));
        assert!(rows[0]
            .quote
            .contains("No beta header is required for mid-conversation system messages."));
        assert_eq!(
            rows[0].review_by_ms - rows[0].established_at_ms,
            60 * 86_400_000
        );
    }
    for (model_id, sentence) in [
        (
            "claude-sonnet-5",
            "This feature is not available on Claude Sonnet 5.",
        ),
        (
            "claude-opus-4-7",
            "Claude Opus 4.7 rejects `role: \"system\"` in `messages` with a 400 error.",
        ),
    ] {
        let response = get(
            &store,
            json!({"provider_id":"anthropic","model_id":model_id}),
        );
        let model = format!("anthropic/{model_id}");
        assert_eq!(response.models[&model][MID_SYSTEM], json!(false));
        let rows = disclosed_fact(&response, &model, MID_SYSTEM);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, json!(false));
        assert!(rows[0].quote.contains(sentence));
    }
    let filtered = get(&store, json!({"fact_prefixes":[MID_SYSTEM]}));
    assert!(filtered
        .models
        .values()
        .all(|facts| facts.keys().all(|key| key == MID_SYSTEM)));
    let excluded = get(
        &store,
        json!({"provider_id":"anthropic","fact_prefixes":["limit."]}),
    );
    assert!(excluded
        .models
        .values()
        .all(|facts| !facts.contains_key(MID_SYSTEM)));
    assert!(excluded.fact_overrides.is_empty());
}

#[test]
fn mid_conversation_system_is_not_projected_onto_other_routes() {
    let (store, _dir) = anthropic_store_with_capability("mid_conversation_system", None);
    for (provider, model_id) in [
        ("anthropic", "claude-sonnet-4-5"),
        ("opencode-go", "claude-haiku-5-5"),
        ("amazon-bedrock", "anthropic.claude-fable-5"),
        ("google-vertex-anthropic", "claude-sonnet-5@default"),
        ("google", "antigravity-claude-opus-5-5"),
    ] {
        for response in [
            get(&store, json!({"provider_id":provider,"model_id":model_id})),
            get(&store, json!({"provider_id":provider})),
            get(&store, json!({})),
        ] {
            let model = format!("{provider}/{model_id}");
            let facts = &response.models[&model];
            assert!(facts.contains_key("limit.context"), "{model} is served");
            assert!(!facts.contains_key(MID_SYSTEM), "{model}");
            assert!(
                disclosed_fact(&response, &model, MID_SYSTEM).is_empty(),
                "{model}"
            );
        }
    }
}

#[test]
fn mid_conversation_system_before_first_era_and_upstream_wins() {
    for published in [
        None,
        Some(json!(false)),
        Some(json!(true)),
        Some(Value::Null),
    ] {
        let (store, _dir) =
            anthropic_store_with_capability("mid_conversation_system", published.clone());
        for args in [
            json!({"provider_id":"anthropic","model_id":"claude-opus-5-5"}),
            json!({"provider_id":"anthropic"}),
            json!({"fact_prefixes":[MID_SYSTEM]}),
        ] {
            let response = get(&store, args);
            if let Some(value) = &published {
                assert_eq!(&response.models[OPUS][MID_SYSTEM], value);
                assert!(disclosed_fact(&response, OPUS, MID_SYSTEM).is_empty());
            } else {
                assert_eq!(response.models[OPUS][MID_SYSTEM], json!(true));
            }
        }
        for response in [
            get(
                &store,
                json!({"provider_id":"anthropic","model_id":"claude-opus-5-5","at_ms":1_000}),
            ),
            get(&store, json!({"at_ms":1_000})),
            paged(&store, json!({"at_ms":1_000})),
        ] {
            assert_eq!(response.models[OPUS].get(MID_SYSTEM), published.as_ref());
            assert!(response.fact_overrides.is_empty());
        }
    }
}

const STATUS: &str = "model.status";
const DAYBREAK_BLUE: &str = "openai/gpt-daybreak-blue-latest";
const DAYBREAK_RED: &str = "openai/gpt-daybreak-red-latest";
// A seed row with no status and no override, so the addition is shown to
// reach only the rows it names.
const DAYBREAK_CONTROL: &str = "openai/gpt-5.6-sol";

/// A store holding the two Daybreak aliases plus a control row, as the seed
/// publishes them. Ingest stores `model.status` on every row, as `null` when
/// models.dev publishes none. `planted` replaces Daybreak Blue's stored status
/// with one models.dev would have published.
fn daybreak_store(planted: Option<Value>) -> (CatalogStore, tempfile::TempDir) {
    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    let mut upstream = json!({});
    upstream["openai"] = seed["openai"].clone();
    upstream["openai"]["models"] = json!({});
    for id in [
        "gpt-daybreak-blue-latest",
        "gpt-daybreak-red-latest",
        "gpt-5.6-sol",
    ] {
        let row = seed["openai"]["models"][id].clone();
        assert!(row.is_object(), "the seed must carry openai/{id}");
        upstream["openai"]["models"][id] = row;
    }
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1_000),
            outcome: ObservationOutcome::Changed {
                snapshot_seq: 1_000,
            },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let catalog = normalize_models_dev(&serde_json::to_vec(&upstream).unwrap())
        .unwrap()
        .catalog;
    let mut plan =
        plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    for era in &mut plan.eras {
        if era.fact_key.as_str() == STATUS && era.model_id.starts_with("gpt-daybreak-") {
            assert_eq!(
                era.value_json, "null",
                "models.dev now publishes a status for {}; review the deprecation rows",
                era.model_id
            );
            if let (Some(value), "gpt-daybreak-blue-latest") = (&planted, era.model_id.as_str()) {
                era.value_json = value.to_string();
            }
        }
    }
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

#[test]
fn a_current_read_marks_the_vendor_deprecated_daybreak_aliases() {
    let (store, _dir) = daybreak_store(None);
    for (label, response) in [
        (
            "named",
            get(
                &store,
                json!({"provider_id":"openai","model_id":"gpt-daybreak-blue-latest"}),
            ),
        ),
        ("provider", get(&store, json!({"provider_id":"openai"}))),
        ("paged", paged(&store, json!({}))),
        // The store holds no status for these rows, so a read filtered to the
        // key must still reach them.
        ("filtered", get(&store, json!({"fact_prefixes":[STATUS]}))),
    ] {
        assert_eq!(
            response.models[DAYBREAK_BLUE][STATUS],
            json!("deprecated"),
            "{label}"
        );
        let rows = disclosed_fact(&response, DAYBREAK_BLUE, STATUS);
        assert_eq!(rows.len(), 1, "{label}: {:?}", response.fact_overrides);
        assert_eq!(rows[0].fact_key, STATUS, "{label}");
        assert_eq!(
            rows[0].replaces,
            Value::Null,
            "{label}: it replaces the stored null"
        );
        assert!(
            rows[0]
                .source_ref
                .contains("https://platform.openai.com/docs/models/gpt-daybreak-blue-latest"),
            "{label}"
        );
        if label != "named" {
            assert_eq!(
                response.models[DAYBREAK_RED][STATUS],
                json!("deprecated"),
                "{label}"
            );
        }
        // The control row has no override, so it keeps its stored null.
        if let Some(facts) = response.models.get(DAYBREAK_CONTROL) {
            assert_eq!(facts.get(STATUS), Some(&Value::Null), "{label}: {facts:?}");
        }
    }
}

#[test]
fn an_at_ms_read_serves_the_recorded_null_status() {
    let (store, _dir) = daybreak_store(None);
    let response = get(
        &store,
        json!({"provider_id":"openai","model_id":"gpt-daybreak-blue-latest","at_ms":1_000}),
    );
    assert_eq!(response.models[DAYBREAK_BLUE][STATUS], Value::Null);
    assert!(response.fact_overrides.is_empty());
}

#[test]
fn a_status_models_dev_publishes_wins_over_the_deprecation_row() {
    // Any published status stands, including one other than deprecated.
    for published in [json!("beta"), json!("deprecated")] {
        let (store, _dir) = daybreak_store(Some(published.clone()));
        let response = get(&store, json!({"provider_id":"openai"}));
        assert_eq!(response.models[DAYBREAK_BLUE][STATUS], published);
        assert!(
            disclosed_fact(&response, DAYBREAK_BLUE, STATUS).is_empty(),
            "{published}"
        );
        // Red still has no published status, so it keeps its addition.
        assert_eq!(response.models[DAYBREAK_RED][STATUS], json!("deprecated"));
    }
}

#[test]
fn carbon_and_neon_antigravity_aliases_serve_target_output_limits() {
    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    let carbon = seed["google"]["models"]["gemini-4-carbon"].clone();
    assert_eq!(
        carbon["limit"]["output"],
        json!(32_768),
        "the embedded seed must publish the value the override replaces"
    );
    let mut google = seed["google"].clone();
    google["models"] = json!({
        "gemini-4-carbon": carbon,
        "gemini-4-argon": seed["google"]["models"]["gemini-4-argon"],
        "gemini-4-neon": seed["google"]["models"]["gemini-4-neon"]
    });
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    ingest(
        &store,
        &serde_json::to_vec(&json!({ "google": google })).unwrap(),
        1_000,
    );

    let get_named = |model| get(&store, json!({"provider_id":"google","model_id":model}));
    let outputs = ["antigravity-gemini-4-carbon", "antigravity-gemini-4-neon"]
        .map(|model| get_named(model).models[&format!("google/{model}")]["limit.output"].clone());
    assert_eq!(outputs, [json!(65_536), json!(65_536)]);

    for (model, source_model) in [
        ("gemini-4-carbon", Some("gemini-4-carbon")),
        ("antigravity-gemini-4-carbon", Some("gemini-4-carbon")),
        ("antigravity-gemini-4-argon", Some("gemini-4-argon")),
        ("antigravity-gemini-4-neon", None),
    ] {
        let identity = format!("google/{model}");
        for (label, response) in [
            ("named", get_named(model)),
            ("provider", get(&store, json!({"provider_id":"google"}))),
            ("paged", paged(&store, json!({}))),
            (
                "filtered",
                get(
                    &store,
                    json!({"provider_id":"google","model_id":model,"fact_prefixes":["limit."]}),
                ),
            ),
        ] {
            assert_eq!(
                response.models[&identity]["limit.output"],
                json!(65_536),
                "{identity}: {label}"
            );
            let rows = disclosed_fact(&response, &identity, "limit.output");
            assert_eq!(
                rows.len(),
                usize::from(source_model.is_some()),
                "{identity}: {label}: {:?}",
                response.fact_overrides
            );
            if let Some(source_model) = source_model {
                let row = rows[0];
                assert_eq!(row.provider_id, "google");
                assert_eq!(row.model_id, source_model);
                assert_eq!(row.value, json!(65_536));
                if source_model == "gemini-4-carbon" {
                    assert_eq!(row.replaces, json!(32_768));
                    assert!(row
                        .source_ref
                        .contains("https://ai.google.dev/gemini-api/docs/models/gemini-4-carbon"));
                }
            }
        }
    }
    let historical = get(&store, json!({"provider_id":"google","at_ms":1_000}));
    assert!(!historical
        .models
        .contains_key("google/antigravity-gemini-4-carbon"));
    assert_eq!(
        historical.models["google/gemini-4-carbon"]["limit.output"],
        json!(32_768)
    );
    assert!(historical.fact_overrides.is_empty());
}

#[test]
fn hydrogen_antigravity_alias_serves_the_targets_output_limit() {
    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    let hydrogen = seed["google"]["models"]["gemini-4-hydrogen"].clone();
    assert_eq!(hydrogen["limit"]["output"], json!(65_536));
    let mut google = seed["google"].clone();
    google["models"] = json!({"gemini-4-hydrogen": hydrogen});
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    ingest(
        &store,
        &serde_json::to_vec(&json!({"google": google})).unwrap(),
        1_000,
    );
    for response in [
        get(
            &store,
            json!({"provider_id":"google","model_id":"antigravity-gemini-4-hydrogen"}),
        ),
        get(&store, json!({"provider_id":"google"})),
        paged(&store, json!({})),
        get(
            &store,
            json!({"provider_id":"google","model_id":"antigravity-gemini-4-hydrogen","fact_prefixes":["limit."]}),
        ),
    ] {
        let identity = "google/antigravity-gemini-4-hydrogen";
        assert_eq!(response.models[identity]["limit.output"], json!(65_536));
        assert!(disclosed_fact(&response, identity, "limit.output").is_empty());
    }
    let target = get(
        &store,
        json!({"provider_id":"google","model_id":"gemini-4-hydrogen"}),
    );
    assert_eq!(
        target.models["google/gemini-4-hydrogen"]["limit.output"],
        json!(65_536)
    );
}

const REASONING_DEFAULT: &str = "capability.reasoning_default";
const REASONING_DEFAULT_ESTABLISHED: i64 = 1_794_960_000_000;
const GPT_55: &str = "openai/gpt-5.5";
const GPT_62_SOL: &str = "openai/gpt-6.2-sol";
const AZURE_GPT_55: &str = "azure/gpt-5.5";

/// Seeded rows for the reasoning default: two documented first-party rows,
/// one OpenAI row whose vendor documents no default, a reseller row sharing
/// gpt-5.5's canonical id under the same model id, and the Antigravity alias
/// of Opus 5.5 (its google provider comes from Gemini 3.8 Flash).
fn reasoning_default_store() -> (CatalogStore, tempfile::TempDir) {
    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    let mut upstream = json!({});
    for (provider, ids) in [
        ("openai", vec!["gpt-5.5", "gpt-6.2-sol"]),
        ("azure", vec!["gpt-5.5"]),
        ("anthropic", vec!["claude-opus-5-5"]),
        ("google", vec!["gemini-3.8-flash"]),
    ] {
        upstream[provider] = seed[provider].clone();
        upstream[provider]["models"] = json!({});
        for id in ids {
            let row = seed[provider]["models"][id].clone();
            assert!(row.is_object(), "the seed must carry {provider}/{id}");
            upstream[provider]["models"][id] = row;
        }
    }
    store_document(&serde_json::to_vec(&upstream).unwrap())
}

#[test]
fn a_current_read_serves_the_documented_reasoning_default() {
    let (store, _dir) = reasoning_default_store();
    for (label, response) in [
        (
            "named",
            get(&store, json!({"provider_id":"openai","model_id":"gpt-5.5"})),
        ),
        ("provider", get(&store, json!({"provider_id":"openai"}))),
        ("bulk", get(&store, json!({}))),
        ("paged", paged(&store, json!({}))),
        // The options the default is checked against are not requested, so
        // the read must fetch them anyway and still serve only the default.
        (
            "filtered bulk",
            get(&store, json!({"fact_prefixes":[REASONING_DEFAULT]})),
        ),
        (
            "filtered named",
            get(
                &store,
                json!({"provider_id":"openai","model_id":"gpt-5.5","fact_prefixes":[REASONING_DEFAULT]}),
            ),
        ),
    ] {
        assert_eq!(
            response.models[GPT_55][REASONING_DEFAULT],
            json!({"type":"effort","value":"medium"}),
            "{label}"
        );
        let rows = disclosed_fact(&response, GPT_55, REASONING_DEFAULT);
        assert_eq!(rows.len(), 1, "{label}: {:?}", response.fact_overrides);
        assert!(rows[0].replaces.is_null(), "{label}");
        assert!(
            rows[0]
                .source_ref
                .contains("https://developers.openai.com/api/docs/models/gpt-5.5"),
            "{label}"
        );
        assert!(rows[0].quote.contains("medium (default)"), "{label}");
        assert_eq!(
            rows[0].review_by_ms - rows[0].established_at_ms,
            60 * 86_400_000
        );
        if label.starts_with("filtered") {
            assert!(
                response
                    .models
                    .values()
                    .all(|facts| facts.keys().all(|key| key == REASONING_DEFAULT)),
                "{label}"
            );
        }
        if !label.contains("named") && label != "provider" {
            assert_eq!(
                response.models["anthropic/claude-opus-5-5"][REASONING_DEFAULT],
                json!({"type":"effort","value":"medium"}),
                "{label}"
            );
        }
        // OpenAI documents no default for GPT-6.2 Sol: no fact, no disclosure.
        if let Some(facts) = response.models.get(GPT_62_SOL) {
            assert!(!facts.contains_key(REASONING_DEFAULT), "{label}");
        }
        assert!(
            disclosed_fact(&response, GPT_62_SOL, REASONING_DEFAULT).is_empty(),
            "{label}"
        );
    }
    let sol = get(
        &store,
        json!({"provider_id":"openai","model_id":"gpt-6.2-sol"}),
    );
    assert!(sol.models[GPT_62_SOL].contains_key("capability.reasoning_options"));
    assert!(!sol.models[GPT_62_SOL].contains_key(REASONING_DEFAULT));
}

#[test]
fn the_reasoning_default_reaches_neither_resellers_nor_aliases() {
    let (store, _dir) = reasoning_default_store();
    for (provider, model_id) in [
        ("azure", "gpt-5.5"),
        ("google", "antigravity-claude-opus-5-5"),
    ] {
        for response in [
            get(&store, json!({"provider_id":provider,"model_id":model_id})),
            get(&store, json!({"provider_id":provider})),
            get(&store, json!({})),
        ] {
            let model = format!("{provider}/{model_id}");
            let facts = &response.models[&model];
            assert!(facts.contains_key("limit.context"), "{model} is served");
            assert!(!facts.contains_key(REASONING_DEFAULT), "{model}");
            assert!(
                disclosed_fact(&response, &model, REASONING_DEFAULT).is_empty(),
                "{model}"
            );
        }
    }
    let azure = get(&store, json!({"provider_id":"azure","model_id":"gpt-5.5"}));
    assert_eq!(
        azure.models[AZURE_GPT_55]["model.canonical_id"],
        json!(GPT_55)
    );
}

#[test]
fn an_at_ms_read_serves_the_reasoning_default_only_from_its_recorded_establishment() {
    let (store, _dir) = reasoning_default_store();
    // Before any boot records the claim, no dated read serves it.
    for at in [1_000, REASONING_DEFAULT_ESTABLISHED] {
        let response = get(
            &store,
            json!({"provider_id":"openai","model_id":"gpt-5.5","at_ms":at}),
        );
        assert!(response.models[GPT_55].contains_key("limit.context"));
        assert!(
            !response.models[GPT_55].contains_key(REASONING_DEFAULT),
            "{at}"
        );
        assert!(response.fact_overrides.is_empty(), "{at}");
    }
    fusiform_module::fact_overrides::apply_to_store(
        &store,
        fusiform_module::fact_overrides::overrides(),
        REASONING_DEFAULT_ESTABLISHED + 1,
    )
    .unwrap();
    for at in [1_000, REASONING_DEFAULT_ESTABLISHED - 1] {
        for response in [
            get(
                &store,
                json!({"provider_id":"openai","model_id":"gpt-5.5","at_ms":at}),
            ),
            get(&store, json!({"at_ms":at})),
            paged(&store, json!({"at_ms":at})),
        ] {
            assert!(response.models[GPT_55].contains_key("limit.context"));
            assert!(
                !response.models[GPT_55].contains_key(REASONING_DEFAULT),
                "{at}"
            );
            assert!(
                disclosed_fact(&response, GPT_55, REASONING_DEFAULT).is_empty(),
                "{at}"
            );
        }
    }
    for at in [
        REASONING_DEFAULT_ESTABLISHED,
        REASONING_DEFAULT_ESTABLISHED + 1,
    ] {
        for response in [
            get(
                &store,
                json!({"provider_id":"openai","model_id":"gpt-5.5","at_ms":at}),
            ),
            get(
                &store,
                json!({"at_ms":at, "fact_prefixes":[REASONING_DEFAULT]}),
            ),
        ] {
            assert_eq!(
                response.models[GPT_55][REASONING_DEFAULT],
                json!({"type":"effort","value":"medium"}),
                "{at}"
            );
            assert_eq!(
                disclosed_fact(&response, GPT_55, REASONING_DEFAULT).len(),
                1
            );
            if let Some(facts) = response.models.get(GPT_62_SOL) {
                assert!(!facts.contains_key(REASONING_DEFAULT), "{at}");
            }
        }
    }
}

const ALLOWED_FALLBACKS: &str = "capability.allowed_fallback_models";
const BIO_ROUTE: &str = "route.refusal_fallback.bio";
const CYBER_ROUTE: &str = "route.refusal_fallback.cyber";

/// Seeded Claude rows for the refusal-fallback additions, with one reseller
/// copy of Haiku 5.5 and the Antigravity alias of Opus 5.5 as boundaries.
fn refusal_fallback_store() -> (CatalogStore, tempfile::TempDir) {
    let seed: Value =
        serde_json::from_str(include_str!("../../data/models-dev-seed.json")).unwrap();
    let mut upstream = json!({});
    for (provider, ids) in [
        (
            "anthropic",
            vec![
                "claude-fable-5",
                "claude-fable-5-1",
                "claude-opus-5",
                "claude-opus-5-5",
                "claude-haiku-5-5",
                // Undated id: the vendor's capture names only the dated one.
                "claude-opus-4-5",
            ],
        ),
        ("google", vec!["gemini-3.8-flash"]),
        ("opencode-go", vec!["claude-haiku-5-5"]),
    ] {
        upstream[provider] = seed[provider].clone();
        upstream[provider]["models"] = json!({});
        for id in ids {
            let row = seed[provider]["models"][id].clone();
            assert!(row.is_object(), "the seed must carry {provider}/{id}");
            upstream[provider]["models"][id] = row;
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(1_000),
            outcome: ObservationOutcome::Changed {
                snapshot_seq: 1_000,
            },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let catalog = normalize_models_dev(&serde_json::to_vec(&upstream).unwrap())
        .unwrap()
        .catalog;
    let plan = plan_ingest(&store, &catalog, Timestamp(1_000), BoundaryKind::Seed, None).unwrap();
    store.append_eras(&plan.eras).unwrap();
    (store, dir)
}

#[test]
fn refusal_fallback_facts_are_served_as_anthropic_publishes_them() {
    let (store, _dir) = refusal_fallback_store();
    let plain = get(&store, json!({}));
    let oauth = get(
        &store,
        json!({"provider_id":"anthropic","auth_method":"oauth"}),
    );
    let filtered = get(
        &store,
        json!({"fact_prefixes":[ALLOWED_FALLBACKS, "route.refusal_fallback."]}),
    );
    for (label, response) in [
        ("plain", &plain),
        ("oauth plane", &oauth),
        ("filtered", &filtered),
    ] {
        let row = |id: &str| {
            response
                .models
                .get(&format!("anthropic/{id}"))
                .unwrap_or_else(|| panic!("{label}: no anthropic/{id} row"))
        };
        assert_eq!(
            row("claude-opus-5-5")[ALLOWED_FALLBACKS],
            json!(["claude-opus-4-8", "claude-opus-5"]),
            "{label}: the vendor's order is kept"
        );
        assert_eq!(
            row("claude-opus-5")[ALLOWED_FALLBACKS],
            json!(["claude-opus-4-8"]),
            "{label}"
        );
        // An empty list is the vendor saying no target is permitted, so it is
        // served, not dropped as if it were unknown.
        assert_eq!(
            row("claude-haiku-5-5").get(ALLOWED_FALLBACKS),
            Some(&json!([])),
            "{label}"
        );
        assert_eq!(
            row("claude-fable-5")[BIO_ROUTE],
            json!("claude-opus-5"),
            "{label}"
        );
        assert_eq!(
            row("claude-opus-5")[CYBER_ROUTE],
            json!("claude-opus-4-8"),
            "{label}"
        );
        // The articles name Fable 5 only.
        assert!(!row("claude-fable-5-1").contains_key(BIO_ROUTE), "{label}");
        assert!(!row("claude-fable-5").contains_key(CYBER_ROUTE), "{label}");
        // The capture names only the dated 4.5 id; the undated one is not
        // guessed. A filtered read drops the row entirely, having no fact.
        assert!(
            response
                .models
                .get("anthropic/claude-opus-4-5")
                .is_none_or(|facts| !facts.contains_key(ALLOWED_FALLBACKS)),
            "{label}"
        );
        let routes = disclosed_fact(response, "anthropic/claude-fable-5", BIO_ROUTE);
        assert_eq!(routes.len(), 1, "{label}: {:?}", response.fact_overrides);
        assert!(routes[0]
            .source_ref
            .contains("https://www.anthropic.com/news/claude-opus-5"));
        assert_eq!(routes[0].replaces, Value::Null, "{label}");
    }
    // A reseller copy and the Antigravity alias reach the model another way.
    for other in ["opencode-go/claude-haiku-5-5", OPUS_ALIAS] {
        let facts = &plain.models[other];
        assert!(!facts.contains_key(ALLOWED_FALLBACKS), "{other}");
        assert!(
            !facts
                .keys()
                .any(|key| key.starts_with("route.refusal_fallback.")),
            "{other}"
        );
    }
}

const EFFORT_CACHE: &str = "capability.effort_change_cache";

#[test]
fn effort_change_cache_follows_the_vendor_documentation() {
    let (store, _dir) = refusal_fallback_store();
    let plain = get(&store, json!({}));
    let filtered = get(&store, json!({"fact_prefixes":[EFFORT_CACHE]}));
    for (label, response) in [("plain", &plain), ("filtered", &filtered)] {
        let fact = |id: &str| {
            response
                .models
                .get(&format!("anthropic/{id}"))
                .and_then(|facts| facts.get(EFFORT_CACHE))
        };
        // Anthropic's per-message effort page lists these models.
        for id in [
            "claude-fable-5-1",
            "claude-opus-5",
            "claude-opus-5-5",
            "claude-haiku-5-5",
        ] {
            assert_eq!(fact(id), Some(&json!(true)), "{label}: {id}");
        }
        // The page names Fable 5 as lacking per-message effort.
        assert_eq!(fact("claude-fable-5"), Some(&json!(false)), "{label}");
        // Not documented either way: absent, which the runner treats as false.
        assert_eq!(fact("claude-opus-4-5"), None, "{label}");
        let rows = disclosed_fact(response, "anthropic/claude-opus-5-5", EFFORT_CACHE);
        assert_eq!(rows.len(), 1, "{label}: {:?}", response.fact_overrides);
        assert!(rows[0].source_ref.contains("effort-per-message"));
    }
    // A reseller copy and an alias route reach the model another way.
    for other in ["opencode-go/claude-haiku-5-5", OPUS_ALIAS] {
        assert!(!plain.models[other].contains_key(EFFORT_CACHE), "{other}");
    }
}
