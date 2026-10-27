//! Point-in-time reads of curated model rows, served from the store's history
//! of the curated file.
//!
//! Each boot records the curated file in the store
//! (`curated_models::apply_to_store`). These tests apply versions of the
//! shipped file at chosen boot instants, then read at instants around the
//! eras those boots wrote.

use super::*;
use crate::curated_models::{apply_to_store, load_table_reporting_rejects, Table};
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, ObservationOutcome};
use fusiform_store::{ingest::plan_ingest, NewObservation};
use serde_json::{json, Value};

const TYPESAFE: &str = "typesafe";
const JEV: &str = "jev-1.13.0";
const JEV_ID: &str = "typesafe/jev-1.13.0";
const CLOUDFLARE: &str = "cloudflare-workers-ai";
const CLEF: &str = "@cf/cloudflare/clef";
const CLEF_ID: &str = "cloudflare-workers-ai/@cf/cloudflare/clef";

/// When the store's record of models.dev begins: long before any curated
/// row, so no read below is refused for predating the record.
const RECORD_BEGINS_AT: i64 = 1_000;

const DAY_MS: i64 = 86_400_000;

fn empty_store() -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("history.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    (store, dir)
}

/// Ingest one models.dev document listing an ordinary Cloudflare model and,
/// when asked, clef with upstream facts unlike the curated row's. The first
/// document is a seed, every later one an observed change, as in the poll
/// loop.
fn ingest(store: &CatalogStore, at: i64, lists_clef: bool) {
    let mut models = serde_json::Map::new();
    models.insert(
        "@cf/meta/ordinary".into(),
        json!({"id":"@cf/meta/ordinary", "name":"Ordinary", "family":"ordinary",
               "limit":{"context":4000, "output":1000}, "cost":{"input":1, "output":2}}),
    );
    if lists_clef {
        models.insert(
            CLEF.into(),
            json!({"id":CLEF, "name":"Clef (upstream)", "family":"upstream-clef",
                   "limit":{"context":1000, "output":500}, "cost":{"input":9, "output":9}}),
        );
    }
    let doc =
        json!({CLOUDFLARE: {"id":CLOUDFLARE, "name":"Cloudflare Workers AI", "models":models}});
    let catalog =
        fusiform_core::normalize::normalize_models_dev(&serde_json::to_vec(&doc).unwrap())
            .unwrap()
            .catalog;
    let seed = at == RECORD_BEGINS_AT;
    let observation = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(at),
            outcome: if seed {
                ObservationOutcome::Seeded
            } else {
                ObservationOutcome::Changed { snapshot_seq: 0 }
            },
            normalized_hash: Some(format!("h{at}")),
            raw_hash: Some(format!("r{at}")),
            etag: None,
            duration_ms: Some(10),
            detail: None,
        })
        .unwrap();
    let (kind, era_observation) = if seed {
        (BoundaryKind::Seed, None)
    } else {
        (BoundaryKind::Observed, Some(observation))
    };
    let plan = plan_ingest(store, &catalog, Timestamp(at), kind, era_observation).unwrap();
    store.append_eras(&plan.eras).unwrap();
    store
        .advance_catalog_version(observation, Timestamp(at))
        .unwrap();
}

/// A store whose models.dev record has begun, with no curated history yet.
fn store() -> (CatalogStore, tempfile::TempDir) {
    let (store, dir) = empty_store();
    ingest(&store, RECORD_BEGINS_AT, false);
    (store, dir)
}

/// The shipped curated file with nothing held, with one edit applied to the
/// parsed document. The edited file must load without rejects, so a test
/// cannot pass on a document that silently lost a row.
fn table(edit: impl FnOnce(&mut Value)) -> Table {
    let mut doc: Value =
        serde_json::from_str(&crate::curated_models::document_without_held()).unwrap();
    edit(&mut doc);
    let (table, rejects) = load_table_reporting_rejects(&doc.to_string());
    assert!(rejects.is_empty(), "{rejects:?}");
    table
}

