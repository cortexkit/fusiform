//! The curated decision and extraction models ("System One" models, in their
//! vendors' term: they return typed decisions or values, never replies). None
//! is in models.dev, and one is under a provider models.dev does not list.

use super::*;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_protocol::money::UnitProvenance;
use fusiform_protocol::RateValue;
use serde_json::{json, Value};

/// Identity, kind, input rate in nanodollars, context limit.
const ROWS: &[(&str, &str, &str, i64, Option<i64>)] = &[
    (
        "cloudflare-workers-ai",
        "@cf/cloudflare/clef",
        "decision",
        240_000_000,
        Some(65_536),
    ),
    (
        "cloudflare-workers-ai",
        "@cf/cloudflare/clef-flash",
        "decision",
        38_000_000,
        Some(24_576),
    ),
    (
        "cloudflare-workers-ai",
        "@cf/cloudflare/clairic",
        "extraction",
        100_000_000,
        Some(65_536),
    ),
    ("typesafe", "jev-1.13.0", "decision", 42_000_000, None),
];

/// The shipped curated file with nothing held, so these rows are served
/// whatever the shipped file holds; `curated_held_tests` covers holding.
fn served() -> crate::curated_models::Planted {
    crate::curated_models::plant(&crate::curated_models::document_without_held())
}

fn store() -> (CatalogStore, tempfile::TempDir) {
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
    (store, dir)
}

fn try_get(store: &CatalogStore, args: Value) -> Result<CatalogGetResponse, RouteError> {
    serve_catalog_get(store, &serde_json::to_vec(&args).unwrap())
}

fn get(store: &CatalogStore, args: Value) -> CatalogGetResponse {
    try_get(store, args).unwrap()
}

