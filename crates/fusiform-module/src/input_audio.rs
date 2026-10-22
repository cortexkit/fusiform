//! Providers whose own pricing documentation bills audio input per token.
//!
//! models.dev publishes `cost.input_audio` beside the per-token rates, in the
//! same "$ per million" convention, with nothing in the payload to say what
//! unit the number charges for. The normalizer therefore refuses it
//! everywhere by default. A provider listed in `input-audio-per-token.json`
//! has a pricing page that states audio input is priced per million tokens,
//! quoted in the row; for that provider the key is served as
//! `rate.input_audio`.
//!
//! Unlike the other curated files this one is applied at INGEST, not at read
//! time, because it does not add a claim of fusiform's own to a row: the
//! number is still the upstream's observation. The file only decides whether
//! the upstream number has an established unit, so the resulting fact is
//! stored as an era like every other models.dev rate, and a point-in-time read
//! replays what was served then. The table is handed to
//! `fusiform_core::normalize_models_dev_with` as data, keeping file IO out of
//! the core crate.

use std::collections::BTreeMap;

use fusiform_core::ChargeUnitEvidence;

const DOCUMENT: &str = include_str!("../data/input-audio-per-token.json");
const SCHEMA: &str = "input-audio-per-token/v1";
const REVIEW_INTERVAL_MS: i64 = 60 * 86_400_000;

/// One provider whose pricing page establishes per-token audio billing.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputAudioSource {
    pub provider_id: String,
    /// The provider's own pricing page that was read.
    pub source_ref: String,
    /// The sentence or table row on that page stating audio input is priced
    /// per million tokens, verbatim.
    pub quote: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
    /// Models of this provider whose published `input_audio` the page does not
    /// support, each with the reason. Their audio key stays unattributed.
    #[serde(default)]
    pub excluded_models: Vec<ExcludedModel>,
}

/// A model left out although its provider is listed.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExcludedModel {
    pub model_id: String,
    pub reason: String,
}

/// A provider whose page was read and did NOT establish the unit, kept so the
/// decision and its source are recorded rather than rediscovered. It has no
/// effect on what is served.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclinedProvider {
    pub provider_id: String,
    pub source_ref: String,
    pub quote: String,
    pub reason: String,
}

pub type InputAudioSources = BTreeMap<String, InputAudioSource>;

/// Reject malformed rows and duplicate providers rather than silently choosing
/// one. Declined rows are validated too, and one naming a listed provider is
/// a reject: the file cannot both accept and decline the same source.
pub fn load_reporting_rejects(doc: &str) -> (InputAudioSources, Vec<String>) {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Doc {
        schema: String,
        providers: Vec<serde_json::Value>,
        #[serde(default)]
        declined: Vec<serde_json::Value>,
    }
    let doc: Doc = match serde_json::from_str(doc) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                BTreeMap::new(),
                vec![format!("input-audio sources unreadable: {error}")],
            )
        }
    };
    if doc.schema != SCHEMA {
        return (
            BTreeMap::new(),
            vec![format!(
                "input-audio sources declare schema {:?}, expected {SCHEMA:?}",
                doc.schema
            )],
        );
    }
    let mut rows = InputAudioSources::new();
    let mut rejects = Vec::new();
    for raw in doc.providers {
        let row: InputAudioSource = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!("input-audio source did not parse: {error}"));
                continue;
            }
        };
        let mut excluded = std::collections::BTreeSet::new();
        let invalid = if [&row.provider_id, &row.source_ref, &row.quote]
            .iter()
            .any(|field| field.trim().is_empty())
        {
            Some("empty provider, source or quote")
        } else if !row.source_ref.starts_with("https://") {
            Some("source_ref must be an https URL to the provider's pricing page")
        } else if !row.quote.contains("1M tokens") && !row.quote.contains("million tokens") {
            Some("quote must state the per-million-token unit it establishes")
        } else if row.established_at_ms <= 0
            || row.review_by_ms.checked_sub(row.established_at_ms) != Some(REVIEW_INTERVAL_MS)
        {
            Some("review_by_ms must be 60 days after established_at_ms")
        } else if rows.contains_key(&row.provider_id) {
            Some("duplicate provider")
        } else if row.excluded_models.iter().any(|m| {
            m.model_id.trim().is_empty()
                || m.reason.trim().is_empty()
                || !excluded.insert(m.model_id.clone())
        }) {
            Some("an excluded model needs a unique model_id and a reason")
        } else {
            None
        };
        if let Some(reason) = invalid {
            rejects.push(format!("input-audio source {}: {reason}", row.provider_id));
        } else {
            rows.insert(row.provider_id.clone(), row);
        }
    }
    let mut declined = std::collections::BTreeSet::new();
    for raw in doc.declined {
        let row: DeclinedProvider = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!(
                    "declined input-audio source did not parse: {error}"
                ));
                continue;
            }
        };
        let invalid = if [&row.provider_id, &row.source_ref, &row.quote, &row.reason]
            .iter()
            .any(|field| field.trim().is_empty())
        {
            Some("empty provider, source, quote or reason")
        } else if rows.contains_key(&row.provider_id) {
            Some("declined and listed at once")
        } else if !declined.insert(row.provider_id.clone()) {
            Some("duplicate declined provider")
        } else {
            None
        };
        if let Some(reason) = invalid {
            rejects.push(format!(
                "declined input-audio source {}: {reason}",
                row.provider_id
            ));
        }
    }
    (rows, rejects)
}

