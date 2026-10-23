//! Curated rows the shipped file holds.
//!
//! A held row keeps its data in the file but is not served. On current reads
//! its id is reported the way a retired alias is: absent on a retired-inclusive
//! read and listed in `retired` on a named read, so a consumer that tracks
//! identities never sees it vanish. These tests plant a copy of the shipped
//! file that holds the four System One rows, whatever the shipped file holds
//! today, against a store that knows none of them.
//! `curated_withdrawn_tests` covers a held id the store holds as a withdrawn
//! upstream row.

use super::*;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use serde_json::{json, Value};

/// The held ids, as (provider, model).
const HELD: &[(&str, &str)] = &[
    ("cloudflare-workers-ai", "@cf/cloudflare/clef"),
    ("cloudflare-workers-ai", "@cf/cloudflare/clef-flash"),
    ("cloudflare-workers-ai", "@cf/cloudflare/clairic"),
    ("typesafe", "jev-1.13.0"),
];

/// The planted `held_since_ms`, 2026-10-21T20:30:00Z.
const HELD_SINCE_MS: i64 = crate::curated_models::TEST_HELD_SINCE_MS;

/// An empty store, with the holding document planted for the guard's life.
fn store() -> (
    CatalogStore,
    tempfile::TempDir,
    crate::curated_models::Planted,
) {
    let held = crate::curated_models::plant(&crate::curated_models::document_holding_system_one());
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("held.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    (store, dir, held)
}

fn try_get(store: &CatalogStore, args: Value) -> Result<CatalogGetResponse, RouteError> {
    serve_catalog_get(store, &serde_json::to_vec(&args).unwrap())
}

fn get(store: &CatalogStore, args: Value) -> CatalogGetResponse {
    try_get(store, args).unwrap()
}

/// Every page of a paged read, one provider per page.
fn pages(store: &CatalogStore, include_retired: bool) -> Vec<CatalogGetResponse> {
    let mut args = json!({"paged": true, "include_retired": include_retired});
    let mut pages = Vec::new();
    loop {
        let page =
            serve_catalog_get_with_budget(store, &serde_json::to_vec(&args).unwrap(), 1).unwrap();
        let next = page.next_page_token.clone();
        pages.push(page);
        match next {
            Some(token) => args["page_token"] = token.into(),
            None => return pages,
        }
    }
}

fn id((provider, model): &(&str, &str)) -> String {
    format!("{provider}/{model}")
}

/// No held id is served, disclosed as curated, or reported retired.
fn assert_not_listed(response: &CatalogGetResponse, read: &str) {
    for held in HELD {
        let id = id(held);
        assert!(!response.models.contains_key(&id), "{read}: {id} is held");
        assert!(
            !response.curated.iter().any(|c| c.model == id),
            "{read}: {id} must not be disclosed as curated"
        );
        assert!(
            !response.retired.iter().any(|r| r.model == id),
            "{read}: a plain read lists no retirement"
        );
    }
    // Only the held rows state `model.kind`, so no row may carry it.
    assert!(
        !response
            .models
            .values()
            .any(|facts| facts.contains_key("model.kind")),
        "{read}: no served row states model.kind while the System One rows are held"
    );
}

/// Each held id is present exactly once, as an identity whose only fact is
/// `existence: absent`, with no curated or retired disclosure.
fn assert_absent_once(responses: &[CatalogGetResponse], ids: &[(&str, &str)], read: &str) {
    for held in ids {
        let id = id(held);
        let rows: Vec<_> = responses
            .iter()
            .filter_map(|response| response.models.get(&id))
            .collect();
        assert_eq!(rows.len(), 1, "{read}: {id} exactly once");
        assert_eq!(
            rows[0],
            &BTreeMap::from([("existence".to_string(), json!("absent"))]),
            "{read}: {id}"
        );
        for response in responses {
            assert!(
                !response.curated.iter().any(|c| c.model == id),
                "{read}: {id} must not be disclosed as curated"
            );
            assert!(
                !response.retired.iter().any(|r| r.model == id),
                "{read}: {id}"
            );
        }
    }
}

#[test]
fn held_rows_are_not_served_on_current_reads() {
    let (store, _dir, _held) = store();
    let bulk = get(&store, json!({}));
    assert_not_listed(&bulk, "bulk");
    // Control: the read serves curated rows at all, so the absence above is
    // the hold and not an empty catalog.
    assert!(bulk.models.contains_key("meta/muse-wonder-full-1.0"));
    assert_not_listed(&get(&store, json!({"fact_prefixes":["model."]})), "model.");
    for provider in ["cloudflare-workers-ai", "typesafe"] {
        assert_not_listed(&get(&store, json!({"provider_id":provider})), provider);
        assert_not_listed(
            &get(
                &store,
                json!({"provider_id":provider, "auth_method":"apikey"}),
            ),
            "apikey plane",
        );
        // A plane read never reports retirements, retired-inclusive or not.
        assert_not_listed(
            &get(
                &store,
                json!({"provider_id":provider, "auth_method":"apikey", "include_retired":true}),
            ),
            "apikey plane include_retired",
        );
    }
    for page in pages(&store, false) {
        assert_not_listed(&page, "paged");
    }
}

#[test]
fn held_rows_are_absent_identities_on_retired_inclusive_reads() {
    let (store, _dir, _held) = store();
    assert_absent_once(
        &[get(&store, json!({"include_retired":true}))],
        HELD,
        "bulk include_retired",
    );
    for provider in ["cloudflare-workers-ai", "typesafe"] {
        let response = get(
            &store,
            json!({"provider_id":provider, "include_retired":true}),
        );
        let ids: Vec<(&str, &str)> = HELD
            .iter()
            .filter(|(p, _)| *p == provider)
            .copied()
            .collect();
        assert_absent_once(
            std::slice::from_ref(&response),
            &ids,
            "provider include_retired",
        );
        // Nothing else under the provider: the store knows none of its ids.
        assert_eq!(response.models.len(), ids.len(), "{provider}");
    }

    let pages = pages(&store, true);
    assert!(pages.len() > 1, "the budget must split the read into pages");
    assert_absent_once(&pages, HELD, "paged include_retired");
    // Each held id rides on its own provider's page: paging is by provider.
    for held in HELD {
        let id = id(held);
        let page = pages
            .iter()
            .find(|page| page.models.contains_key(&id))
            .unwrap();
        let providers: BTreeSet<&str> = page.models.keys().map(|m| model_provider(m)).collect();
        assert_eq!(providers, BTreeSet::from([held.0]), "{id}");
    }
}

#[test]
fn a_named_read_of_a_held_id_reports_it_retired() {
    let (store, _dir, _held) = store();
    for held in HELD {
        let (provider, model) = held;
        let id = id(held);
        let named = try_get(&store, json!({"provider_id":provider, "model_id":model}))
            .unwrap_or_else(|e| panic!("{id}: a held id is known, not refused: {e:?}"));
        assert!(named.models.is_empty(), "{id}: {:?}", named.models);
        assert!(named.curated.is_empty(), "{id}");
        assert_eq!(
            named.retired,
            [fusiform_protocol::RetiredModelWire {
                model: id.clone(),
                retired_at_ms: HELD_SINCE_MS,
                successor: None,
            }],
            "{id}"
        );
        // Retired-inclusive: the identity, marked absent, and no retirement.
        let named = get(
            &store,
            json!({"provider_id":provider, "model_id":model, "include_retired":true}),
        );
        assert_absent_once(
            std::slice::from_ref(&named),
            &[*held],
            "named include_retired",
        );
        assert_eq!(named.models.len(), 1, "{id}");
    }
}

/// With jev held, typesafe has no row to serve. It stays a known provider:
/// a plain provider read answers with zero models, not a refusal.
#[test]
fn a_provider_whose_only_row_is_held_stays_known() {
    let (store, _dir, _held) = store();
    let plain = get(&store, json!({"provider_id":"typesafe"}));
    assert!(plain.models.is_empty(), "{:?}", plain.models);
    assert!(plain.curated.is_empty() && plain.retired.is_empty());
    let all = get(
        &store,
        json!({"provider_id":"typesafe", "include_retired":true}),
    );
    assert_eq!(
        all.models,
        BTreeMap::from([(
            "typesafe/jev-1.13.0".to_string(),
            BTreeMap::from([("existence".to_string(), json!("absent"))])
        )])
    );
    // A model typesafe never carried is still an unknown model, not an
    // unknown provider.
    let refused = try_get(
        &store,
        json!({"provider_id":"typesafe", "model_id":"jev-latest"}),
    )
    .unwrap_err();
    assert!(
        refused.message.starts_with("unknown model"),
        "{}",
        refused.message
    );
}

/// A held row names no curated source in history: like a retired alias, the
/// store has no eras for the id, so history refuses it as an unknown model.
#[test]
fn history_of_a_held_id_names_no_curated_source() {
    let (store, _dir, _held) = store();
    for (provider, model) in HELD {
        let refused = serve_history(
            &store,
            &serde_json::to_vec(
                &json!({"provider_id":provider, "model_id":model, "fact_key":"model.kind"}),
            )
            .unwrap(),
        )
        .unwrap_err();
        assert_eq!(refused.code, fusiform_protocol::CODE_NO_COVERAGE);
        assert!(
            refused.message.starts_with("unknown model"),
            "{}",
            refused.message
        );
    }
}

/// Control: removing one held entry from the document serves that row as curated
/// again, and only that row.
#[test]
fn releasing_a_held_entry_serves_the_row_as_curated_again() {
    let (store, _dir, _held) = store();
    for released in HELD {
        let _planted =
            crate::curated_models::plant(&crate::curated_models::document_releasing(&[*released]));
        let id = id(released);
        let (provider, model) = released;
        for (read, response) in [
            ("bulk", get(&store, json!({}))),
            (
                "bulk include_retired",
                get(&store, json!({"include_retired":true})),
            ),
            ("provider", get(&store, json!({"provider_id":provider}))),
            (
                "named",
                get(&store, json!({"provider_id":provider, "model_id":model})),
            ),
        ] {
            assert_eq!(response.models[&id]["existence"], "present", "{read}: {id}");
            assert!(response.models[&id].contains_key("model.kind"), "{read}");
            assert_eq!(
                response.curated.iter().filter(|c| c.model == id).count(),
                1,
                "{read}: {id}"
            );
            assert!(response.retired.is_empty(), "{read}: {id}");
            for still_held in HELD.iter().filter(|h| *h != released) {
                let other = self::id(still_held);
                assert!(
                    response
                        .models
                        .get(&other)
                        .is_none_or(|facts| facts["existence"] == "absent"),
                    "{read}: {other} is still held"
                );
            }
        }
    }
}
