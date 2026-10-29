//! Routing's curated diversity groups, joined to current rows without storage.

use std::collections::BTreeMap;

const DOCUMENT: &str = include_str!("../data/model-lineage.json");
const LINEAGES: &[&str] = &[
    "claude", "gpt", "gemini", "grok", "qwen", "kimi", "glm", "deepseek", "minimax", "mistral",
    "llama",
];
const LABS: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "xai",
    "alibaba",
    "moonshotai",
    "zhipuai",
    "deepseek",
    "minimax",
    "mistral",
    "meta",
];

#[derive(Debug, Default)]
pub struct ModelLineage {
    models: BTreeMap<String, String>,
    families: BTreeMap<String, String>,
    labs: BTreeMap<String, String>,
    providers: BTreeMap<String, String>,
}

impl ModelLineage {
    pub fn lookup(
        &self,
        model: &str,
        family: Option<&str>,
        canonical: Option<&str>,
        alias: bool,
    ) -> Option<&str> {
        if let Some(lineage) = self.models.get(model) {
            return Some(lineage);
        }
        // Alias families are copied from the target. Only the alias's explicit
        // classification or served canonical identity establishes its lineage.
        if !alias {
            if let Some(family) = family {
                return self.families.get(family).map(String::as_str);
            }
        }
        if let Some(canonical) = canonical {
            return canonical
                .split_once('/')
                .and_then(|(lab, _)| self.labs.get(lab))
                .map(String::as_str);
        }
        if alias {
            return None;
        }
        // With no upstream classification or canonical identity, only a
        // first-party provider establishes origin; reseller names do not.
        model
            .split_once('/')
            .and_then(|(provider, _)| self.providers.get(provider))
            .map(String::as_str)
    }
}

/// Reject invalid entries independently, so one bad classification cannot hide
/// the remaining valid entries. Unknown lineage strings are never served.
pub fn load_reporting_rejects(doc: &str) -> (ModelLineage, Vec<String>) {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Doc {
        models: serde_json::Value,
        families: serde_json::Value,
        labs: serde_json::Value,
        providers: serde_json::Value,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Map {
        purpose: String,
        entries: BTreeMap<String, serde_json::Value>,
    }
    let doc: Doc = match serde_json::from_str(doc) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                ModelLineage::default(),
                vec![format!("model lineage unreadable: {error}")],
            )
        }
    };
    let mut rows = ModelLineage::default();
    let mut rejects = Vec::new();
    for (name, raw, destination) in [
        ("models", doc.models, &mut rows.models),
        ("families", doc.families, &mut rows.families),
        ("labs", doc.labs, &mut rows.labs),
        ("providers", doc.providers, &mut rows.providers),
    ] {
        let map: Map = match serde_json::from_value(raw) {
            Ok(map) => map,
            Err(error) => {
                rejects.push(format!("model lineage {name} did not parse: {error}"));
                continue;
            }
        };
        if map.purpose.trim().is_empty() {
            rejects.push(format!("model lineage {name} requires a purpose"));
            continue;
        }
        for (key, value) in map.entries {
            let valid_key = !key.is_empty()
                && !key.chars().any(char::is_whitespace)
                && match name {
                    "models" => key
                        .split_once('/')
                        .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty()),
                    "families" => !key.contains('/'),
                    "labs" => LABS.contains(&key.as_str()),
                    "providers" => LABS.contains(&key.as_str()) || key == "zai",
                    _ => unreachable!(),
                };
            if !valid_key || !value.as_str().is_some_and(|v| LINEAGES.contains(&v)) {
                rejects.push(format!(
                    "model lineage {name}/{key}: invalid key or lineage"
                ));
            } else {
                destination.insert(key, value.as_str().unwrap().into());
            }
        }
    }
    (rows, rejects)
}

pub fn classifications() -> &'static ModelLineage {
    #[cfg(test)]
    if let Some(rows) = PLANTED.with(|planted| planted.get()) {
        return rows;
    }
    static ROWS: std::sync::OnceLock<ModelLineage> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| load_reporting_rejects(DOCUMENT).0)
}

#[cfg(test)]
thread_local! {
    static PLANTED: std::cell::Cell<Option<&'static ModelLineage>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
#[must_use]
pub struct Planted {
    previous: Option<&'static ModelLineage>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

/// Use fixture classifications on the calling thread until the guard is dropped.
#[cfg(test)]
pub fn plant(doc: &str) -> Planted {
    let (rows, rejects) = load_reporting_rejects(doc);
    assert!(rejects.is_empty(), "{rejects:?}");
    let rows = Box::leak(Box::new(rows));
    Planted {
        previous: PLANTED.with(|planted| planted.replace(Some(rows))),
        _thread_bound: std::marker::PhantomData,
    }
}

#[cfg(test)]
impl Drop for Planted {
    fn drop(&mut self) {
        PLANTED.with(|planted| planted.set(self.previous));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shipped_classifications_load_without_rejects() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(rows.families.len(), 67);
        assert_eq!(rows.providers.len(), 12);
    }

    #[test]
    fn lookup_precedence_and_conservative_absence() {
        let rows = classifications();
        assert_eq!(
            rows.lookup(
                "google/antigravity-claude-opus-5-5",
                Some("gemini"),
                Some("google/anything"),
                true
            ),
            Some("claude")
        );
        assert_eq!(
            rows.lookup("reseller/id", Some("glm"), Some("openai/id"), false),
            Some("glm")
        );
        assert_eq!(
            rows.lookup("reseller/id", None, Some("google/id"), false),
            Some("gemini")
        );
        assert_eq!(rows.lookup("zai/id", None, None, false), Some("glm"));
        assert_eq!(
            rows.lookup("openai/id", Some("whisper"), Some("openai/id"), false),
            None
        );
        assert_eq!(
            rows.lookup("openai/id", None, Some("unknown/id"), false),
            None
        );
        assert_eq!(rows.lookup("reseller/gpt-id", None, None, false), None);
        assert_eq!(rows.lookup("openai/id", None, None, true), None);
        assert_eq!(rows.lookup("reseller/id", Some("glm"), None, true), None);
        assert_eq!(
            rows.lookup("reseller/id", Some("glm"), Some("openai/id"), true),
            Some("gpt")
        );
    }

    #[test]
    fn invalid_entries_and_maps_are_counted_without_discarding_valid_entries() {
        let mut doc: serde_json::Value = serde_json::from_str(DOCUMENT).unwrap();
        doc["models"]["entries"] = json!({"bad":"gpt", "p/m":"other", "p/n":3});
        doc["families"]["entries"] = json!({"glm":"glm", "bad family":"glm"});
        doc["labs"]["entries"] = json!({"reseller":"gpt"});
        doc["providers"]["entries"] = json!({"reseller":"gpt"});
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rejects.len(), 6);
        assert_eq!(rows.lookup("p/m", Some("glm"), None, false), Some("glm"));
        assert_eq!(rows.lookup("p/m", None, None, false), None);
        doc["models"]["purpose"] = json!("");
        doc["labs"] = json!(false);
        let (_, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rejects.len(), 4);
        let (_, rejects) = load_reporting_rejects("not json");
        assert_eq!(rejects.len(), 1);
    }
}
