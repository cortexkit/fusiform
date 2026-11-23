//! The embedded fact overrides document and its local adapter.

pub use fusiform_curate::fact_overrides::*;
use std::collections::BTreeMap;

const DOCUMENT: &str = include_str!("../data/fact-overrides.json");

#[cfg(test)]
const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;
pub fn overrides() -> &'static FactOverrides {
    static ROWS: std::sync::OnceLock<FactOverrides> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

/// Record the compiled claims on every boot, independently of upstream polls.
pub fn apply_to_store(
    store: &fusiform_store::CatalogStore,
    rows: &FactOverrides,
    boot_at_ms: i64,
) -> Result<usize, fusiform_store::CatalogError> {
    let rows: Vec<_> = rows
        .values()
        .map(|r| fusiform_store::NewFactOverride {
            provider_id: r.provider_id.clone(),
            model_id: r.model_id.clone(),
            fact_key: r.fact_key.clone(),
            value: r.value.clone(),
            replaces: r.replaces.clone(),
            reason: r.reason.clone(),
            source_ref: r.source_ref.clone(),
            quote: r.quote.clone(),
            notes: r.notes.clone(),
            established_by: r.established_by.clone(),
            established_at_ms: r.established_at_ms,
            review_by_ms: r.review_by_ms,
        })
        .collect();
    store.append_fact_overrides(&rows, boot_at_ms)
}

/// Load the claims in force, dropping tombstones rather than falling back to
/// the compiled file: a removed override must stay removed on historical reads.
pub fn at(
    store: &fusiform_store::CatalogStore,
    at_ms: i64,
    provider_id: Option<&str>,
) -> Result<FactOverrides, fusiform_store::CatalogError> {
    Ok(from_eras(store.fact_overrides_at(at_ms, provider_id)?))
}

pub(crate) fn from_eras(
    eras: impl IntoIterator<Item = fusiform_store::FactOverrideEra>,
) -> FactOverrides {
    eras.into_iter()
        .filter_map(|era| {
            let r = era.claim?;
            Some((
                (
                    r.provider_id.clone(),
                    r.model_id.clone(),
                    r.fact_key.clone(),
                ),
                FactOverride {
                    provider_id: r.provider_id,
                    model_id: r.model_id,
                    fact_key: r.fact_key,
                    value: r.value,
                    replaces: r.replaces,
                    reason: r.reason,
                    source_ref: r.source_ref,
                    quote: r.quote,
                    notes: r.notes,
                    established_by: r.established_by,
                    established_at_ms: r.established_at_ms,
                    review_by_ms: r.review_by_ms,
                },
            ))
        })
        .collect()
}

/// Rows the embedded models.dev seed would not let apply.
///
/// The seed is normalized exactly as ingest stores it, so this asks the same
/// question `apply` asks of a stored row. A replacement is stale when the
/// seed's stored value is not its `replaces`; an addition is stale when the
/// seed's row carries the fact at all, which means models.dev now publishes
/// it and upstream wins. Either way, and when the model is missing from the
/// seed, a store seeded from that copy would never apply the row: upstream has
/// moved (re-source the override or remove it) or the row was written against
/// the wrong value.
pub fn stale_against_seed(rows: &FactOverrides, seed: &serde_json::Value) -> Vec<String> {
    let bytes = serde_json::to_vec(seed).expect("a JSON value serializes");
    let catalog = match fusiform_core::normalize::normalize_models_dev(&bytes) {
        Ok(outcome) => outcome.catalog,
        Err(error) => return vec![format!("the embedded seed does not normalize: {error:?}")],
    };
    let mut stored: BTreeMap<(&str, &str), BTreeMap<String, serde_json::Value>> = BTreeMap::new();
    for model in catalog.models() {
        let facts = fusiform_store::ingest::facts_with_values(model)
            .into_iter()
            .map(|(key, raw)| {
                let value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw));
                (key.as_str().to_string(), value)
            })
            .collect();
        stored.insert((&model.key.provider_id, &model.key.model_id), facts);
    }
    rows.values()
        .filter_map(|row| {
            stale_row(
                row,
                stored.get(&(row.provider_id.as_str(), row.model_id.as_str())),
            )
        })
        .collect()
}

