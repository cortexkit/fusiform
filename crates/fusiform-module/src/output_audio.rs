//! Sourced output-audio token units, independent of input-audio billing.
//!
//! models.dev's audio cost keys do not establish whether a number bills tokens
//! or time. Only providers whose own pricing page states the token unit are
//! admitted. This changes ingest evidence, not the upstream price or history.

use fusiform_core::ChargeUnitEvidence;

const DOCUMENT: &str = include_str!("../data/output-audio-per-token.json");
const SCHEMA: &str = "output-audio-per-token/v1";

pub fn sources() -> &'static crate::input_audio::InputAudioSources {
    static ROWS: std::sync::OnceLock<crate::input_audio::InputAudioSources> =
        std::sync::OnceLock::new();
    ROWS.get_or_init(|| crate::input_audio::load_for_schema(DOCUMENT, SCHEMA).0)
}

pub fn add_evidence(evidence: ChargeUnitEvidence) -> ChargeUnitEvidence {
    fusiform_curate::input_audio::add_output_evidence(evidence, sources())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_audio_sources_are_valid_used_and_within_review_dates() {
        let (rows, rejects) = crate::input_audio::load_for_schema(DOCUMENT, SCHEMA);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(
            rows.keys().map(String::as_str).collect::<Vec<_>>(),
            ["google", "openai"]
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let overdue = crate::input_audio::overdue_at(&rows, now);
        assert!(overdue.is_empty(), "{overdue:?}");
        let seed: serde_json::Value =
            serde_json::from_str(include_str!("../data/models-dev-seed.json")).unwrap();
        for row in rows.values() {
            assert!(
                row.audio_tokens_per_second.is_none(),
                "output tokenization is model-specific, not a provider-wide input conversion"
            );
            let models = seed[&row.provider_id]["models"].as_object().unwrap();
            let publishes_audio =
                |model: &serde_json::Value| model.pointer("/cost/audio/output/price").is_some();
            assert!(models.values().any(publishes_audio));
            if let Some(included) = &row.included_models {
                for model in included {
                    assert!(publishes_audio(&models[model]));
                }
            }
            for excluded in &row.excluded_models {
                assert!(publishes_audio(&models[&excluded.model_id]));
            }
        }
    }
}