fn shipped() -> Table {
    table(|_| {})
}

/// The row of that identity in a parsed curated document.
fn row_in<'a>(doc: &'a mut Value, provider: &str, model: &str) -> &'a mut Value {
    doc["models"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|row| row["provider_id"] == provider && row["model_id"] == model)
        .expect("the row is shipped")
}

fn without(provider: &'static str, model: &'static str) -> impl FnOnce(&mut Value) {
    move |doc| {
        doc["models"]
            .as_array_mut()
            .unwrap()
            .retain(|row| row["provider_id"] != provider || row["model_id"] != model);
    }
}

fn holding(provider: &'static str, model: &'static str) -> impl FnOnce(&mut Value) {
    move |doc| {
        doc["held"] = json!([{"provider_id":provider, "model_id":model,
            "held_since_ms":1, "reason":"held by a test"}]);
    }
}

fn established_at(provider: &str, model: &str) -> i64 {
    shipped().models[&(provider.to_string(), model.to_string())].established_at_ms
}

fn jev_established_at() -> i64 {
    established_at(TYPESAFE, JEV)
}

fn try_get(store: &CatalogStore, args: Value) -> Result<CatalogGetResponse, RouteError> {
    serve_catalog_get(store, &serde_json::to_vec(&args).unwrap())
}

fn get(store: &CatalogStore, args: Value) -> CatalogGetResponse {
    try_get(store, args).unwrap()
}

fn named_at(store: &CatalogStore, provider: &str, model: &str, at: i64) -> CatalogGetResponse {
    get(
        store,
        json!({"provider_id":provider, "model_id":model, "at_ms":at}),
    )
}

fn refused_at(store: &CatalogStore, provider: &str, model: &str, at: i64) -> RouteError {
    let refused = try_get(
        store,
        json!({"provider_id":provider, "model_id":model, "at_ms":at}),
    )
    .unwrap_err();
    assert_eq!(refused.code, fusiform_protocol::CODE_NO_COVERAGE);
    assert!(
        !refused.message.contains("check the model id"),
        "a curated id is right, so the refusal must not suggest a typo: {}",
        refused.message
    );
    refused
}

fn jev_input_units(response: &CatalogGetResponse) -> i64 {
    response.models[JEV_ID]["rate.input"]["units"]
        .as_i64()
        .expect("jev's input rate is priced")
}

/// Jev at an instant inside its first era: the shipped rates, its other
/// facts, and the disclosure, exactly as a current read serves them.
#[test]
fn a_read_after_the_first_era_serves_jevs_recorded_rates_and_disclosure() {
    let (store, _dir) = store();
    let boot = jev_established_at() + 5 * DAY_MS;
    let written = apply_to_store(&store, &shipped(), boot).unwrap();
    // The shipped file serves five rows and holds none, so a first boot
    // writes one era per row.
    assert_eq!(written, 5);

    let file_row = &shipped().models[&(TYPESAFE.to_string(), JEV.to_string())];
    for at in [
        jev_established_at(),
        jev_established_at() + 1,
        boot + DAY_MS,
    ] {
        let response = named_at(&store, TYPESAFE, JEV, at);
        assert_eq!(
            response.resolved_at_ms, at,
            "the instant the read describes"
        );
        let facts = &response.models[JEV_ID];
        assert_eq!(
            facts["rate.input"],
            json!({"state":"priced", "units":42_000_000, "exponent":9, "currency":"USD",
                   "unit_provenance":{"kind":"stated"}}),
            "at {at}"
        );
        assert_eq!(
            facts["rate.output"],
            json!({"state":"stated_zero"}),
            "at {at}"
        );
        assert_eq!(facts, &file_row.facts, "the whole curated row, at {at}");
        assert_eq!(
            response.curated,
            [curated_disclosure(file_row)],
            "the disclosure carries the era's provenance, at {at}"
        );

        // Bulk and provider reads at the same instant carry it too.
        for args in [
            json!({"at_ms":at}),
            json!({"provider_id":TYPESAFE, "at_ms":at}),
        ] {
            let response = get(&store, args.clone());
            assert_eq!(&response.models[JEV_ID], facts, "{args}");
            assert!(response.curated.iter().any(|c| c.model == JEV_ID), "{args}");
        }
    }
}

