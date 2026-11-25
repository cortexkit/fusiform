//! The embedded input audio document and its local adapter.

use fusiform_core::ChargeUnitEvidence;
pub use fusiform_curate::input_audio::*;

const DOCUMENT: &str = include_str!("../data/input-audio-per-token.json");

#[cfg(test)]
const SCHEMA: &str = "input-audio-per-token/v1";
/// The embedded provider rows, with the reject list discarded. A test
/// (`shipped_sources_load_without_rejects`) asserts the shipped file produces
/// no rejects.
pub fn sources() -> &'static InputAudioSources {
    static ROWS: std::sync::OnceLock<InputAudioSources> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

/// The evidence production ingest and seeding normalize with.
pub fn evidence() -> ChargeUnitEvidence {
    crate::output_audio::add_evidence(evidence_from(sources()))
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

    #[test]
    fn included_models_are_exact_nonempty_and_exclusive() {
        let openai = json!({
            "provider_id": "openai",
            "included_models": ["gpt-realtime-2.1"],
            "source_ref": "https://example.org/audio-pricing",
            "quote": "Audio input: USD 32 per million tokens",
            "established_at_ms": 1794528000000_i64,
            "review_by_ms": 1799712000000_i64
        });
        let load = |row: serde_json::Value| {
            load_reporting_rejects(&json!({"schema":SCHEMA, "providers":[row]}).to_string())
        };
        let (rows, rejects) = load(openai.clone());
        assert!(rejects.is_empty(), "{rejects:?}");
        let evidence = evidence_from(&rows);
        assert!(evidence.bills_input_audio_per_token("openai", "gpt-realtime-2.1"));
        assert!(!evidence.bills_input_audio_per_token("openai", "gpt-6.1-sol"));
        assert!(!evidence.bills_input_audio_per_token("openai", "future-audio-model"));
        assert!(!evidence.bills_input_audio_per_token("other", "gpt-realtime-2.1"));
        for included in [
            json!([]),
            json!(null),
            json!([""]),
            json!([" "]),
            json!(["same", "same"]),
        ] {
            let mut row = openai.clone();
            row["included_models"] = included;
            let (rows, rejects) = load(row);
            assert!(rows.is_empty());
            assert_eq!(rejects.len(), 1);
            assert!(rejects[0].contains("nonempty list of unique model ids"));
        }
        let mut row = openai.clone();
        row["excluded_models"] = json!([]);
        let (rows, rejects) = load(row);
        assert!(rows.is_empty());
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("cannot both be set"));

        let mut row = openai;
        row["included_models"] = json!(["not-in-the-embedded-seed"]);
        let (rows, rejects) = load(row);
        assert!(rejects.is_empty());
        let unused = unused_rows(&rows, &seed());
        assert_eq!(unused.len(), 1, "{unused:?}");
        assert!(unused[0].starts_with("openai/not-in-the-embedded-seed:"));
        assert!(unused[0].contains("correct or remove the inclusion"));
    }

    #[test]
    fn shipped_sources_load_without_rejects() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert!(!rows.is_empty());
        assert!(rows.contains_key("google"));
        assert!(evidence().bills_input_audio_per_token("google", "gemini-3.8-flash"));
        assert!(evidence().bills_input_audio_per_token("openai", "gpt-realtime-2.1"));
        assert!(!evidence().bills_input_audio_per_token("openai", "gpt-realtime-2.1-mini"));
        assert!(!evidence().bills_input_audio_per_token("google-vertex", "gemini-3.8-flash"));
        assert_eq!(
            rows["google"]
                .audio_tokens_per_second
                .as_ref()
                .unwrap()
                .value,
            32
        );
    }

    #[test]
    fn audio_tokens_per_second_is_optional_and_malformed_values_are_counted_rejects() {
        let mut row = document()["providers"][0].clone();
        row.as_object_mut()
            .unwrap()
            .remove("audio_tokens_per_second");
        let (rows, rejects) = load_reporting_rejects(
            &json!({"schema": SCHEMA, "providers": [row.clone()]}).to_string(),
        );
        assert!(rejects.is_empty(), "{rejects:?}");
        assert!(rows["google"].audio_tokens_per_second.is_none());
        let valid = json!({
            "value": 32,
            "source_ref": "https://ai.google.dev/gemini-api/docs/audio",
            "quote": "Tokens: 32 tokens per second of audio (1 minute = 1,920 tokens)",
        });
        for (field, value) in [
            ("value", json!(0)),
            ("value", json!(-1)),
            ("value", json!(1.5)),
            ("value", json!("32")),
            ("value", json!(null)),
            ("source_ref", json!("http://example.com/audio")),
            ("source_ref", json!("")),
            ("quote", json!(" ")),
            ("unknown", json!(true)),
        ] {
            row["audio_tokens_per_second"] = valid.clone();
            row["audio_tokens_per_second"][field] = value;
            let (rows, rejects) = load_reporting_rejects(
                &json!({"schema": SCHEMA, "providers": [row.clone()]}).to_string(),
            );
            assert!(rows.is_empty(), "{field}: {rows:?}");
            assert_eq!(rejects.len(), 1, "{field}: {rejects:?}");
        }
    }

    #[test]
    fn audio_token_overrides_are_optional_and_malformed_values_are_rejected() {
        let mut row = document()["providers"][0].clone();
        let valid = json!({
            "model_id": "gemini-3.8-flash",
            "value": 25,
            "basis": "measured",
            "source_ref": "https://ai.google.dev/api/generate-content#UsageMetadata",
            "quote": "Audio input counts billed at ceil(25 × duration in seconds)",
        });
        let load = |row: &serde_json::Value| {
            load_reporting_rejects(&json!({"schema": SCHEMA, "providers": [row]}).to_string())
        };
        row["audio_tokens_per_second"]["model_overrides"] = json!([valid.clone()]);
        let (rows, rejects) = load(&row);
        assert!(rejects.is_empty(), "{rejects:?}");
        let audio = rows["google"].audio_tokens_per_second.as_ref().unwrap();
        assert_eq!(audio.value_for("gemini-3.8-flash"), 25);
        assert_eq!(audio.value_for("gemini-4-argon"), 32);

        for (field, value) in [
            ("model_id", json!(" ")),
            ("value", json!(0)),
            ("value", json!(-1)),
            ("value", json!(1.5)),
            ("value", json!("25")),
            ("value", json!(null)),
            ("basis", json!("")),
            ("basis", json!("documented")),
            ("source_ref", json!("http://example.com/audio")),
            ("source_ref", json!("")),
            ("quote", json!(" ")),
            ("unknown", json!(true)),
        ] {
            let mut entry = valid.clone();
            entry[field] = value;
            row["audio_tokens_per_second"]["model_overrides"] = json!([entry]);
            let (rows, rejects) = load(&row);
            assert!(rows.is_empty(), "{field}: {rows:?}");
            assert_eq!(rejects.len(), 1, "{field}: {rejects:?}");
        }
        row["audio_tokens_per_second"]["model_overrides"] = json!([valid.clone(), valid]);
        let (rows, rejects) = load(&row);
        assert!(rows.is_empty());
        assert_eq!(rejects.len(), 1, "{rejects:?}");

        row["audio_tokens_per_second"]
            .as_object_mut()
            .unwrap()
            .remove("model_overrides");
        let (rows, rejects) = load(&row);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(
            rows["google"]
                .audio_tokens_per_second
                .as_ref()
                .unwrap()
                .value_for("gemini-3.8-flash"),
            32
        );
    }

    #[test]
    fn audio_token_override_naming_unknown_model_fails_seed_fence() {
        let mut doc = document();
        doc["providers"][0]["audio_tokens_per_second"]["model_overrides"][0]["model_id"] =
            json!("not-in-the-embedded-seed");
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert!(rejects.is_empty(), "{rejects:?}");
        let unused = unused_rows(&rows, &seed());
        assert_eq!(unused.len(), 1, "{unused:?}");
        assert!(unused[0].starts_with("google/not-in-the-embedded-seed:"));
        assert!(unused[0].contains("remove the audio token override"));
    }

    #[test]
    fn planted_stale_exclusion_is_named() {
        let mut seed = seed();
        let mut doc = document();
        let excluded = "gemini-3.8-flash";
        doc["providers"][0]["excluded_models"] =
            json!([{"model_id": excluded, "reason": "Not in the fixture pricing table"}]);
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert!(rejects.is_empty(), "{rejects:?}");
        assert!(unused_rows(&rows, &seed).is_empty());
        seed["google"]["models"][excluded]["cost"]["audio"]
            .as_object_mut()
            .unwrap()
            .remove("input");
        let unused = unused_rows(&rows, &seed);
        assert_eq!(unused.len(), 1, "{unused:?}");
        assert!(unused[0].starts_with(&format!("google/{excluded}:")));
        assert!(unused[0].contains("remove the exclusion"));
    }

    #[test]
    fn planted_audio_token_override_without_seed_model_is_named() {
        let mut seed = seed();
        seed["google"]["models"]
            .as_object_mut()
            .unwrap()
            .remove("gemini-3.8-flash")
            .unwrap();
        let unused = unused_rows(sources(), &seed);
        assert_eq!(unused.len(), 1, "{unused:?}");
        assert!(unused[0].starts_with("google/gemini-3.8-flash:"));
        assert!(unused[0].contains("remove the audio token override"));
    }

    #[test]
    fn malformed_exclusions_and_declined_rows_are_rejected() {
        let listed = document()["providers"][0].clone();
        let declined = document()["declined"][0].clone();
        let load = |providers: serde_json::Value, declined: serde_json::Value| {
            load_reporting_rejects(
                &json!({"schema": SCHEMA, "providers": providers, "declined": declined})
                    .to_string(),
            )
        };
        // Control: the shipped rows load clean in this harness.
        assert!(load(json!([listed.clone()]), json!([declined.clone()]))
            .1
            .is_empty());

        for excluded in [
            json!([{"model_id": "", "reason": "r"}]),
            json!([{"model_id": "m", "reason": " "}]),
            json!([{"model_id": "m", "reason": "r"}, {"model_id": "m", "reason": "r"}]),
            json!([{"model_id": "m"}]),
        ] {
            let mut row = listed.clone();
            row["excluded_models"] = excluded.clone();
            let (rows, rejects) = load(json!([row]), json!([]));
            assert!(rows.is_empty(), "{excluded}");
            assert_eq!(rejects.len(), 1, "{excluded}");
        }

        for (field, value) in [
            ("provider_id", json!("")),
            ("provider_id", listed["provider_id"].clone()),
            ("reason", json!("")),
            ("unknown", json!(1)),
        ] {
            let mut row = declined.clone();
            row[field] = value;
            let (_, rejects) = load(json!([listed.clone()]), json!([row]));
            assert_eq!(rejects.len(), 1, "{field}");
        }
        let (_, rejects) = load(json!([]), json!([declined.clone(), declined]));
        assert_eq!(rejects.len(), 1);
        assert!(rejects[0].contains("duplicate declined"));
    }

    #[test]
    fn every_source_has_input_audio_rows_in_the_embedded_seed() {
        let unused = unused_rows(sources(), &seed());
        assert!(unused.is_empty(), "{}", unused.join("\n"));
    }

    #[test]
    fn planted_provider_without_audio_rows_is_named() {
        let mut seed = seed();
        let provider = sources().keys().next().unwrap().clone();
        for model in seed[&provider]["models"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            if let Some(cost) = model["cost"]["audio"].as_object_mut() {
                cost.remove("input");
            }
        }
        let unused = unused_rows(sources(), &seed);
        // Missing model-level evidence is reported separately from the provider.
        let provider_level: Vec<_> = unused
            .iter()
            .filter(|line| line.starts_with(&format!("{provider}:")))
            .collect();
        assert_eq!(provider_level.len(), 1, "{unused:?}");
        assert!(provider_level[0].contains("correct or remove"));
    }

    #[test]
    fn input_audio_sources_are_within_their_review_dates() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let overdue = overdue_at(sources(), now);
        assert!(overdue.is_empty(), "{}", overdue.join("\n"));
    }

    #[test]
    fn planted_overdue_source_requires_rereading_or_removal() {
        let row = sources().values().next().unwrap().clone();
        let rows = InputAudioSources::from([(row.provider_id.clone(), row.clone())]);
        assert!(overdue_at(&rows, row.review_by_ms).is_empty());
        let overdue = overdue_at(&rows, row.review_by_ms + 1);
        assert_eq!(overdue.len(), 1);
        assert!(overdue[0].contains(&row.source_ref));
        assert!(overdue[0].contains("or remove"));
    }

    #[test]
    fn malformed_and_duplicate_rows_are_counted_as_rejects() {
        let mut doc = document();
        let row = doc["providers"][0].clone();
        doc["providers"]
            .as_array_mut()
            .unwrap()
            .extend([row, json!({})]);
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rows.len(), sources().len());
        assert_eq!(rejects.len(), 2, "{rejects:?}");
        assert!(rejects[0].contains("duplicate provider"));
        assert!(rejects[1].contains("did not parse"));
        assert_eq!(load_reporting_rejects("not json").1.len(), 1);
        assert_eq!(
            load_reporting_rejects(
                r#"{"schema": "input-audio-per-token/v1", "providers": [], "extra": 1}"#
            )
            .1
            .len(),
            1
        );
        assert_eq!(
            load_reporting_rejects(r#"{"schema": "input-audio-per-token/v2", "providers": []}"#)
                .1
                .len(),
            1
        );
    }

    #[test]
    fn malformed_fields_are_rejected() {
        for (field, value) in [
            ("provider_id", json!(" ")),
            ("source_ref", json!("")),
            ("source_ref", json!("http://example.com/pricing")),
            ("quote", json!("")),
            ("quote", json!("Audio input is priced per second")),
            ("established_at_ms", json!(0)),
            ("review_by_ms", json!(1)),
            ("unknown", json!(true)),
        ] {
            let mut row = document()["providers"][0].clone();
            row[field] = value;
            let (rows, rejects) =
                load_reporting_rejects(&json!({"schema": SCHEMA, "providers": [row]}).to_string());
            assert!(rows.is_empty(), "{field}");
            assert_eq!(rejects.len(), 1, "{field}");
        }
    }
}
