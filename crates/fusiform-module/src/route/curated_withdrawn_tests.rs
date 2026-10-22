//! A curated row and an upstream row models.dev listed and then withdrew.
//!
//! Upstream wins only while it currently publishes the model. These tests
//! ingest real models.dev documents into a real store: one that lists
//! `cloudflare-workers-ai/@cf/cloudflare/clef`, a later one that withdraws it,
//! and (in the control) a third that lists it again.

use super::*;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{BoundaryKind, ObservationOutcome};
use fusiform_store::{ingest::plan_ingest, NewObservation};
use serde_json::{json, Value};

const PROVIDER: &str = "cloudflare-workers-ai";
const MODEL: &str = "@cf/cloudflare/clef";
const ID: &str = "cloudflare-workers-ai/@cf/cloudflare/clef";
/// An upstream model under the same provider that is never withdrawn, so the
/// provider stays published throughout.
const ORDINARY: &str = "cloudflare-workers-ai/@cf/meta/ordinary";

/// When upstream lists clef, then withdraws it, then (control) lists it again.
const LISTED_AT: i64 = 1_000;
const WITHDRAWN_AT: i64 = 2_000;
const RELISTED_AT: i64 = 3_000;

fn empty_store() -> (CatalogStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("withdrawn.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    (store, dir)
}

/// Ingest one models.dev document as the poll loop does: the first document
/// into an empty store is a seed, every later one an observed change.
fn ingest(store: &CatalogStore, at: i64, lists_clef: bool) {
    let mut models = serde_json::Map::new();
    models.insert(
        "@cf/meta/ordinary".into(),
        json!({"id":"@cf/meta/ordinary", "name":"Ordinary", "family":"ordinary",
               "limit":{"context":4000, "output":1000}, "cost":{"input":1, "output":2}}),
    );
    if lists_clef {
        // Upstream facts deliberately unlike the curated row's, and some keys
        // the curated row does not carry at all, so any upstream fact mixed
        // into a curated read is visible.
        models.insert(
            MODEL.into(),
            json!({"id":MODEL, "name":"Clef (upstream)", "family":"upstream-clef",
                   "reasoning":true, "limit":{"context":1000, "output":500},
                   "cost":{"input":9, "output":9}}),
        );
    }
    let doc = json!({PROVIDER: {"id":PROVIDER, "name":"Cloudflare Workers AI", "models":models}});
    let catalog =
        fusiform_core::normalize::normalize_models_dev(&serde_json::to_vec(&doc).unwrap())
            .unwrap()
            .catalog;
    let seed = at == LISTED_AT;
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

/// The shipped curated file with nothing held, so clef is served as curated
/// whenever upstream does not currently publish it.
fn served() -> crate::curated_models::Planted {
    crate::curated_models::plant(&crate::curated_models::document_without_held())
}

fn get(store: &CatalogStore, args: Value) -> CatalogGetResponse {
    serve_catalog_get(store, &serde_json::to_vec(&args).unwrap()).unwrap()
}

/// Every page of a paged read, one provider per page, merged.
fn paged(store: &CatalogStore, include_retired: bool) -> CatalogGetResponse {
    let mut args = json!({"paged": true, "include_retired": include_retired});
    let mut merged: Option<CatalogGetResponse> = None;
    loop {
        let page =
            serve_catalog_get_with_budget(store, &serde_json::to_vec(&args).unwrap(), 1).unwrap();
        let next = page.next_page_token.clone();
        match merged.as_mut() {
            None => merged = Some(page),
            Some(all) => {
                all.models.extend(page.models);
                all.curated.extend(page.curated);
                all.retired.extend(page.retired);
                all.withheld.extend(page.withheld);
                all.uncertain.extend(page.uncertain);
            }
        }
        match next {
            Some(token) => args["page_token"] = token.into(),
            None => return merged.unwrap(),
        }
    }
}

fn history(store: &CatalogStore, fact_key: &str) -> HistoryResponse {
    serve_history(
        store,
        &serde_json::to_vec(
            &json!({"provider_id":PROVIDER, "model_id":MODEL, "fact_key":fact_key}),
        )
        .unwrap(),
    )
    .unwrap()
}

/// The shipped curated row's facts, exactly as the file states them.
fn curated_facts() -> BTreeMap<String, Value> {
    crate::curated_models::models()[&(PROVIDER.to_string(), MODEL.to_string())]
        .facts
        .clone()
}

/// Clef is served as the curated row, whole, and disclosed once.
fn assert_curated_clef(response: &CatalogGetResponse, read: &str) {
    let facts = response
        .models
        .get(ID)
        .unwrap_or_else(|| panic!("{read}: {ID} must be served"));
    assert_eq!(facts["model.kind"], "decision", "{read}");
    assert_eq!(facts["existence"], "present", "{read}");
    // Equal to the curated row, so no stored upstream fact is mixed in: the
    // upstream row's family, output limit and reasoning flag would show here.
    assert_eq!(facts, &curated_facts(), "{read}");
    let disclosures: Vec<_> = response.curated.iter().filter(|c| c.model == ID).collect();
    assert_eq!(disclosures.len(), 1, "{read}: one curated disclosure");
    assert_eq!(disclosures[0].auth_methods, ["apikey"], "{read}");
    assert!(
        !response.retired.iter().any(|r| r.model == ID),
        "{read}: a served curated row is not retired"
    );
    assert!(
        !response.withheld.iter().any(|w| w.model == ID)
            && !response
                .uncertain
                .iter()
                .any(|u| u.provider_id == PROVIDER && u.model_id == MODEL),
        "{read}: no disclosure of the withdrawn upstream row survives"
    );
}

/// Clef is served as the upstream row, with no curated disclosure.
fn assert_upstream_clef(response: &CatalogGetResponse, read: &str) {
    let facts = response
        .models
        .get(ID)
        .unwrap_or_else(|| panic!("{read}: {ID} must be served"));
    assert_eq!(facts["existence"], "present", "{read}");
    assert_eq!(facts["limit.context"], 1000, "{read}: the upstream value");
    assert_eq!(facts["model.family"], "upstream-clef", "{read}");
    assert!(!facts.contains_key("model.kind"), "{read}: {facts:?}");
    assert!(
        !response.curated.iter().any(|c| c.model == ID),
        "{read}: upstream wins, so clef is not disclosed as curated"
    );
}

/// The current reads the rule covers, each labelled for failure messages.
fn current_reads(store: &CatalogStore) -> Vec<(&'static str, CatalogGetResponse)> {
    vec![
        ("bulk", get(store, json!({}))),
        ("provider", get(store, json!({"provider_id":PROVIDER}))),
        (
            "apikey plane",
            get(
                store,
                json!({"provider_id":PROVIDER, "auth_method":"apikey"}),
            ),
        ),
        ("paged", paged(store, false)),
        (
            "named",
            get(store, json!({"provider_id":PROVIDER, "model_id":MODEL})),
        ),
        (
            "bulk include_retired",
            get(store, json!({"include_retired":true})),
        ),
        (
            "provider include_retired",
            get(
                store,
                json!({"provider_id":PROVIDER, "include_retired":true}),
            ),
        ),
        ("paged include_retired", paged(store, true)),
        (
            "named include_retired",
            get(
                store,
                json!({"provider_id":PROVIDER, "model_id":MODEL, "include_retired":true}),
            ),
        ),
    ]
}

#[test]
fn a_withdrawn_upstream_row_gives_way_to_the_curated_row() {
    let _planted = served();
    let (store, _dir) = empty_store();
    ingest(&store, LISTED_AT, true);
    ingest(&store, WITHDRAWN_AT, false);

    // Control: the store really holds clef as a withdrawn upstream row with
    // upstream facts, so the reads below are not passing on an empty store.
    let (stored, _) = store
        .read_model(SourceId::ModelsDev, PROVIDER, MODEL, None)
        .unwrap();
    let stored = stored.expect("the store knows clef");
    assert!(!stored.is_present());
    assert!(store
        .model_is_known(SourceId::ModelsDev, PROVIDER, MODEL)
        .unwrap());

    for (read, response) in current_reads(&store) {
        assert_curated_clef(&response, read);
        // Control: the upstream row that was never withdrawn is still served.
        assert!(response.models.contains_key(ORDINARY) || read.starts_with("named"));
    }

    // At the instant upstream listed clef, a point-in-time read serves the
    // stored upstream row and no curated row.
    for (read, args) in [
        (
            "named at_ms",
            json!({"provider_id":PROVIDER, "model_id":MODEL, "at_ms":LISTED_AT}),
        ),
        (
            "provider at_ms",
            json!({"provider_id":PROVIDER, "at_ms":LISTED_AT}),
        ),
        ("bulk at_ms", json!({"at_ms":LISTED_AT})),
    ] {
        let response = get(&store, args);
        assert_upstream_clef(&response, read);
        assert!(response.curated.is_empty(), "{read}");
    }
    // After the withdrawal, a point-in-time read reports what the store held:
    // the upstream row as retired, never the curated row.
    let response = get(
        &store,
        json!({"provider_id":PROVIDER, "model_id":MODEL, "at_ms":WITHDRAWN_AT + 1}),
    );
    assert!(response.curated.is_empty());
    assert!(!response.models.contains_key(ID));
    assert!(response.retired.iter().any(|r| r.model == ID));

    // History: a fact the upstream row recorded keeps its stored eras; a fact
    // only the curated row carries names the curated source.
    let limit = history(&store, "limit.context");
    assert_eq!(limit.eras.len(), 1);
    assert_eq!(limit.eras[0].value, json!(1000));
    assert!(limit.curated.is_none());
    let existence = history(&store, "existence");
    let values: Vec<&Value> = existence.eras.iter().map(|e| &e.value).collect();
    assert_eq!(values, [&json!("present"), &json!("absent")]);
    let kind = history(&store, "model.kind");
    assert!(kind.eras.is_empty());
    assert_eq!(kind.curated.expect("curated source named").model, ID);
}

#[test]
fn an_upstream_relisting_wins_over_the_curated_row_again() {
    let _planted = served();
    let (store, _dir) = empty_store();
    ingest(&store, LISTED_AT, true);
    ingest(&store, WITHDRAWN_AT, false);
    // Withdrawn: the curated row is served.
    assert_curated_clef(&get(&store, json!({})), "bulk after withdrawal");

    // Upstream lists clef again. Nothing else changes: the curated file and
    // the process are the same, so the switch back is decided at read time.
    ingest(&store, RELISTED_AT, true);
    for (read, response) in current_reads(&store) {
        assert_upstream_clef(&response, read);
    }
    let kind = history(&store, "model.kind");
    assert!(kind.eras.is_empty());
    assert!(kind.curated.is_none(), "upstream wins on history too");
}

/// With clef held and withdrawn upstream, the store's own
/// withdrawn row is what every current read reports: no curated row, no
/// second row for the id, and on a named read the store's retirement.
#[test]
fn a_held_row_leaves_the_withdrawn_upstream_row_to_report_itself() {
    let _held = crate::curated_models::plant(&crate::curated_models::document_holding_system_one());
    let (store, _dir) = empty_store();
    ingest(&store, LISTED_AT, true);
    ingest(&store, WITHDRAWN_AT, false);

    let named = get(&store, json!({"provider_id":PROVIDER, "model_id":MODEL}));
    assert!(named.models.is_empty(), "{:?}", named.models);
    assert!(named.curated.is_empty());
    assert_eq!(
        named.retired,
        [fusiform_protocol::RetiredModelWire {
            model: ID.to_string(),
            // The store's retirement instant, not the file's held_since_ms.
            retired_at_ms: WITHDRAWN_AT,
        }]
    );

    for (read, response) in current_reads(&store) {
        assert!(
            !response.curated.iter().any(|c| c.model == ID),
            "{read}: a held row is not disclosed as curated"
        );
        if read.contains("include_retired") {
            // The stored upstream row, once, marked absent: its own facts.
            let facts = &response.models[ID];
            assert_eq!(facts["existence"], "absent", "{read}");
            assert_eq!(facts["model.family"], "upstream-clef", "{read}");
            assert!(!facts.contains_key("model.kind"), "{read}");
        } else {
            assert!(!response.models.contains_key(ID), "{read}");
        }
        // Control: the upstream row that was never withdrawn is still served.
        assert!(response.models.contains_key(ORDINARY) || read.starts_with("named"));
    }

    // History names no curated source for a fact only the curated row states.
    let kind = history(&store, "model.kind");
    assert!(kind.eras.is_empty());
    assert!(kind.curated.is_none());
}
