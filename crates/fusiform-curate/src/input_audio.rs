//! Providers whose own pricing documentation establishes per-token audio
//! billing.
//!
//! models.dev publishes `cost.input_audio` beside the per-token rates, in the
//! same "$ per million" convention, without saying whether the number is per
//! token, second or minute. Without evidence the normalizer leaves the key
//! unpriced. A provider listed in `input-audio-per-token.json` has a pricing
//! page that states audio input is priced per million tokens, quoted in the
//! row; its eligible models are served as `rate.input_audio`.
//!
//! The pricing evidence is used while normalizing a poll or the embedded seed,
//! because it adds no price of fusiform's own: the number is
//! still the upstream's observation, and the file only decides whether it has
//! an established unit. The resulting fact is stored as an era like every
//! other models.dev rate, so a point-in-time read replays what was recorded
//! then. The module converts the rows to `ChargeUnitEvidence` and passes that
//! to `fusiform_core::normalize_models_dev_with`, so the core crate does no
//! file IO.
//!
//! A row may also quote the provider's audio tokens per second. That conversion
//! is joined only onto current served rows carrying `rate.input_audio`, never
//! stored or applied to a point-in-time read.

use std::collections::BTreeMap;

use fusiform_core::ChargeUnitEvidence;

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
    /// When present, evidence covers only these model ids, not the provider.
    /// An empty list cannot establish any billing unit.
    #[serde(default)]
    pub included_models: Option<Vec<String>>,
    /// Models of this provider whose published `input_audio` the page does not
    /// support, each with the reason. Their audio key stays unattributed.
    #[serde(default)]
    pub excluded_models: Vec<ExcludedModel>,
    /// A tokenizer conversion with model-specific evidence, joined only onto current audio rates.
    #[serde(default)]
    pub audio_tokens_per_second: Option<AudioTokensPerSecond>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioTokensPerSecond {
    pub value: u64,
    pub source_ref: String,
    pub quote: String,
    #[serde(default)]
    pub model_overrides: Vec<AudioTokensPerSecondOverride>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioTokensPerSecondOverride {
    pub model_id: String,
    pub value: u64,
    pub basis: String,
    pub source_ref: String,
    pub quote: String,
}

impl AudioTokensPerSecond {
    pub fn value_for(&self, model_id: &str) -> u64 {
        self.model_overrides
            .iter()
            .find(|row| row.model_id == model_id)
            .map_or(self.value, |row| row.value)
    }
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
    load_for_schema(doc, SCHEMA)
}

/// The two directions share source validation, not unit evidence: each has its
/// own schema, source document and normalizer allowlist.
pub fn load_for_schema(doc: &str, schema: &str) -> (InputAudioSources, Vec<String>) {
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
    if doc.schema != schema {
        return (
            BTreeMap::new(),
            vec![format!(
                "audio sources declare schema {:?}, expected {schema:?}",
                doc.schema
            )],
        );
    }
    let mut rows = InputAudioSources::new();
    let mut rejects = Vec::new();
    for raw in doc.providers {
        let included_present = raw.get("included_models").is_some();
        let excluded_present = raw.get("excluded_models").is_some();
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
        } else if included_present && schema != SCHEMA {
            Some("included_models is supported only for input-audio evidence")
        } else if included_present && excluded_present {
            Some("included_models and excluded_models cannot both be set")
        } else if included_present
            && row.included_models.as_ref().is_none_or(|models| {
                let mut ids = std::collections::BTreeSet::new();
                models.is_empty()
                    || models
                        .iter()
                        .any(|id| id.trim().is_empty() || !ids.insert(id))
            })
        {
            Some("included_models needs a nonempty list of unique model ids")
        } else if row.audio_tokens_per_second.as_ref().is_some_and(|audio| {
            audio.value == 0
                || !audio.source_ref.starts_with("https://")
                || audio.quote.trim().is_empty()
        }) {
            Some("audio_tokens_per_second needs a positive integer, https source_ref and quote")
        } else if row.audio_tokens_per_second.as_ref().is_some_and(|audio| {
            let mut models = std::collections::BTreeSet::new();
            audio.model_overrides.iter().any(|entry| {
                entry.model_id.trim().is_empty()
                    || !models.insert(&entry.model_id)
                    || entry.value == 0
                    || entry.basis != "measured"
                    || !entry.source_ref.starts_with("https://")
                    || entry.quote.trim().is_empty()
            })
        }) {
            Some("an audio token override needs a unique model_id, positive integer, measured basis, https source_ref and quote")
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

/// The normalizer's view of provider-wide or model-only input evidence.
pub fn evidence_from(rows: &InputAudioSources) -> ChargeUnitEvidence {
    rows.values().fold(
        ChargeUnitEvidence::input_audio_per_token(
            rows.values()
                .filter(|row| row.included_models.is_none())
                .map(|row| row.provider_id.clone()),
        ),
        |evidence, row| {
            let evidence =
                row.included_models
                    .iter()
                    .flatten()
                    .fold(evidence, |evidence, model| {
                        evidence.with_input_audio_model(row.provider_id.clone(), model.clone())
                    });
            row.excluded_models.iter().fold(evidence, |evidence, m| {
                evidence.excluding_input_audio_of(row.provider_id.clone(), m.model_id.clone())
            })
        },
    )
}

/// Listed providers the embedded seed does not carry, or carries with no
/// `cost.input_audio` on any row, included or excluded models the seed does not
/// carry with `cost.input_audio`, and audio token overrides naming models
/// absent from that provider: those entries should be corrected or removed.
pub fn unused_rows(rows: &InputAudioSources, seed: &serde_json::Value) -> Vec<String> {
    let publishes_audio = |model: &serde_json::Value| model["cost"].get("input_audio").is_some();
    let mut unused = Vec::new();
    for row in rows.values() {
        let provider = &row.provider_id;
        let models = seed[provider.as_str()]["models"].as_object();
        if let Some(included) = &row.included_models {
            for model_id in included {
                if !models
                    .and_then(|models| models.get(model_id))
                    .is_some_and(publishes_audio)
                {
                    unused.push(format!(
                        "{provider}/{model_id}: included, but the embedded seed has no such row \
                        publishing cost.input_audio; correct or remove the inclusion"
                    ));
                }
            }
        }
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
        if let Some(audio) = &row.audio_tokens_per_second {
            for entry in &audio.model_overrides {
                if !models.is_some_and(|models| models.contains_key(&entry.model_id)) {
                    unused.push(format!(
                        "{provider}/{}: no such model in this provider's embedded seed; \
                         correct or remove the audio token override",
                        entry.model_id
                    ));
                }
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