#[test]
fn a_read_before_the_first_era_is_refused_with_the_reason() {
    let (store, _dir) = store();
    apply_to_store(&store, &shipped(), jev_established_at() + DAY_MS).unwrap();
    let before = jev_established_at() - 1;
    let refused = refused_at(&store, TYPESAFE, JEV, before);
    assert!(
        refused.message.contains(&format!(
            "{JEV_ID} is a curated row, and fusiform's record of it begins at {}",
            jev_established_at()
        )),
        "{}",
        refused.message
    );
    // A bulk read at the instant does not list it.
    let response = get(&store, json!({"at_ms":before}));
    assert!(!response.models.contains_key(JEV_ID));
    assert!(!response.curated.iter().any(|c| c.model == JEV_ID));
    // Control: a wrong id under the provider is still an unknown model.
    let refused = try_get(
        &store,
        json!({"provider_id":TYPESAFE, "model_id":"jev-9.9.9", "at_ms":before}),
    )
    .unwrap_err();
    assert!(
        refused.message.starts_with("unknown model"),
        "{}",
        refused.message
    );
}

#[test]
fn a_changed_curated_price_opens_an_era_dated_at_the_boot_that_carries_it() {
    let (store, _dir) = store();
    let first_boot = jev_established_at() + DAY_MS;
    apply_to_store(&store, &shipped(), first_boot).unwrap();

    let repriced = table(|doc| {
        row_in(doc, TYPESAFE, JEV)["facts"]["rate.input"]["units"] = json!(50_000_000);
    });
    let boot = jev_established_at() + 10 * DAY_MS;
    assert_eq!(
        apply_to_store(&store, &repriced, boot).unwrap(),
        1,
        "only the repriced row is written"
    );

    // Dated at the boot, not at the row's unchanged curation date: a read
    // between the two still gets the old price.
    assert_eq!(
        jev_input_units(&named_at(&store, TYPESAFE, JEV, boot - 1)),
        42_000_000
    );
    assert_eq!(
        jev_input_units(&named_at(&store, TYPESAFE, JEV, boot)),
        50_000_000
    );
    assert_eq!(
        jev_input_units(&named_at(&store, TYPESAFE, JEV, boot + 1)),
        50_000_000
    );
    let boundaries: Vec<i64> = store
        .curated_model_eras(TYPESAFE, JEV)
        .unwrap()
        .iter()
        .map(|era| era.boundary_at_ms)
        .collect();
    assert_eq!(boundaries, [jev_established_at(), boot]);

    // A boot whose clock is behind the record dates the next change at the
    // newest boundary rather than before it, so the history never runs
    // backwards and the newer claim is the one in force.
    let restored = shipped();
    assert_eq!(apply_to_store(&store, &restored, boot - DAY_MS).unwrap(), 1);
    assert_eq!(
        jev_input_units(&named_at(&store, TYPESAFE, JEV, boot)),
        42_000_000
    );
    assert_eq!(
        jev_input_units(&named_at(&store, TYPESAFE, JEV, boot - 1)),
        42_000_000
    );
}