/// The shipped table, rejects dropped. A reject in the shipped file fails the
/// build through `shipped_sources_load_without_rejects`.
pub fn sources() -> &'static InputAudioSources {
    static ROWS: std::sync::OnceLock<InputAudioSources> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

/// The normalizer's view of a table: which providers bill audio per token,
/// less the models each one excludes.
pub fn evidence_from(rows: &InputAudioSources) -> ChargeUnitEvidence {
    rows.values().fold(
        ChargeUnitEvidence::input_audio_per_token(rows.keys().cloned()),
        |evidence, row| {
            row.excluded_models.iter().fold(evidence, |evidence, m| {
                evidence.excluding_input_audio_of(row.provider_id.clone(), m.model_id.clone())
            })
        },
    )
}

/// The evidence production ingest and seeding normalize with.
pub fn evidence() -> ChargeUnitEvidence {
    evidence_from(sources())
}

/// Listed providers the embedded seed does not carry, or carries with no
/// `cost.input_audio` on any row, and excluded models the seed does not
/// carry with `cost.input_audio`: a row like that establishes or excludes
/// nothing and should be corrected or removed.
pub fn unused_rows(rows: &InputAudioSources, seed: &serde_json::Value) -> Vec<String> {
    let publishes_audio = |model: &serde_json::Value| model["cost"].get("input_audio").is_some();
    let mut unused = Vec::new();
    for row in rows.values() {
        let provider = &row.provider_id;
        let models = seed[provider.as_str()]["models"].as_object();
        if !models.is_some_and(|models| models.values().any(publishes_audio)) {
            unused.push(format!(
                "{provider}: no row in the embedded seed publishes cost.input_audio; \
                 correct or remove the input-audio source"
            ));
        }
        for excluded in &row.excluded_models {
            let model = models.and_then(|models| models.get(&excluded.model_id));
            if !model.is_some_and(publishes_audio) {
                unused.push(format!(
                    "{provider}/{}: excluded, but the embedded seed has no such row \
                     publishing cost.input_audio; correct or remove the exclusion",
                    excluded.model_id
                ));
            }
        }
    }
    unused
}

/// Rows past their review date.
pub fn overdue_at(rows: &InputAudioSources, now_ms: i64) -> Vec<String> {
    rows.values()
        .filter(|row| row.review_by_ms < now_ms)
        .map(|row| {
            format!(
                "NOT A BUILD FAILURE: a review date passed. {}: re-read {} and confirm \
                 it still prices audio input per million tokens, then re-date \
                 established_at_ms and review_by_ms (60 days later), or remove the row",
                row.provider_id, row.source_ref
            )
        })
        .collect()
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
    fn shipped_sources_load_without_rejects() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert!(!rows.is_empty());
        assert!(rows.contains_key("google"));
        assert!(evidence().bills_input_audio_per_token("google", "gemini-3.8-flash"));
        assert!(!evidence().bills_input_audio_per_token("google", "gemini-3.1-flash-lite-preview"));
        assert!(!evidence().bills_input_audio_per_token("google-vertex", "gemini-3.8-flash"));
    }

    #[test]
    fn planted_stale_exclusion_is_named() {
        let mut seed = seed();
        let row = &sources()["google"];
        let excluded = &row.excluded_models[0].model_id;
        seed["google"]["models"][excluded]["cost"]
            .as_object_mut()
            .unwrap()
            .remove("input_audio");
        let unused = unused_rows(sources(), &seed);
        assert_eq!(unused.len(), 1, "{unused:?}");
        assert!(unused[0].starts_with(&format!("google/{excluded}:")));
        assert!(unused[0].contains("remove the exclusion"));
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
            if let Some(cost) = model["cost"].as_object_mut() {
                cost.remove("input_audio");
            }
        }
        let unused = unused_rows(sources(), &seed);
        // Stripping every audio key also strands the provider's exclusions,
        // which are reported on their own lines.
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
