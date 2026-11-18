//! The embedded aliases document and its local adapter.

pub use fusiform_curate::aliases::*;
use std::collections::BTreeMap;

const ALIASES: &str = include_str!("../data/model-aliases.json");

/// The shipped table, parsed once per process.
///
/// Embedded, so a re-parse on every catalog read would cost the same answer
/// repeatedly.
pub fn aliases() -> &'static Aliases {
    static PARSED: std::sync::OnceLock<Aliases> = std::sync::OnceLock::new();
    PARSED.get_or_init(|| load_reporting_rejects(ALIASES).0)
}

/// The shipped retired-alias list, parsed once per process.
pub fn retired_aliases() -> &'static BTreeMap<(String, String), RetiredAlias> {
    static PARSED: std::sync::OnceLock<BTreeMap<(String, String), RetiredAlias>> =
        std::sync::OnceLock::new();
    PARSED.get_or_init(|| load_retired(ALIASES))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = include_str!("../data/models-dev-seed.json");

    /// Ids read and deliberately NOT aliased, with the reason.
    ///
    /// Kept beside the loader so the next person reading the routing code
    /// sees what was already ruled out and why, and so a row for one of these
    /// cannot be added without deleting its reason first.
    const DECLARED_EXCLUSIONS: &[(&str, &str, &str)] = &[
        (
            "google",
            "antigravity-gemini-3.5-flash",
            "GEMINI_35_FLASH_ROUTES sends the high tier (and the default model) to \
             gemini-3-flash-agent, a different model from gemini-3.5-flash",
        ),
        (
            "google",
            "antigravity-gemini-3.1-pro",
            "models.dev publishes no google/gemini-3.1-pro row to alias to, and the \
             high tier routes to gemini-pro-agent",
        ),
        (
            "google",
            "antigravity-gpt-oss-120b-medium",
            "models.dev publishes no openai/gpt-oss-120b row, and the id fixes \
             effort at medium, so the target's reasoning options would be false \
             for it",
        ),
        // Withdrawn server-side: two fetchAvailableModels reads on the daily
        // Code Assist endpoint, just before 2026-10-14T13:11Z, listed none of
        // these routes, and antigravity-auth@48919aa7 no longer defines the ids.
        (
            "google",
            "antigravity-gemini-3.7-flash",
            "Google withdrew the gemini-3.7-flash-low/-medium/-high routes \
             (absent from fetchAvailableModels on 2026-10-14), so the alias \
             would price a route that returns 404",
        ),
        (
            "google",
            "antigravity-gemini-3.6-flash",
            "Google withdrew the gemini-3.6-flash-low/-medium/-high routes \
             (absent from fetchAvailableModels on 2026-10-14), so the alias \
             would price a route that returns 404",
        ),
        (
            "google",
            "antigravity-claude-sonnet-4-6-thinking",
            "no claude-sonnet-4-6 route is served (absent from \
             fetchAvailableModels on 2026-10-14), and the plugin now defines \
             antigravity-claude-sonnet-5-5 instead",
        ),
        (
            "google",
            "antigravity-claude-opus-4-6-thinking",
            "the resolver has no RESOLVER_ALIASES entry for claude-opus-4-6-thinking, \
             so every tier reaches the claude-opus-4-6-thinking backend rather than \
             claude-opus-4-6; the routing code does not establish they are one model",
        ),
    ];

    fn seed_has(provider_id: &str, model_id: &str) -> bool {
        let seed: serde_json::Value = serde_json::from_str(SEED).expect("the seed parses");
        seed.get(provider_id)
            .and_then(|p| p.get("models"))
            .and_then(|m| m.get(model_id))
            .is_some()
    }

    #[test]
    fn the_shipped_table_loads_without_rejects() {
        let (aliases, rejects) = load_reporting_rejects(ALIASES);
        assert!(
            rejects.is_empty(),
            "the shipped alias table must parse cleanly: {rejects:?}"
        );
        assert!(
            !aliases.is_empty(),
            "an empty table would make every assertion below vacuous"
        );
        assert_eq!(
            aliases
                .get(&(
                    "google".to_string(),
                    "antigravity-gemini-3.8-flash".to_string()
                ))
                .map(|a| (a.target_provider_id.as_str(), a.target_model_id.as_str())),
            Some(("google", "gemini-3.8-flash")),
            "the alias the table exists for must resolve"
        );
    }

    /// Every target is a row the embedded seed publishes.
    ///
    /// An alias to a row that does not exist serves nothing, so a typo in a
    /// target would look exactly like a model the catalog does not cover.
    #[test]
    fn every_alias_target_exists_in_the_embedded_seed() {
        let aliases = aliases();
        let missing: Vec<String> = aliases
            .values()
            .filter(|a| !seed_has(&a.target_provider_id, &a.target_model_id))
            .map(|a| format!("{}/{}", a.target_provider_id, a.target_model_id))
            .collect();
        assert!(
            missing.is_empty(),
            "alias targets absent from the embedded models.dev seed: {missing:?}"
        );
    }

    /// No alias id is a row the embedded seed publishes.
    ///
    /// The serve path lets a real row win, so an alias shadowing a published
    /// id would be dead at runtime and would still read, in the file, as a
    /// claim about identity. Either the alias is wrong or it is redundant, and
    /// both are curation errors.
    #[test]
    fn no_alias_id_exists_in_the_embedded_seed() {
        let shadowing: Vec<String> = aliases()
            .keys()
            .filter(|(p, m)| seed_has(p, m))
            .map(|(p, m)| format!("{p}/{m}"))
            .collect();
        assert!(
            shadowing.is_empty(),
            "alias ids that models.dev publishes as real rows: {shadowing:?}"
        );
    }

    /// The fence above can fail: a seed row is found when it exists.
    #[test]
    fn the_seed_lookup_finds_a_row_that_exists() {
        assert!(seed_has("google", "gemini-3.8-flash"));
        assert!(!seed_has("google", "antigravity-gemini-3.8-flash"));
    }

    /// Every shipped retired id parses, and none is also a live alias: an id
    /// cannot be both served and reported absent.
    #[test]
    fn retired_aliases_parse_and_are_not_live() {
        let retired = retired_aliases();
        assert_eq!(
            retired.len(),
            3,
            "the three Code Assist ids withdrawn on 2026-10-14"
        );
        let live = aliases();
        for key in retired.keys() {
            assert!(!live.contains_key(key), "{key:?} is both live and retired");
        }
    }

    #[test]
    fn tier_efforts_agree_with_served_reasoning_options() {
        let seed = serde_json::from_str(SEED).unwrap();
        assert!(reasoning_option_drift(aliases(), &seed).is_empty());
    }

    #[test]
    fn planted_tier_effort_drift_is_reported() {
        let mut rows = aliases().clone();
        rows.get_mut(&("google".into(), "antigravity-gemini-3.8-flash".into()))
            .unwrap()
            .reasoning_wire_ids
            .insert("minimal".into(), "not-supported".into());
        let seed = serde_json::from_str(SEED).unwrap();
        let drift = reasoning_option_drift(&rows, &seed);
        assert_eq!(drift.len(), 1);
        assert!(drift[0].contains("minimal"));
        assert!(drift[0].contains("antigravity-gemini-3.8-flash"));
    }

    #[test]
    fn malformed_output_limits_are_rejected_and_counted() {
        let valid = serde_json::json!({"value":262144,"source_ref":"pinned capture","quote":"maxOutputTokens: 262144"});
        let mut doc: serde_json::Value = serde_json::from_str(ALIASES).unwrap();
        let count = doc["aliases"].as_array().unwrap().len();
        doc["aliases"][0]["limit_output"] = valid.clone();
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.len(), count);
        for malformed in [
            serde_json::json!({"value":0,"source_ref":"source","quote":"quote"}),
            serde_json::json!({"value":-1,"source_ref":"source","quote":"quote"}),
            serde_json::json!({"value":1.5,"source_ref":"source","quote":"quote"}),
            serde_json::json!({"value":1,"source_ref":" ","quote":"quote"}),
            serde_json::json!({"value":1,"source_ref":"source","quote":""}),
            serde_json::json!({"value":1,"source_ref":"source"}),
            serde_json::json!({"value":1,"source_ref":"source","quote":"quote","unknown":true}),
        ] {
            doc["aliases"][0]["limit_output"] = malformed.clone();
            let (rows, rejects) = load_reporting_rejects(&doc.to_string());
            assert_eq!(rows.len(), count - 1, "{malformed}");
            assert_eq!(rejects.len(), 1, "{malformed}: {rejects:?}");
            assert!(!rows.contains_key(&("google".into(), "antigravity-claude-sonnet-5-5".into())));
        }
    }

    #[test]
    fn invalid_tier_defaults_are_rejected() {
        for (map, default) in [
            (serde_json::json!({}), serde_json::json!("medium")),
            (serde_json::json!({"low":"wire"}), serde_json::Value::Null),
            (
                serde_json::json!({"low":"wire"}),
                serde_json::json!("medium"),
            ),
            (serde_json::json!({"low":""}), serde_json::json!("low")),
        ] {
            let mut doc: serde_json::Value = serde_json::from_str(ALIASES).unwrap();
            doc["aliases"][0]["reasoning_wire_ids"] = map;
            doc["aliases"][0]["reasoning_default"] = default;
            let (_, rejects) = load_reporting_rejects(&doc.to_string());
            assert_eq!(rejects.len(), 1);
            assert!(rejects[0].contains("invalid reasoning"));
        }
    }

    fn currently_served() -> std::collections::BTreeSet<String> {
        use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
        use fusiform_core::{BoundaryKind, Timestamp};
        let dir = tempfile::tempdir().unwrap();
        let store = fusiform_store::CatalogStore::open(&StorageDescriptor {
            module_id: "fusiform".into(),
            storage_namespace: "default".into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.path().join("store.db").to_string_lossy().to_string(),
            },
        })
        .unwrap();
        let catalog = fusiform_core::normalize_models_dev(SEED.as_bytes())
            .unwrap()
            .catalog;
        let plan = fusiform_store::ingest::plan_ingest(
            &store,
            &catalog,
            Timestamp(1_000),
            BoundaryKind::Seed,
            None,
        )
        .unwrap();
        store.append_eras(&plan.eras).unwrap();
        let response = crate::route::serve_catalog_get(&store, br#"{"paged":true}"#).unwrap();
        let mut ids: std::collections::BTreeSet<_> = response.models.into_keys().collect();
        let mut token = response.next_page_token;
        while let Some(next) = token {
            let request = serde_json::json!({"paged":true,"page_token":next});
            let page =
                crate::route::serve_catalog_get(&store, &serde_json::to_vec(&request).unwrap())
                    .unwrap();
            ids.extend(page.models.into_keys());
            token = page.next_page_token;
        }
        ids
    }

    #[test]
    fn successors_name_currently_served_rows() {
        let invalid = invalid_successors(retired_aliases(), &currently_served());
        assert!(invalid.is_empty(), "{invalid:?}");
    }

    #[test]
    fn planted_missing_or_unsourced_successor_is_reported() {
        let mut retired = retired_aliases().clone();
        let row = retired
            .get_mut(&("google".into(), "antigravity-gemini-3.7-flash".into()))
            .unwrap();
        row.successor = Some("google/not-served".into());
        let served = currently_served();
        assert_eq!(invalid_successors(&retired, &served).len(), 1);
        let row = retired
            .get_mut(&("google".into(), "antigravity-gemini-3.7-flash".into()))
            .unwrap();
        row.successor = Some("google/antigravity-gemini-3.8-flash".into());
        row.successor_source_ref = None;
        assert_eq!(invalid_successors(&retired, &served).len(), 1);
    }

    #[test]
    fn no_declared_exclusion_is_in_the_table() {
        for (p, m, why) in DECLARED_EXCLUSIONS {
            assert!(
                !aliases().contains_key(&(p.to_string(), m.to_string())),
                "{p}/{m} was ruled out ({why}); remove it from the exclusions \
                 with a reason before adding it to the table"
            );
        }
    }

    #[test]
    fn a_duplicate_alias_id_is_reported_rather_than_resolved() {
        let row = |target: &str| {
            format!(
                r#"{{"provider_id":"google","model_id":"x","target":{{"provider_id":"google","model_id":"{target}"}},
                "basis":"b","source_ref":"s","established_at_ms":1,"review_by_ms":2}}"#
            )
        };
        let doc = format!(r#"{{"aliases":[{},{}]}}"#, row("a"), row("b"));
        let (aliases, rejects) = load_reporting_rejects(&doc);
        assert_eq!(
            rejects.len(),
            1,
            "the duplicate must be reported: {rejects:?}"
        );
        assert!(rejects[0].contains("appears twice"), "{rejects:?}");
        assert_eq!(aliases.len(), 1);
    }

    #[test]
    fn a_malformed_row_is_counted_not_fatal() {
        let doc = r#"{"aliases":[
            {"provider_id":"google","model_id":"x","target":{"provider_id":"google","model_id":"y"},
             "basis":"b","source_ref":"s","established_at_ms":1,"review_by_ms":2},
            {"provider_id":"google","model_id":"z"}
        ]}"#;
        let (aliases, rejects) = load_reporting_rejects(doc);
        assert_eq!(aliases.len(), 1, "the good row still loads");
        assert_eq!(rejects.len(), 1, "the bad row is reported: {rejects:?}");
    }

    #[test]
    fn a_chained_alias_is_refused() {
        let doc = r#"{"aliases":[
            {"provider_id":"google","model_id":"a","target":{"provider_id":"google","model_id":"b"},
             "basis":"x","source_ref":"s","established_at_ms":1,"review_by_ms":2},
            {"provider_id":"google","model_id":"b","target":{"provider_id":"google","model_id":"c"},
             "basis":"x","source_ref":"s","established_at_ms":1,"review_by_ms":2}
        ]}"#;
        let (aliases, rejects) = load_reporting_rejects(doc);
        assert!(!aliases.contains_key(&("google".to_string(), "a".to_string())));
        assert_eq!(rejects.len(), 1, "{rejects:?}");
    }

    /// The review gate on the shipped table.
    ///
    /// Time-triggered: it fails on a day nobody changed anything. The message
    /// says so, and says what to do.
    #[test]
    fn no_alias_is_past_its_review_date() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_millis() as i64;
        let overdue = overdue_at(aliases(), now);
        assert!(overdue.is_empty(), "\n\n{}", overdue_report(&overdue));
    }

    /// The gate fires, and its message names the row and what to do.
    #[test]
    fn an_overdue_alias_fires_and_the_message_is_actionable() {
        let doc = r#"{"aliases":[{"provider_id":"google","model_id":"x",
            "target":{"provider_id":"google","model_id":"y"},"basis":"b",
            "source_ref":"repo@abc file.ts","established_at_ms":1,"review_by_ms":1000}]}"#;
        let (aliases, rejects) = load_reporting_rejects(doc);
        assert!(rejects.is_empty(), "{rejects:?}");

        let overdue = overdue_at(&aliases, 1000 + 86_400_000 * 3);
        assert_eq!(overdue.len(), 1);
        assert_eq!(overdue[0].days_overdue, 3);
        let msg = overdue_report(&overdue);
        assert!(msg.contains("NOT A BUILD FAILURE"), "{msg}");
        assert!(msg.contains("google/x -> google/y"), "{msg}");
        assert!(msg.contains("3 day(s)"), "{msg}");
        assert!(msg.contains("repo@abc file.ts"), "{msg}");
        assert!(msg.contains("CANNOT tell you"), "{msg}");

        // CONTROL: a row inside its window does not fire.
        assert!(overdue_at(&aliases, 999).is_empty());
    }
}