/// Every page of a paged read, one provider per page.
fn paged(store: &CatalogStore) -> Vec<CatalogGetResponse> {
    let mut pages = Vec::new();
    let mut args = json!({"paged": true});
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

/// The facts and disclosure each row must carry wherever it is served.
#[derive(Default)]
struct Served {
    models: BTreeMap<String, BTreeMap<String, Value>>,
    curated: Vec<fusiform_protocol::CuratedModelWire>,
}

impl Served {
    fn add(&mut self, response: CatalogGetResponse) {
        self.models.extend(response.models);
        self.curated.extend(response.curated);
    }
}

fn assert_served(response: &Served, read: &str) {
    for (provider, model, kind, units, context) in ROWS {
        let id = format!("{provider}/{model}");
        let facts = response
            .models
            .get(&id)
            .unwrap_or_else(|| panic!("{read}: {id} must be served"));
        assert_eq!(facts["existence"], "present", "{read}: {id}");
        assert_eq!(facts["model.kind"], *kind, "{read}: {id}");
        assert_eq!(
            facts.get("limit.context").and_then(Value::as_i64),
            *context,
            "{read}: {id}"
        );
        assert_eq!(
            facts["rate.input"],
            json!({"state":"priced", "units":units, "exponent":9, "currency":"USD",
                   "unit_provenance":{"kind":"stated"}}),
            "{read}: {id}"
        );
        assert_eq!(
            facts["rate.output"],
            json!({"state":"stated_zero"}),
            "{read}: {id}"
        );
        let disclosure = response
            .curated
            .iter()
            .find(|row| row.model == id)
            .unwrap_or_else(|| panic!("{read}: {id} must be disclosed as curated"));
        assert_eq!(disclosure.auth_methods, ["apikey"], "{read}: {id}");
        assert!(disclosure.source_ref.contains("https://"), "{read}: {id}");
    }
}

#[test]
fn system_one_rows_are_served_on_bulk_named_provider_and_paged_reads() {
    let _planted = served();
    let (store, _dir) = store();
    let mut bulk = Served::default();
    bulk.add(get(&store, json!({})));
    assert_served(&bulk, "bulk");

    let mut named = Served::default();
    let mut by_provider = Served::default();
    let mut plane = Served::default();
    for (provider, model, ..) in ROWS {
        let response = get(&store, json!({"provider_id":provider, "model_id":model}));
        assert_eq!(response.models.len(), 1, "a named read serves one row");
        named.add(response);
        by_provider.add(get(&store, json!({"provider_id":provider})));
        plane.add(get(
            &store,
            json!({"provider_id":provider, "auth_method":"apikey"}),
        ));
    }
    assert_served(&named, "named");
    assert_served(&by_provider, "provider-filtered");
    assert_served(&plane, "apikey plane");

    let pages = paged(&store);
    assert!(pages.len() > 1, "the budget must split the read into pages");
    let mut all = Served::default();
    for page in pages {
        all.add(page);
    }
    assert_served(&all, "paged");
}

#[test]
fn fact_prefixes_select_the_kind_and_the_rates_separately() {
    let _planted = served();
    let (store, _dir) = store();
    for (provider, model, kind, ..) in ROWS {
        let id = format!("{provider}/{model}");
        for args in [
            json!({"provider_id":provider, "model_id":model, "fact_prefixes":["model."]}),
            json!({"provider_id":provider, "fact_prefixes":["model."]}),
            json!({"fact_prefixes":["model."]}),
        ] {
            let response = get(&store, args.clone());
            let facts = &response.models[&id];
            assert_eq!(facts.len(), 2, "{args}: {facts:?}");
            assert_eq!(facts["model.kind"], *kind);
            assert_eq!(
                facts["model.name"],
                crate::curated_models::models()[&(provider.to_string(), model.to_string())].facts
                    ["model.name"]
            );
        }
        for args in [
            json!({"provider_id":provider, "model_id":model, "fact_prefixes":["rate."]}),
            json!({"provider_id":provider, "fact_prefixes":["rate."]}),
        ] {
            let response = get(&store, args.clone());
            let keys: Vec<&str> = response.models[&id].keys().map(String::as_str).collect();
            assert_eq!(keys, ["rate.input", "rate.output"], "{args}");
        }
    }
}

#[test]
fn a_stated_zero_output_rate_decodes_as_stated_zero() {
    let _planted = served();
    let (store, _dir) = store();
    let response = get(&store, json!({}));
    for (provider, model, _, units, _) in ROWS {
        let facts = &response.models[&format!("{provider}/{model}")];
        let output: RateValue = serde_json::from_value(facts["rate.output"].clone()).unwrap();
        assert_eq!(
            output,
            RateValue::StatedZero {
                inherited_from: None
            }
        );
        let input: RateValue = serde_json::from_value(facts["rate.input"].clone()).unwrap();
        let RateValue::Priced {
            amount,
            inherited_from: None,
            ..
        } = input
        else {
            panic!("{provider}/{model}: rate.input must decode as this row's own price");
        };
        assert_eq!(
            (amount.units, amount.exponent, amount.currency.as_str()),
            (*units, 9, "USD")
        );
        assert_eq!(amount.unit_provenance, UnitProvenance::Stated);
    }
}

/// With no history of the curated file recorded, a point-in-time read
/// serves none of these rows; `curated_history_tests` covers the rows once
/// a boot has recorded them.
#[test]
fn point_in_time_reads_serve_no_unrecorded_system_one_row() {
    let _planted = served();
    let (store, _dir) = store();
    // A point-in-time read needs a record to read against.
    store
        .append_eras(&[fusiform_store::NewEra {
            source: SourceId::ModelsDev,
            provider_id: "meta".into(),
            model_id: "ordinary".into(),
            fact_key: FactKey::from_stored("existence".into()),
            value_json: json!("present").to_string(),
            boundary_at: Timestamp(1000),
            boundary_kind: fusiform_core::BoundaryKind::Seed,
            observation_id: None,
        }])
        .unwrap();
    let response = get(&store, json!({"at_ms":1000}));
    assert!(response.curated.is_empty());
    for (provider, model, ..) in ROWS {
        let id = format!("{provider}/{model}");
        assert!(!response.models.contains_key(&id));
        // A provider filter at an instant answers empty, as it does for
        // meta, rather than refusing a provider whose current rows serve.
        let response = get(&store, json!({"provider_id":provider, "at_ms":1000}));
        assert!(
            response.models.is_empty() && response.curated.is_empty(),
            "{id}"
        );
        let refused = try_get(
            &store,
            json!({"provider_id":provider, "model_id":model, "at_ms":1000}),
        )
        .unwrap_err();
        assert_eq!(refused.code, fusiform_protocol::CODE_NO_COVERAGE, "{id}");
    }
}

#[test]
fn an_unknown_typesafe_model_is_refused_as_an_unknown_model() {
    let _planted = served();
    let (store, _dir) = store();
    let response = get(
        &store,
        json!({"provider_id":"typesafe", "model_id":"jev-1.13.0"}),
    );
    assert_eq!(
        response.models["typesafe/jev-1.13.0"]["model.kind"],
        "decision"
    );
    // The alias page's moving name is not served: only versioned ids are.
    for model in ["jev-latest", "jev-9.9.9"] {
        let refused =
            try_get(&store, json!({"provider_id":"typesafe", "model_id":model})).unwrap_err();
        assert_eq!(refused.code, fusiform_protocol::CODE_NO_COVERAGE);
        assert!(
            refused.message.starts_with("unknown model"),
            "typesafe is a served provider, so the refusal names the model: {}",
            refused.message
        );
        let refused = serve_history(
            &store,
            &serde_json::to_vec(
                &json!({"provider_id":"typesafe", "model_id":model, "fact_key":"rate.input"}),
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
    // A curated row this store holds no history for is refused at an
    // instant with that reason, not as an unknown id; the current read
    // prices it. `curated_history_tests` covers the rows once recorded.
    let refused = try_get(
        &store,
        json!({"provider_id":"typesafe", "model_id":"jev-1.13.0", "at_ms":now_ms()}),
    )
    .unwrap_err();
    assert_eq!(refused.code, fusiform_protocol::CODE_NO_COVERAGE);
    assert!(
        refused
            .message
            .contains("is a curated row with no stored history"),
        "{}",
        refused.message
    );
    assert!(
        !refused.message.contains("check the model id"),
        "{}",
        refused.message
    );
    // Control: a wrong id at an instant is still an unknown model.
    let refused = try_get(
        &store,
        json!({"provider_id":"typesafe", "model_id":"jev-9.9.9", "at_ms":now_ms()}),
    )
    .unwrap_err();
    assert!(
        refused.message.starts_with("unknown model"),
        "{}",
        refused.message
    );
    // Control: a provider nobody serves is still refused as a provider.
    let refused = try_get(
        &store,
        json!({"provider_id":"typesafe-ai", "model_id":"jev-1.13.0"}),
    )
    .unwrap_err();
    assert!(
        refused.message.starts_with("unknown provider"),
        "{}",
        refused.message
    );
    let refused = try_get(&store, json!({"provider_id":"typesafe-ai"})).unwrap_err();
    assert!(
        refused.message.starts_with("unknown provider"),
        "{}",
        refused.message
    );
}

#[test]
fn a_curated_history_read_names_the_source_for_a_system_one_row() {
    let _planted = served();
    let (store, _dir) = store();
    for (provider, model, ..) in ROWS {
        let response = serve_history(
            &store,
            &serde_json::to_vec(
                &json!({"provider_id":provider, "model_id":model, "fact_key":"model.kind"}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(response.eras.is_empty());
        assert_eq!(response.curated.unwrap().auth_methods, ["apikey"]);
    }
}