#[test]
fn re_applying_an_unchanged_or_only_redated_file_writes_nothing() {
    let (store, _dir) = store();
    let boot = jev_established_at() + DAY_MS;
    assert_eq!(apply_to_store(&store, &shipped(), boot).unwrap(), 5);
    assert_eq!(
        apply_to_store(&store, &shipped(), boot + DAY_MS).unwrap(),
        0
    );

    // A re-verification moves the row's dates and source and changes no
    // served fact. That is a confirmation, not a change, so nothing is
    // written and the stored claim keeps its first provenance.
    let redated = table(|doc| {
        let row = row_in(doc, TYPESAFE, JEV);
        row["established_at_ms"] = json!(jev_established_at() + 30 * DAY_MS);
        row["review_by_ms"] = json!(jev_established_at() + 90 * DAY_MS);
        row["source_ref"] = json!("Re-read 2026-11-20 at https://docs.typesafe.ai/models.");
        row["auth_methods"] = json!(["apikey"]);
    });
    assert_eq!(
        apply_to_store(&store, &redated, boot + 2 * DAY_MS).unwrap(),
        0
    );
    assert_eq!(store.curated_model_eras(TYPESAFE, JEV).unwrap().len(), 1);

    // Control: a fact change on the same row does write, so the zeros above
    // are not an apply that writes nothing at all.
    let renamed = table(|doc| {
        row_in(doc, TYPESAFE, JEV)["facts"]["model.name"] = json!("Jev 1.13 (renamed)");
    });
    assert_eq!(
        apply_to_store(&store, &renamed, boot + 3 * DAY_MS).unwrap(),
        1
    );
}

#[test]
fn a_removed_row_is_tombstoned_and_a_read_after_it_is_refused() {
    let (store, _dir) = store();
    let first_boot = jev_established_at() + DAY_MS;
    apply_to_store(&store, &shipped(), first_boot).unwrap();

    let removed_at = jev_established_at() + 10 * DAY_MS;
    assert_eq!(
        apply_to_store(&store, &table(without(TYPESAFE, JEV)), removed_at).unwrap(),
        1,
        "the removed row, and only it, is tombstoned"
    );
    let refused = refused_at(&store, TYPESAFE, JEV, removed_at + 1);
    assert!(
        refused.message.contains(&format!(
            "{JEV_ID} is a curated row fusiform stopped serving at {removed_at}"
        )),
        "{}",
        refused.message
    );
    let response = get(&store, json!({"at_ms":removed_at + 1}));
    assert!(!response.models.contains_key(JEV_ID));
    // Before the tombstone, the claim then in force still answers.
    assert_eq!(
        jev_input_units(&named_at(&store, TYPESAFE, JEV, removed_at - 1)),
        42_000_000
    );
    // An ordinary later boot does not tombstone it again.
    assert_eq!(
        apply_to_store(&store, &table(without(TYPESAFE, JEV)), removed_at + DAY_MS).unwrap(),
        0
    );

    // A held row is not in effect either, so it is tombstoned like a removed
    // one.
    let held_at = removed_at + 2 * DAY_MS;
    let holding_clef = table(|doc| {
        without(TYPESAFE, JEV)(doc);
        holding(CLOUDFLARE, CLEF)(doc);
    });
    assert_eq!(apply_to_store(&store, &holding_clef, held_at).unwrap(), 1);
    let refused = refused_at(&store, CLOUDFLARE, CLEF, held_at + 1);
    assert!(
        refused.message.contains("stopped serving at"),
        "{}",
        refused.message
    );

    // Restoring the full file serves both again from that boot on, and the
    // gap stays refused.
    let restored_at = held_at + DAY_MS;
    assert_eq!(apply_to_store(&store, &shipped(), restored_at).unwrap(), 2);
    assert_eq!(
        jev_input_units(&named_at(&store, TYPESAFE, JEV, restored_at)),
        42_000_000
    );
    refused_at(&store, TYPESAFE, JEV, restored_at - 1);
    assert!(named_at(&store, CLOUDFLARE, CLEF, restored_at)
        .models
        .contains_key(CLEF_ID));
}

/// A row held from the first boot that records it is first served when it is
/// released, not from its curation date, since fusiform did not serve it
/// while it was held.
#[test]
fn a_row_held_from_its_first_boot_is_first_served_at_its_release() {
    let (store, _dir) = store();
    let held_boot = jev_established_at() + DAY_MS;
    assert_eq!(
        apply_to_store(&store, &table(holding(TYPESAFE, JEV)), held_boot).unwrap(),
        5,
        "four served rows and one tombstone for the held row"
    );
    let released_at = held_boot + DAY_MS;
    assert_eq!(apply_to_store(&store, &shipped(), released_at).unwrap(), 1);
    refused_at(&store, TYPESAFE, JEV, jev_established_at() + 1);
    refused_at(&store, TYPESAFE, JEV, released_at - 1);
    assert_eq!(
        jev_input_units(&named_at(&store, TYPESAFE, JEV, released_at)),
        42_000_000
    );
}

