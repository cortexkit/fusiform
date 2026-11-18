//! The embedded wire families document and its local adapter.

pub use fusiform_curate::wire_families::*;

use std::sync::OnceLock;
pub fn families() -> &'static Families {
    static TABLE: OnceLock<Families> = OnceLock::new();
    TABLE.get_or_init(|| load_reporting_rejects(include_str!("../data/wire-families.json")).0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_rules_have_no_rejects() {
        let (families, rejects) =
            load_reporting_rejects(include_str!("../data/wire-families.json"));
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(families.len(), 14);
        // These selectors are verified by the source but not published in the
        // current seed, so seed-row witnesses cannot exercise them.
        assert_eq!(
            lookup(super::families(), "@ai-sdk/xai", Some("responses")),
            Some("openai.responses")
        );
        assert_eq!(
            lookup(super::families(), "@ai-sdk/openai", Some("completions")),
            Some("openai.chat_completions")
        );
        assert_eq!(
            lookup(super::families(), "@ai-sdk/xai", Some("completions")),
            None
        );
        assert_eq!(
            lookup(super::families(), "@ai-sdk/amazon-bedrock/mantle", None),
            None
        );
        assert_eq!(
            lookup(super::families(), "@ai-sdk/anthropic", Some("responses")),
            None
        );
    }

    #[test]
    fn unknown_tokens_and_malformed_rules_are_counted() {
        let (families, rejects) = load_reporting_rejects(
            r#"{"rules":[
            {"npm":["a"],"shapes":[null],"wire_family":"unknown","basis":"source"},
            {"npm":["b"],"shapes":[null],"wire_family":"openai.responses","basis":""},
            {"npm":["c"],"shapes":[null],"wire_family":"openai.responses","basis":"source"},
            {"npm":["c"],"shapes":[null],"wire_family":"anthropic.messages","basis":"source"},
            {}
        ]}"#,
        );
        assert_eq!(families.len(), 1);
        assert_eq!(rejects.len(), 4);
        assert_eq!(load_reporting_rejects("not JSON").1.len(), 1);
        assert_eq!(load_reporting_rejects("{}").1.len(), 1);
        assert_eq!(
            lookup(super::families(), "@ai-sdk/openai", Some("unknown")),
            None
        );
    }
}
