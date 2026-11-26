use fusiform_store::CatalogStore;
use serde_json::{json, Value};

pub(crate) fn store() -> (CatalogStore, tempfile::TempDir) {
    super::media_catalog::v2_seed_store(&[
        ("openai", "gpt-6.1-sol"),
        ("openai", "gpt-5.5"),
        ("openai", "gpt-4o"),
    ])
}

fn get(store: &CatalogStore, args: Value) -> Value {
    serde_json::to_value(super::curated_wire::get(store, args)).unwrap()
}

#[test]
fn antigravity_login_allowlist_applies_only_to_the_alias() {
    let (store, _dir) = super::media_catalog::v2_seed_store(&[("google", "gemini-4-hydrogen")]);
    let alias = "google/antigravity-gemini-4-hydrogen";
    let plain = "google/gemini-4-hydrogen";
    for method in [None, Some("apikey")] {
        let mut args = json!({"provider_id":"google", "model_id":"antigravity-gemini-4-hydrogen"});
        if let Some(method) = method {
            args["auth_method"] = json!(method);
        }
        let response = get(&store, args);
        if method == Some("apikey") {
            assert_eq!(response["models"], json!({}));
            assert!(response.get("aliased").is_none());
            assert!(response.get("login_methods").is_none());
        } else {
            assert!(response["models"].get(alias).is_some());
            assert_eq!(response["aliased"][0]["model"], alias);
            assert_eq!(response["login_methods"][0]["model"], alias);
            assert_eq!(
                response["login_methods"][0]["auth_methods"],
                json!(["antigravity"])
            );
        }

        let mut args = json!({"provider_id":"google", "model_id":"gemini-4-hydrogen"});
        if let Some(method) = method {
            args["auth_method"] = json!(method);
        }
        let response = get(&store, args);
        assert!(response["models"].get(plain).is_some(), "{method:?}");
        assert!(response.get("login_methods").is_none());
    }
}

#[test]
fn gpt_6_2_models_are_reachable_on_verified_chatgpt_and_apikey_routes() {
    let (store, _dir) = super::cache_lifetimes::store(None);
    for model in ["gpt-6.2-astra", "gpt-6.2-sol"] {
        for (auth, family) in [
            ("apikey", "openai.responses"),
            ("chatgpt", "openai.chatgpt_responses"),
        ] {
            let response = get(
                &store,
                json!({"provider_id":"openai", "model_id":model, "auth_method":auth}),
            );
            let id = format!("openai/{model}");
            assert_eq!(
                response["models"][&id]["wire.family"], family,
                "{id} {auth}"
            );
            assert_eq!(response["login_methods"][0]["model"], id);
            assert_eq!(
                response["login_methods"][0]["auth_methods"],
                json!(["apikey", "chatgpt"])
            );
        }
    }
}

#[test]
fn chatgpt_login_allowlist_discloses_and_refuses_only_established_rows() {
    let (store, _dir) = store();
    let version = store.catalog_version().unwrap();
    let plain = get(&store, json!({"provider_id":"openai"}));
    let rows = plain["login_methods"].as_array().expect("login disclosure");
    assert_eq!(rows.len(), 2);
    assert!(!plain["curated"].as_array().unwrap().iter().any(|row| {
        ["openai/gpt-6.1-sol", "openai/gpt-5.5", "openai/gpt-4o"]
            .contains(&row["model"].as_str().unwrap())
    }));
    for (id, methods) in [
        ("openai/gpt-6.1-sol", json!(["apikey", "chatgpt"])),
        ("openai/gpt-5.5", json!(["apikey"])),
    ] {
        let row = rows.iter().find(|row| row["model"] == id).unwrap();
        assert_eq!(row["auth_methods"], methods);
        assert_eq!(
            row["source_ref"],
            "https://developers.openai.com/codex/models"
        );
        assert!(row["review_by_ms"].as_i64().unwrap() > 0);
        assert_eq!(row.as_object().unwrap().len(), 4);
    }
    let plane = get(
        &store,
        json!({"provider_id":"openai", "auth_method":"chatgpt"}),
    );
    assert!(plane["models"].get("openai/gpt-6.1-sol").is_some());
    assert!(plane["models"].get("openai/gpt-5.5").is_none());
    assert!(plane["models"].get("openai/gpt-4o").is_some());
    assert_eq!(plane["login_methods"].as_array().unwrap().len(), 1);
    let refused = get(
        &store,
        json!({"provider_id":"openai", "model_id":"gpt-5.5", "auth_method":"chatgpt"}),
    );
    assert_eq!(refused["models"], json!({}));
    assert!(refused.get("login_methods").is_none());
    let api = get(
        &store,
        json!({"provider_id":"openai", "model_id":"gpt-5.5", "auth_method":"apikey"}),
    );
    assert!(api["models"].get("openai/gpt-5.5").is_some());
    let sol = get(
        &store,
        json!({"provider_id":"openai", "model_id":"gpt-6.1-sol", "auth_method":"chatgpt"}),
    );
    assert!(sol["models"].get("openai/gpt-6.1-sol").is_some());
    let unlisted = get(&store, json!({"provider_id":"openai", "model_id":"gpt-4o"}));
    assert!(unlisted.get("login_methods").is_none());
    assert_eq!(store.catalog_version().unwrap(), version);
}

#[test]
fn login_disclosure_survives_fact_filters_and_paging_but_not_historical_reads() {
    let (store, _dir) = store();
    for args in [
        json!({"provider_id":"openai", "model_id":"gpt-6.1-sol", "fact_prefixes":["rate."]}),
        json!({"provider_id":"openai", "model_id":"gpt-6.1-sol", "fact_prefixes":["missing."]}),
        json!({"provider_id":"openai", "auth_method":"chatgpt", "fact_prefixes":["rate."]}),
    ] {
        let response = get(&store, args);
        assert_eq!(response["login_methods"][0]["model"], "openai/gpt-6.1-sol");
    }
    let page = get(&store, json!({"paged":true, "fact_prefixes":["rate."]}));
    assert_eq!(page["login_methods"].as_array().unwrap().len(), 2);
    for row in page["login_methods"].as_array().unwrap() {
        assert!(page["models"].get(row["model"].as_str().unwrap()).is_some());
    }
    let historical = get(
        &store,
        json!({"provider_id":"openai", "model_id":"gpt-5.5", "at_ms":1000}),
    );
    assert!(historical["models"].get("openai/gpt-5.5").is_some());
    assert!(historical.get("login_methods").is_none());
}
