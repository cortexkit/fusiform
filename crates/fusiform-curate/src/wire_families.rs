//! Verified request protocols, derived on current reads without changing history.

use std::collections::BTreeMap;

pub const TOKENS: &[&str] = &[
    "anthropic.messages",
    "openai.responses",
    "openai.chat_completions",
    "openai.chatgpt_responses",
    "google.generate_content",
    "google.code_assist",
    "aws.bedrock_converse",
    // https://elevenlabs.io/docs/api-reference/text-to-speech/convert
    "elevenlabs.text_to_speech",
    // https://platform.openai.com/docs/models/gpt-image-2.6-flare
    "openai.images",
    // https://ai.google.dev/gemini-api/docs/omni
    "google.interactions",
];

pub fn is_known(token: &str) -> bool {
    TOKENS.contains(&token)
}

pub type Families = BTreeMap<(String, Option<String>), String>;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    npm: Vec<String>,
    shapes: Vec<Option<String>>,
    wire_family: String,
    basis: String,
}

/// Keep malformed curation observable while leaving unrelated rules usable.
pub fn load_reporting_rejects(doc: &str) -> (Families, Vec<String>) {
    let parsed: serde_json::Value = match serde_json::from_str(doc) {
        Ok(value) => value,
        Err(error) => {
            return (
                Families::new(),
                vec![format!("wire families JSON: {error}")],
            )
        }
    };
    let Some(rules) = parsed["rules"].as_array() else {
        return (
            Families::new(),
            vec!["wire families: missing rules array".into()],
        );
    };
    let mut families = Families::new();
    let mut rejects = Vec::new();
    for (index, raw) in rules.iter().enumerate() {
        let rule: Rule = match serde_json::from_value(raw.clone()) {
            Ok(rule) => rule,
            Err(error) => {
                rejects.push(format!("wire family rule {index}: {error}"));
                continue;
            }
        };
        let keys: Vec<_> = rule
            .npm
            .iter()
            .flat_map(|npm| {
                rule.shapes
                    .iter()
                    .map(move |shape| (npm.clone(), shape.clone()))
            })
            .collect();
        if !is_known(&rule.wire_family)
            || rule.basis.trim().is_empty()
            || rule.basis.contains(['\n', '\r'])
            || rule.npm.is_empty()
            || rule.shapes.is_empty()
            || rule.npm.iter().any(|npm| npm.trim().is_empty())
            || rule
                .shapes
                .iter()
                .flatten()
                .any(|shape| shape.trim().is_empty())
            || keys.iter().any(|key| families.contains_key(key))
            || keys.iter().collect::<std::collections::BTreeSet<_>>().len() != keys.len()
        {
            rejects.push(format!("wire family rule {index}: unknown token, invalid basis, empty or duplicate selector"));
            continue;
        }
        for key in keys {
            families.insert(key, rule.wire_family.clone());
        }
    }
    (families, rejects)
}

pub fn lookup<'a>(families: &'a Families, npm: &str, shape: Option<&str>) -> Option<&'a str> {
    families
        .get(&(npm.to_string(), shape.map(str::to_string)))
        .map(String::as_str)
}