/// Apply every shipped row for this model that upstream still lets apply,
/// returning one disclosure per applied row.
///
/// `withheld` names this model's facts that a correction keeps out of the
/// read. They are absent from `facts`, and an addition must not mistake that
/// absence for "upstream publishes nothing" and serve a value over the
/// withholding.
pub fn apply(
    provider_id: &str,
    model_id: &str,
    facts: &mut BTreeMap<String, serde_json::Value>,
    prefixes: Option<&Vec<String>>,
    withheld: &[&str],
) -> Vec<fusiform_protocol::FactOverrideWire> {
    apply_rows(
        overrides(),
        provider_id,
        model_id,
        facts,
        prefixes,
        withheld,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> serde_json::Value {
        serde_json::from_str(DOCUMENT).unwrap()
    }

    fn seed() -> serde_json::Value {
        serde_json::from_str(include_str!("../data/models-dev-seed.json")).unwrap()
    }

    fn argon() -> &'static FactOverride {
        &overrides()[&(
            "google".to_string(),
            "gemini-4-argon".to_string(),
            "limit.output".to_string(),
        )]
    }

    fn fallback(model_id: &str) -> Option<&'static FactOverride> {
        overrides().get(&(
            "anthropic".to_string(),
            model_id.to_string(),
            "capability.server_side_fallback".to_string(),
        ))
    }

    /// The first addition row in the shipped document, for planted variants.
    fn addition_row() -> serde_json::Value {
        document()["overrides"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["fact_key"] == "capability.server_side_fallback")
            .expect("the document ships an addition row")
            .clone()
    }

    #[test]
    fn shipped_overrides_load_without_rejects() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.len(), 104);
        let row = argon();
        assert_eq!(row.value, json!(65_536));
        assert_eq!(row.replaces, json!(32_768));
        // The alias inherits this value, so the row cites the route's own
        // declaration of it.
        assert!(row.notes.as_ref().unwrap().contains(
            "cortexkit/antigravity-auth/blob/48919aa7ae9378ea19af17e7d9da5b986395f00a/packages/core/src/model-registry.ts"
        ));
    }

    #[test]
    fn temperature_overrides_require_different_published_booleans() {
        let original = document()["overrides"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["fact_key"] == "capability.temperature")
            .unwrap()
            .clone();
        for (field, value) in [
            ("replaces", json!(null)),
            ("value", json!(null)),
            ("value", json!(true)),
            ("replaces", json!(1)),
        ] {
            let mut row = original.clone();
            row[field] = value;
            let (rows, rejects) = load_reporting_rejects(&json!({"overrides":[row]}).to_string());
            assert!(rows.is_empty(), "{field}");
            assert_eq!(rejects.len(), 1, "{field}");
        }
    }

    #[test]
    fn image_rate_rows_validate_shape_and_yield_to_upstream_or_withholding() {
        let rows = overrides();
        assert!(!rows.contains_key(&(
            "google".into(),
            "gemini-3.1-flash-image".into(),
            "rate.output".into(),
        )));
        let image = &rows[&(
            "google".into(),
            "gemini-3.1-flash-image".into(),
            "rate.output_image".into(),
        )];
        {
            let row = image;
            let mut raw = document()["overrides"]
                .as_array()
                .unwrap()
                .iter()
                .find(|raw| raw["fact_key"] == row.fact_key && raw["model_id"] == row.model_id)
                .unwrap()
                .clone();
            raw["value"]["per"] = json!(1000);
            assert_eq!(
                load_reporting_rejects(&json!({"overrides":[raw]}).to_string())
                    .1
                    .len(),
                1
            );
        }
        let mut facts = BTreeMap::from([
            (
                "rate.output".into(),
                json!({"state":"priced", "units":3_000_000_000i64,
                "exponent":9, "currency":"USD"}),
            ),
            ("rate.output_image".into(), json!(null)),
        ]);
        let applied = apply_rows(
            rows,
            "google",
            "gemini-3.1-flash-image",
            &mut facts,
            None,
            &[],
        );
        assert!(applied.is_empty());
        assert_eq!(facts["rate.output"]["units"], 3_000_000_000i64);
        assert!(facts["rate.output_image"].is_null());
        facts.remove("rate.output_image");
        let applied = apply_rows(
            rows,
            "google",
            "gemini-3.1-flash-image",
            &mut facts,
            None,
            &["rate.output_image"],
        );
        assert!(applied.is_empty());
        assert!(!facts.contains_key("rate.output_image"));
    }

    #[test]
    fn server_side_fallback_is_served_as_anthropic_documents_it() {
        const PAGE: &str =
            "https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback";
        const EXCLUSION: &str = "Claude Haiku 5.5 has no server-side fallback: with `fallbacks: \"default\"`, a declined request stays declined";
        for model in [
            "claude-fable-5",
            "claude-fable-5-1",
            "claude-fable-5-2",
            "claude-opus-5",
            "claude-opus-5-5",
        ] {
            let row = fallback(model).unwrap_or_else(|| panic!("{model} has a row"));
            assert_eq!(row.value, json!(true), "{model}");
            assert!(row.replaces.is_null(), "{model} is an addition");
            assert!(row.source_ref.contains(PAGE), "{model}");
            assert!(
                row.quote.contains("server-side-fallback-2026-07-01"),
                "{model}"
            );
            // Haiku 5.5 is the page's only exclusion, so every true row quotes it.
            assert!(row.quote.contains(EXCLUSION), "{model}");
        }
        let haiku = fallback("claude-haiku-5-5").expect("Haiku 5.5 has a row");
        assert_eq!(haiku.value, json!(false));
        assert!(haiku.replaces.is_null());
        assert!(haiku.quote.contains(EXCLUSION));
        // Anthropic's page shows Sonnet 5.5 falling back only when a request
        // names its own fallback models, never with `fallbacks: "default"`,
        // and anthropic-auth marks it false, so no value is served for it.
        assert!(fallback("claude-sonnet-5-5").is_none());
    }

    #[test]
    fn fact_overrides_are_within_their_review_dates() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let overdue = overdue_at(overrides(), now);
        assert!(overdue.is_empty(), "{}", overdue.join("\n"));
    }

    #[test]
    fn mid_conversation_system_rows_match_the_documented_models() {
        let expected = BTreeMap::from([
            ("claude-fable-5-2", true),
            ("claude-fable-5-1", true),
            ("claude-fable-5", true),
            ("claude-opus-5-5", true),
            ("claude-opus-4-8", true),
            ("claude-opus-5", true),
            ("claude-sonnet-5-5", true),
            ("claude-haiku-5-5", true),
            ("claude-sonnet-5", false),
            ("claude-opus-4-7", false),
        ]);
        let rows: Vec<_> = overrides()
            .values()
            .filter(|row| row.fact_key == "capability.mid_conversation_system")
            .collect();
        assert_eq!(rows.len(), expected.len());
        for row in rows {
            assert_eq!(row.provider_id, "anthropic");
            assert_eq!(row.value, json!(expected[row.model_id.as_str()]));
            assert!(row.replaces.is_null());
            assert_eq!(row.review_by_ms - row.established_at_ms, REVIEW_INTERVAL_MS);
            assert!(row
                .notes
                .as_ref()
                .unwrap()
                .contains("not the beta turn-scoped clear_at variant"));
        }
    }

    #[test]
    fn planted_overdue_override_fails_the_review_gate() {
        let row = argon().clone();
        let review = row.review_by_ms;
        let rows = FactOverrides::from([(
            (
                row.provider_id.clone(),
                row.model_id.clone(),
                row.fact_key.clone(),
            ),
            row,
        )]);
        assert!(overdue_at(&rows, review).is_empty());
        let overdue = overdue_at(&rows, review + 1);
        assert_eq!(overdue.len(), 1);
        assert!(overdue[0].contains("google/gemini-4-argon limit.output"));
        assert!(overdue[0].contains("re-verify"));
        assert!(overdue[0].contains("or remove"));
    }

    #[test]
    fn overrides_replace_what_the_embedded_seed_publishes() {
        let stale = stale_against_seed(overrides(), &seed());
        assert!(stale.is_empty(), "{}", stale.join("\n"));
    }

    #[test]
    fn planted_upstream_move_reports_the_stale_override() {
        let mut seed = seed();
        seed["google"]["models"]["gemini-4-argon"]["limit"]["output"] = json!(65_536);
        let stale = stale_against_seed(overrides(), &seed);
        assert_eq!(stale.len(), 1);
        assert!(stale[0].contains("google/gemini-4-argon limit.output"));
        assert!(stale[0].contains("publishes 65536"));
    }

    /// An addition is stale once the seed's stored row carries the fact.
    ///
    /// models.dev has no such field today, so no edit to the seed can make
    /// ingest store one; the stored facts are planted directly instead, which
    /// is what the gate checks.
    #[test]
    fn planted_upstream_publication_reports_the_stale_addition() {
        let row = fallback("claude-opus-5-5").unwrap();
        let control = BTreeMap::new();
        assert!(stale_row(row, Some(&control)).is_none());
        for published in [json!(false), json!(true), json!(null)] {
            let stored = BTreeMap::from([(row.fact_key.clone(), published.clone())]);
            let stale = stale_row(row, Some(&stored)).expect("upstream wins");
            assert!(stale.contains("anthropic/claude-opus-5-5 capability.server_side_fallback"));
            assert!(stale.contains(&format!("publishes {published}")), "{stale}");
            assert!(stale.contains("not no value for this fact"), "{stale}");
        }
        // A model the seed does not carry can never receive the addition.
        let missing = stale_row(row, None).expect("missing model");
        assert!(missing.contains("does not carry this model"));
        // And through the real gate, a model removed from the seed is named.
        let mut seed = seed();
        seed["anthropic"]["models"]
            .as_object_mut()
            .unwrap()
            .remove("claude-haiku-5-5");
        let stale = stale_against_seed(overrides(), &seed);
        let haiku_rows = overrides()
            .values()
            .filter(|row| row.provider_id == "anthropic" && row.model_id == "claude-haiku-5-5")
            .count();
        assert_eq!(stale.len(), haiku_rows, "{stale:?}");
        for row in overrides()
            .values()
            .filter(|row| row.provider_id == "anthropic" && row.model_id == "claude-haiku-5-5")
        {
            let key = &row.fact_key;
            assert!(
                stale
                    .iter()
                    .any(|problem| problem.contains(&format!("anthropic/claude-haiku-5-5 {key}"))),
                "{stale:?}"
            );
        }
    }

    #[test]
    fn malformed_and_duplicate_rows_are_counted_as_rejects() {
        let mut doc = document();
        let row = doc["overrides"][0].clone();
        doc["overrides"]
            .as_array_mut()
            .unwrap()
            .extend([row, json!({})]);
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rows.len(), overrides().len());
        assert_eq!(rejects.len(), 2);
        assert!(rejects[0].contains("duplicate"));
        assert!(rejects[1].contains("did not parse"));
        assert_eq!(load_reporting_rejects("not json").1.len(), 1);
    }

    #[test]
    fn malformed_fields_are_rejected() {
        for (field, value, reason) in [
            ("provider_id", json!(" "), "empty identity"),
            ("model_id", json!(""), "empty identity"),
            ("reason", json!(""), "empty identity"),
            ("source_ref", json!(""), "empty identity"),
            ("quote", json!(" "), "empty identity"),
            ("notes", json!(" "), "notes"),
            ("fact_key", json!("rate.input"), "only limit"),
            ("fact_key", json!("wire.api"), "only limit"),
            ("value", json!(null), "positive integers"),
            ("value", json!("65536"), "positive integers"),
            ("replaces", json!(0), "positive integers"),
            ("value", json!(32_768), "must differ"),
            ("established_by", json!("upstream"), "established_by"),
            ("established_at_ms", json!(0), "review_by_ms"),
            ("review_by_ms", json!(1), "review_by_ms"),
            ("unknown", json!(true), "did not parse"),
            // A limit is stored as null when unpublished, so a limit addition
            // could only ever fill a withheld fact.
            ("replaces", json!(null), "positive integers"),
        ] {
            let mut row = document()["overrides"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["fact_key"] == "limit.output")
                .unwrap()
                .clone();
            row[field] = value;
            let (rows, rejects) = load_reporting_rejects(&json!({"overrides": [row]}).to_string());
            assert!(rows.is_empty(), "{field}");
            assert_eq!(rejects.len(), 1, "{field}");
            assert!(rejects[0].contains(reason), "{field}: {rejects:?}");
        }
    }

    #[test]
    fn malformed_addition_rows_are_rejected() {
        for key in ADDED_FACTS {
            for (field, value, reason) in [
                ("value", json!(null), "must be a boolean"),
                ("value", json!("true"), "must be a boolean"),
                ("value", json!(1), "must be a boolean"),
                ("replaces", json!(false), "must be additions"),
                ("replaces", json!(true), "must be additions"),
                ("quote", json!(""), "empty identity"),
                ("source_ref", json!(" "), "empty identity"),
                ("review_by_ms", json!(1), "review_by_ms"),
            ] {
                let doc = document();
                let mut row = doc["overrides"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["fact_key"] == key)
                    .unwrap()
                    .clone();
                // A route names a model id, so the string case is a valid
                // value there; a blank id is the malformed string instead.
                let value = if key.starts_with("route.") && value.is_string() {
                    json!(" ")
                } else {
                    value
                };
                row[field] = value;
                let (rows, rejects) =
                    load_reporting_rejects(&json!({"overrides": [row]}).to_string());
                assert!(rows.is_empty(), "{key} {field}");
                assert_eq!(rejects.len(), 1, "{key} {field}");
                let reason = if key.starts_with("rate.") && matches!(field, "value" | "replaces") {
                    "must add a sourced token rate"
                } else if key == "capability.reasoning_default" && field == "value" {
                    "reasoning default value must be exactly"
                } else if key == "capability.allowed_fallback_models" && field == "value" {
                    "must be a list of distinct nonempty model ids"
                } else if key.starts_with("route.") && field == "value" {
                    "must name one nonempty model id"
                } else {
                    reason
                };
                assert!(rejects[0].contains(reason), "{field}: {rejects:?}");
            }
        }
        // An addition must say so: a row without `replaces` is not read as one.
        let mut row = addition_row();
        row.as_object_mut().unwrap().remove("replaces");
        let (rows, rejects) = load_reporting_rejects(&json!({"overrides": [row]}).to_string());
        assert!(rows.is_empty());
        assert!(rejects[0].contains("did not parse"), "{rejects:?}");
        // Two additions for one identity are a duplicate.
        let row = addition_row();
        let (rows, rejects) =
            load_reporting_rejects(&json!({"overrides": [row.clone(), row]}).to_string());
        assert_eq!(rows.len(), 1);
        assert!(rejects[0].contains("duplicate"), "{rejects:?}");
    }

    #[test]
    fn a_status_row_accepts_only_deprecated_over_a_null_status() {
        let status_row = |value: serde_json::Value, replaces: serde_json::Value| {
            let mut row = addition_row();
            row["fact_key"] = json!("model.status");
            row["value"] = value;
            row["replaces"] = replaces;
            row
        };
        let (rows, rejects) = load_reporting_rejects(
            &json!({"overrides": [status_row(json!("deprecated"), json!(null))]}).to_string(),
        );
        assert_eq!(rows.len(), 1, "{rejects:?}");
        for (value, replaces, reason) in [
            (json!("beta"), json!(null), "must be \"deprecated\""),
            (json!(true), json!(null), "must be \"deprecated\""),
            (json!("deprecated"), json!("beta"), "replaces must be null"),
        ] {
            let (rows, rejects) = load_reporting_rejects(
                &json!({"overrides": [status_row(value.clone(), replaces.clone())]}).to_string(),
            );
            assert!(rows.is_empty(), "{value} over {replaces}");
            assert!(
                rejects[0].contains(reason),
                "{value} over {replaces}: {rejects:?}"
            );
        }
    }

    #[test]
    fn an_addition_applies_only_while_upstream_has_no_such_fact() {
        const KEY: &str = "capability.server_side_fallback";
        let only_key = vec![KEY.to_string()];
        let mut served = facts(json!(1));
        let applied = apply(
            "anthropic",
            "claude-opus-5-5",
            &mut served,
            Some(&only_key),
            &[],
        );
        assert_eq!(served[KEY], json!(true));
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].model, "anthropic/claude-opus-5-5");
        assert_eq!(applied[0].fact_key, KEY);
        assert!(applied[0].replaces.is_null());

        let mut served = facts(json!(1));
        apply("anthropic", "claude-haiku-5-5", &mut served, None, &[]);
        assert_eq!(served[KEY], json!(false));

        // Any stored value, null included, is upstream publishing the key.
        for published in [json!(false), json!(true), json!(null)] {
            let mut served = facts(json!(1));
            served.insert(KEY.to_string(), published.clone());
            assert!(apply(
                "anthropic",
                "claude-opus-5-5",
                &mut served,
                Some(&only_key),
                &[]
            )
            .is_empty());
            assert_eq!(served[KEY], published);
        }
        // A withheld fact is stored and hidden, not unpublished.
        let mut served = facts(json!(1));
        assert!(apply(
            "anthropic",
            "claude-opus-5-5",
            &mut served,
            Some(&only_key),
            &[KEY]
        )
        .is_empty());
        assert!(!served.contains_key(KEY));
        // A filter that excludes the key adds nothing; one naming it does.
        let limits = vec!["limit.".to_string()];
        let mut served = facts(json!(1));
        assert!(apply(
            "anthropic",
            "claude-opus-5-5",
            &mut served,
            Some(&limits),
            &[]
        )
        .is_empty());
        assert!(!served.contains_key(KEY));
        let capability = only_key;
        assert_eq!(
            apply(
                "anthropic",
                "claude-opus-5-5",
                &mut served,
                Some(&capability),
                &[]
            )
            .len(),
            1
        );
        // The same model id under another provider is not the Claude API.
        let mut served = facts(json!(1));
        assert!(apply("opencode-go", "claude-haiku-5-5", &mut served, None, &[]).is_empty());
        assert!(!served.contains_key(KEY));
    }

    fn facts(output: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
        BTreeMap::from([
            ("limit.context".to_string(), json!(1_048_576)),
            ("limit.output".to_string(), output),
        ])
    }

    fn reasoning_default(provider: &str, model: &str) -> Option<&'static FactOverride> {
        overrides().get(&(
            provider.to_string(),
            model.to_string(),
            "capability.reasoning_default".to_string(),
        ))
    }

    #[test]
    fn reasoning_default_rows_match_the_documented_models() {
        let expected: BTreeMap<(&str, &str), &str> = [
            ("openai", "gpt-6.1-terra", "medium"),
            ("openai", "gpt-6-luna", "medium"),
            ("openai", "gpt-6-sol", "medium"),
            ("openai", "gpt-6.1-sol", "medium"),
            ("openai", "gpt-5.6", "medium"),
            ("openai", "gpt-5.6-sol", "medium"),
            ("openai", "gpt-5.6-terra", "medium"),
            ("openai", "gpt-5.6-luna", "medium"),
            ("openai", "gpt-5.5", "medium"),
            ("openai", "gpt-5.4-pro", "medium"),
            ("openai", "gpt-5.5-pro", "high"),
            ("openai", "gpt-5-pro", "high"),
            ("openai", "gpt-5.4", "none"),
            ("openai", "gpt-5.4-mini", "none"),
            ("openai", "gpt-5.4-nano", "none"),
            ("openai", "gpt-5.2", "none"),
            ("openai", "gpt-5.1", "none"),
            ("anthropic", "claude-opus-5-5", "medium"),
            ("anthropic", "claude-sonnet-5-5", "high"),
            ("anthropic", "claude-haiku-5-5", "medium"),
            ("anthropic", "claude-fable-5", "high"),
            ("anthropic", "claude-fable-5-1", "high"),
            ("anthropic", "claude-fable-5-2", "high"),
            ("anthropic", "claude-opus-5", "high"),
            ("google", "gemini-4-carbon", "medium"),
            ("google", "gemini-4-argon", "medium"),
            ("google", "gemini-4-hydrogen", "high"),
            ("google", "gemini-3.8-flash", "medium"),
            ("google", "gemini-3.6-flash", "medium"),
            ("google", "gemini-3.5-flash-lite", "minimal"),
            ("xai", "grok-4.7", "high"),
            ("xai", "grok-4.6", "high"),
            ("xai", "grok-4.5", "high"),
            ("xai", "grok-4.3", "low"),
            ("moonshotai", "kimi-k3", "max"),
            ("moonshotai", "kimi-k3.1-pro", "max"),
            ("deepseek", "deepseek-flash", "high"),
            ("deepseek", "deepseek-v4-pro", "high"),
            ("meta", "muse-spark-1.1", "high"),
            ("meta", "muse-spark-1.2", "high"),
            ("meta", "muse-spark-1.3", "high"),
            ("meta", "muse-spark-1.2-contributor", "high"),
            ("meta", "muse-spark-1.3-contributor", "high"),
            ("meta", "muse-wonder-1.0", "max"),
            ("meta", "muse-ember-1.0", "max"),
        ]
        .into_iter()
        .map(|(provider, model, effort)| ((provider, model), effort))
        .collect();
        let rows: Vec<_> = overrides()
            .values()
            .filter(|row| row.fact_key == "capability.reasoning_default")
            .collect();
        assert_eq!(rows.len(), expected.len());
        for row in rows {
            let effort = expected[&(row.provider_id.as_str(), row.model_id.as_str())];
            assert_eq!(
                row.value,
                json!({"type":"effort","value":effort}),
                "{}/{}",
                row.provider_id,
                row.model_id
            );
            assert!(row.replaces.is_null());
            assert_eq!(row.review_by_ms - row.established_at_ms, REVIEW_INTERVAL_MS);
        }
        // OpenAI documents no default for these two, so nothing is served.
        for model in ["gpt-6.2-sol", "gpt-6.2-astra"] {
            assert!(reasoning_default("openai", model).is_none(), "{model}");
        }
    }

    #[test]
    fn a_reasoning_default_outside_the_row_options_is_refused() {
        const KEY: &str = "capability.reasoning_default";
        let row = reasoning_default("openai", "gpt-5.5").unwrap();
        let options = |values: serde_json::Value| {
            BTreeMap::from([(
                "capability.reasoning_options".to_string(),
                json!([{"type":"toggle"}, {"type":"effort","values":values}]),
            )])
        };
        // Control: options listing the effort let the row apply.
        let mut served = options(json!(["low", "medium", "high"]));
        assert_eq!(apply("openai", "gpt-5.5", &mut served, None, &[]).len(), 1);
        assert_eq!(served[KEY], json!({"type":"effort","value":"medium"}));
        assert!(stale_row(row, Some(&options(json!(["medium"])))).is_none());
        // Options that do not list it, or no options at all, refuse it at
        // serve time and in the seed gate.
        for stored in [
            options(json!(["low", "high"])),
            BTreeMap::from([(
                "capability.reasoning_options".to_string(),
                json!([{"type":"budget","values":["medium"]}]),
            )]),
            BTreeMap::from([("capability.reasoning_options".to_string(), json!(null))]),
            BTreeMap::new(),
        ] {
            let mut served = stored.clone();
            assert!(apply("openai", "gpt-5.5", &mut served, None, &[]).is_empty());
            assert!(!served.contains_key(KEY));
            let stale = stale_row(row, Some(&stored)).expect("refused");
            assert!(stale.contains("openai/gpt-5.5 capability.reasoning_default"));
            assert!(stale.contains("do not list"), "{stale}");
        }
        // Through the real gate, a seed whose options drop the effort names
        // the row.
        let mut seed = seed();
        seed["openai"]["models"]["gpt-5.5"]["reasoning_options"] =
            json!([{"type":"effort","values":["low","high"]}]);
        let stale = stale_against_seed(overrides(), &seed);
        assert_eq!(stale.len(), 1, "{stale:?}");
        assert!(stale[0].contains("openai/gpt-5.5 capability.reasoning_default"));
        // The loader refuses any other value shape.
        let shipped = document()["overrides"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["fact_key"] == KEY)
            .unwrap()
            .clone();
        for value in [
            json!("medium"),
            json!({"type":"budget","value":"medium"}),
            json!({"type":"effort","value":""}),
            json!({"type":"effort","value":1}),
            json!({"type":"effort"}),
            json!({"type":"effort","value":"medium","extra":true}),
        ] {
            let mut planted = shipped.clone();
            planted["value"] = value.clone();
            let (rows, rejects) =
                load_reporting_rejects(&json!({"overrides":[planted]}).to_string());
            assert!(rows.is_empty(), "{value}");
            assert!(
                rejects[0].contains("reasoning default value must be exactly"),
                "{value}: {rejects:?}"
            );
        }
    }

    #[test]
    fn applies_only_while_upstream_publishes_the_replaced_value() {
        let mut served = facts(json!(32_768));
        let applied = apply("google", "gemini-4-argon", &mut served, None, &[]);
        assert_eq!(served["limit.output"], json!(65_536));
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].model, "google/gemini-4-argon");
        assert_eq!(applied[0].replaces, json!(32_768));

        // Upstream moved to any other value, including the override's own:
        // upstream wins and nothing is disclosed.
        for moved in [json!(65_536), json!(49_152), json!(null)] {
            let mut served = facts(moved.clone());
            assert!(apply("google", "gemini-4-argon", &mut served, None, &[]).is_empty());
            assert_eq!(served["limit.output"], moved);
        }
        // An absent fact is not supplied.
        let mut served = BTreeMap::new();
        assert!(apply("google", "gemini-4-argon", &mut served, None, &[]).is_empty());
        assert!(served.is_empty());
        // Another model, or another provider serving the same id, is untouched.
        for (provider, model) in [
            ("google", "gemini-3.8-flash"),
            ("openrouter", "gemini-4-argon"),
        ] {
            let mut served = facts(json!(32_768));
            assert!(apply(provider, model, &mut served, None, &[]).is_empty());
            assert_eq!(served["limit.output"], json!(32_768));
        }
    }

    #[test]
    fn a_filter_excluding_the_fact_applies_nothing() {
        let mut served = facts(json!(32_768));
        let rates = vec!["rate.".to_string()];
        assert!(apply("google", "gemini-4-argon", &mut served, Some(&rates), &[]).is_empty());
        assert_eq!(served["limit.output"], json!(32_768));
        let limits = vec!["limit.".to_string()];
        assert_eq!(
            apply("google", "gemini-4-argon", &mut served, Some(&limits), &[]).len(),
            1
        );
    }
}