/// The upstream-wins rule is asked at the read's instant: where models.dev
/// had the model present then, its row is served and the curated one is not.
#[test]
fn a_historical_read_where_upstream_had_the_model_serves_the_upstream_row() {
    let (store, _dir) = store();
    let clef_established = established_at(CLOUDFLARE, CLEF);
    apply_to_store(&store, &shipped(), clef_established + DAY_MS).unwrap();
    let listed_at = clef_established + 2 * DAY_MS;
    let withdrawn_at = clef_established + 3 * DAY_MS;
    ingest(&store, listed_at, true);
    ingest(&store, withdrawn_at, false);

    let file_row = &shipped().models[&(CLOUDFLARE.to_string(), CLEF.to_string())];
    let mut curated_facts = file_row.facts.clone();
    curated_facts.insert(
        "provider.name".to_string(),
        Value::from("Cloudflare Workers AI"),
    );

    // While upstream lists it: the upstream row, on named and bulk reads.
    for args in [
        json!({"provider_id":CLOUDFLARE, "model_id":CLEF, "at_ms":listed_at + 1}),
        json!({"provider_id":CLOUDFLARE, "at_ms":listed_at + 1}),
        json!({"at_ms":listed_at + 1}),
    ] {
        let response = get(&store, args.clone());
        let facts = &response.models[CLEF_ID];
        assert_eq!(facts["limit.context"], 1000, "{args}: the upstream value");
        assert_eq!(facts["model.family"], "upstream-clef", "{args}");
        assert!(!facts.contains_key("model.kind"), "{args}: {facts:?}");
        assert!(
            !response.curated.iter().any(|c| c.model == CLEF_ID),
            "{args}: upstream wins, so clef is not disclosed as curated"
        );
    }
    // Control: jev, which upstream never listed, is curated at that instant.
    assert!(get(&store, json!({"at_ms":listed_at + 1}))
        .curated
        .iter()
        .any(|c| c.model == JEV_ID));

    // Before upstream listed it and after upstream withdrew it, the curated
    // row is served whole, with nothing of the upstream row mixed in.
    for at in [clef_established + 1, withdrawn_at + 1] {
        for args in [
            json!({"provider_id":CLOUDFLARE, "model_id":CLEF, "at_ms":at}),
            json!({"at_ms":at, "include_retired":true}),
        ] {
            let response = get(&store, args.clone());
            assert_eq!(response.models[CLEF_ID], curated_facts, "{args}");
            assert_eq!(
                response
                    .curated
                    .iter()
                    .filter(|c| c.model == CLEF_ID)
                    .collect::<Vec<_>>(),
                [&curated_disclosure(file_row)],
                "{args}"
            );
            assert!(
                !response.retired.iter().any(|r| r.model == CLEF_ID),
                "{args}"
            );
        }
    }
}

/// A current read is untouched by the stored history: it serves the compiled
/// file even when the store's newest claim differs.
#[test]
fn a_current_read_serves_the_compiled_file_not_the_stored_history() {
    let _planted = crate::curated_models::plant(&crate::curated_models::document_without_held());
    let (store, _dir) = store();
    let repriced = table(|doc| {
        row_in(doc, TYPESAFE, JEV)["facts"]["rate.input"]["units"] = json!(50_000_000);
    });
    apply_to_store(&store, &repriced, jev_established_at() + DAY_MS).unwrap();
    let response = get(&store, json!({"provider_id":TYPESAFE, "model_id":JEV}));
    assert_eq!(jev_input_units(&response), 42_000_000);
}
