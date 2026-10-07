//! Curated billing claims for a provider's authentication planes.
//!
//! These claims are applied only when serving a current plane view, never at
//! ingest. A subscription's API rates are quota weights rather than a bill,
//! and a cache write billed as input is not the same thing as a free write.
//! Both distinctions need sources and independent review dates, including on
//! a plane that has no per-class rules.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use fusiform_core::TokenClass;

const BILLING_PLANES: &str = include_str!("../data/billing-planes.json");

/// The closed authentication vocabulary used to select a billing plane.
pub const AUTH_METHODS: &[&str] = &["apikey", "chatgpt", "oauth", "antigravity"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaneKind {
    Upstream,
    QuotaProxy,
}

impl PlaneKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upstream => "upstream",
            Self::QuotaProxy => "quota_proxy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleState {
    BilledAs,
    StatedZero,
    NotEstablished,
}

impl RuleState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BilledAs => "billed_as",
            Self::StatedZero => "stated_zero",
            Self::NotEstablished => "not_established",
        }
    }
}

/// A plane's claim about how a provider bills this authentication method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    pub provider_id: String,
    pub auth_method: String,
    pub kind: PlaneKind,
    pub source_ref: String,
    pub established_by: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
    pub rules: Vec<BillingRule>,
}

/// A per-class claim, with provenance independent of the plane's claim.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct BillingRule {
    pub class: TokenClass,
    pub state: RuleState,
    pub as_class: Option<TokenClass>,
    /// Absent means every model; an explicit list means only those exact ids.
    pub model_ids: Option<Vec<String>>,
    pub source_ref: String,
    pub established_by: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

impl BillingRule {
    pub fn applies_to(&self, model_id: &str) -> bool {
        self.model_ids
            .as_ref()
            .is_none_or(|ids| ids.iter().any(|id| id == model_id))
    }
}

/// `(provider_id, auth_method)` -> the declared billing plane.
pub type Planes = BTreeMap<(String, String), Plane>;

/// The compiled-in table, parsed once per process.
pub fn planes() -> &'static Planes {
    static PARSED: OnceLock<Planes> = OnceLock::new();
    PARSED.get_or_init(|| load_reporting_rejects(BILLING_PLANES).0)
}

#[derive(serde::Deserialize)]
struct PlaneRow {
    provider_id: String,
    auth_method: String,
    kind: PlaneKind,
    source_ref: String,
    established_by: String,
    established_at_ms: i64,
    review_by_ms: i64,
    #[serde(default)]
    rules: Vec<serde_json::Value>,
}

fn scopes_overlap(a: &BillingRule, b: &BillingRule) -> bool {
    match (&a.model_ids, &b.model_ids) {
        (None, None) => true,
        (Some(ids), None) | (None, Some(ids)) => !ids.is_empty(),
        (Some(a), Some(b)) => a.iter().any(|id| b.contains(id)),
    }
}

/// Parse a document, returning the table and one positioned reason per reject.
///
/// Malformed rows are non-fatal: this file is embedded, so zero-reject tests
/// catch curation mistakes before release without taking down the unrelated
/// catalog at runtime. A malformed rule is refused on its own; its plane and
/// other valid rules can still load. Document syntax errors are counted too.
pub fn load_reporting_rejects(doc: &str) -> (Planes, Vec<String>) {
    let parsed: serde_json::Value = match serde_json::from_str(doc) {
        Ok(v) => v,
        Err(e) => {
            return (
                Planes::new(),
                vec![format!("billing planes document is not JSON: {e}")],
            )
        }
    };
    let Some(raw_planes) = parsed["planes"].as_array() else {
        return (
            Planes::new(),
            vec!["billing planes document: no `planes` array".into()],
        );
    };

    let mut table = Planes::new();
    let mut positions = BTreeMap::new();
    let mut rejects = Vec::new();
    for (pi, raw) in raw_planes.iter().enumerate() {
        let at = |reason: &str| format!("plane {pi}: {reason}");
        let row: PlaneRow = match serde_json::from_value(raw.clone()) {
            Ok(row) => row,
            Err(e) => {
                rejects.push(at(&format!("malformed plane: {e}")));
                continue;
            }
        };
        if [&row.provider_id, &row.source_ref, &row.established_by]
            .iter()
            .any(|s| s.trim().is_empty())
        {
            rejects.push(at("empty identity or provenance field"));
            continue;
        }
        if !AUTH_METHODS.contains(&row.auth_method.as_str()) {
            rejects.push(at(&format!(
                "unknown auth_method {:?}; expected apikey, chatgpt, oauth or antigravity",
                row.auth_method
            )));
            continue;
        }
        if row.kind == PlaneKind::QuotaProxy && row.auth_method == "apikey" {
            rejects.push(at("quota_proxy is not allowed on apikey: an API price is upstream, not a subscription quota weight"));
            continue;
        }
        let key = (row.provider_id.clone(), row.auth_method.clone());
        if let Some(prior) = positions.get(&key) {
            rejects.push(at(&format!(
                "duplicates plane {prior}: {}/{} already has a declaration",
                key.0, key.1
            )));
            continue;
        }

        // Parse each rule separately, preserving original indices even if a
        // preceding rule was refused. A report must point into the authored
        // document, not into a shorter list of rows that happened to load.
        let mut candidates = Vec::new();
        for (ri, raw_rule) in row.rules.iter().enumerate() {
            let at_rule = |reason: &str| format!("plane {pi} rule {ri}: {reason}");
            let rule: BillingRule = match serde_json::from_value(raw_rule.clone()) {
                Ok(rule) => rule,
                Err(e) => {
                    rejects.push(at_rule(&format!("malformed rule: {e}")));
                    continue;
                }
            };
            if rule.source_ref.trim().is_empty() || rule.established_by.trim().is_empty() {
                rejects.push(at_rule("empty provenance field"));
                continue;
            }
            if rule.state == RuleState::BilledAs
                && (rule.as_class.is_none() || rule.as_class == Some(rule.class))
            {
                rejects.push(at_rule("billed_as needs as_class different from class"));
                continue;
            }
            if rule
                .model_ids
                .as_ref()
                .is_some_and(|ids| ids.iter().any(|id| id.trim().is_empty()))
            {
                rejects.push(at_rule("model_ids contains an empty id"));
                continue;
            }
            candidates.push((ri, rule));
        }

        let mut rules = Vec::new();
        for (ri, rule) in &candidates {
            let at_rule = |reason: &str| format!("plane {pi} rule {ri}: {reason}");
            if let Some((prior, _)) = candidates.iter().find(|(other_index, other)| {
                other_index < ri && other.class == rule.class && scopes_overlap(rule, other)
            }) {
                rejects.push(at_rule(&format!(
                    "overlaps rule {prior}: one class may have only one rule per model"
                )));
                continue;
            }
            // Inspect all candidates before removing any. Otherwise a forward
            // chain or a cycle could be accepted depending on file order, or
            // after its target rule was rejected for a different collision.
            if rule.state == RuleState::BilledAs {
                if let Some((target, _)) = candidates.iter().find(|(_, other)| {
                    Some(other.class) == rule.as_class && scopes_overlap(rule, other)
                }) {
                    rejects.push(at_rule(&format!("billed_as targets ruled class in rule {target} with overlapping model scope; chains are not allowed")));
                    continue;
                }
            }
            rules.push(rule.clone());
        }

        positions.insert(key.clone(), pi);
        table.insert(
            key,
            Plane {
                provider_id: row.provider_id,
                auth_method: row.auth_method,
                kind: row.kind,
                source_ref: row.source_ref,
                established_by: row.established_by,
                established_at_ms: row.established_at_ms,
                review_by_ms: row.review_by_ms,
                rules,
            },
        );
    }
    (table, rejects)
}

/// A plane or rule past its own review date, with enough detail to re-read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overdue {
    pub provider_id: String,
    pub auth_method: String,
    /// `None` names the plane itself, not an absent rule.
    pub rule_index: Option<usize>,
    pub class: Option<TokenClass>,
    pub days_overdue: i64,
    pub source_ref: String,
}

/// Find overdue claims at an explicit instant, including ruleless planes.
pub fn overdue_at(table: &Planes, now_ms: i64) -> Vec<Overdue> {
    let mut overdue = Vec::new();
    for plane in table.values() {
        if plane.review_by_ms < now_ms {
            overdue.push(Overdue {
                provider_id: plane.provider_id.clone(),
                auth_method: plane.auth_method.clone(),
                rule_index: None,
                class: None,
                days_overdue: now_ms.saturating_sub(plane.review_by_ms) / 86_400_000,
                source_ref: plane.source_ref.clone(),
            });
        }
        for (ri, rule) in plane.rules.iter().enumerate() {
            if rule.review_by_ms < now_ms {
                overdue.push(Overdue {
                    provider_id: plane.provider_id.clone(),
                    auth_method: plane.auth_method.clone(),
                    rule_index: Some(ri),
                    class: Some(rule.class),
                    days_overdue: now_ms.saturating_sub(rule.review_by_ms) / 86_400_000,
                    source_ref: rule.source_ref.clone(),
                });
            }
        }
    }
    overdue
}

/// Explain why the date-triggered gate fired and how to resolve each claim.
pub fn overdue_report(overdue: &[Overdue]) -> String {
    let mut out = String::from(
        "NOT A BUILD FAILURE. Nothing changed; a review date passed.\n\n\
         These curated billing planes or rules are past their review date. \
         No upstream fetch confirms how a subscription bills or whether a \
         cache write still has no surcharge; the review is the signal that \
         these claims may have gone stale.\n\n",
    );
    for o in overdue {
        let claim = match (o.rule_index, o.class) {
            (Some(ri), Some(class)) => format!("rule {ri} ({})", class_name(class)),
            _ => "plane".into(),
        };
        out.push_str(&format!(
            "  {}/{} {claim} — {} day(s) overdue\n    read: {}\n",
            o.provider_id, o.auth_method, o.days_overdue, o.source_ref,
        ));
    }
    out.push_str(
        "\nOpen each source, re-read how this login or class bills, and commit \
         crates/fusiform-module/data/billing-planes.json:\n\
         \x20 - update or remove the plane or rule IF its claim changed\n\
         \x20 - record established_by, established_at_ms and review_by_ms \
         (60 days later) ALWAYS, on each claim reviewed\n\n\
         Confirming a claim unchanged is a real result. A ruleless quota proxy \
         must be reviewed too; reviewing a rule does not renew its plane.\n\n\
         What this gate CANNOT tell you: whether a claim is still correct. It \
         knows only that nobody has looked recently.\n",
    );
    out
}

/// A token class's spelling in rate keys and the curated document.
pub fn class_name(class: TokenClass) -> &'static str {
    match class {
        TokenClass::Input => "input",
        TokenClass::Output => "output",
        TokenClass::CacheRead => "cache_read",
        TokenClass::CacheWrite => "cache_write",
        TokenClass::Reasoning => "reasoning",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn plane(provider: &str, rules: Vec<Value>) -> Value {
        json!({
            "provider_id": provider, "auth_method": "oauth", "kind": "quota_proxy",
            "source_ref": "https://example.test/plane", "established_by": "reader",
            "established_at_ms": 1, "review_by_ms": 1000, "rules": rules
        })
    }

    fn rule(class: &str, state: &str) -> Value {
        json!({
            "class": class, "state": state,
            "source_ref": "https://example.test/rule", "established_by": "reader",
            "established_at_ms": 2, "review_by_ms": 1000
        })
    }

    fn billed_as(class: &str, as_class: &str) -> Value {
        let mut r = rule(class, "billed_as");
        r["as_class"] = json!(as_class);
        r
    }

    fn load(rows: Vec<Value>) -> (Planes, Vec<String>) {
        load_reporting_rejects(&json!({"planes": rows}).to_string())
    }

    fn assert_reject(rejects: &[String], position: &str, reason: &str) {
        assert_eq!(
            rejects.len(),
            1,
            "one rejected row must be counted: {rejects:?}"
        );
        assert!(rejects[0].contains(position), "{rejects:?}");
        assert!(rejects[0].contains(reason), "{rejects:?}");
    }

    #[test]
    fn the_shipped_file_has_no_rejects() {
        let (table, rejects) = load_reporting_rejects(BILLING_PLANES);
        assert!(
            rejects.is_empty(),
            "the shipped file must parse: {rejects:?}"
        );
        assert_eq!(table.len(), 4, "an empty or truncated table must not pass");
        assert_eq!(&table, planes());
        assert!(std::ptr::eq(planes(), planes()), "the table is parsed once");
    }

    #[test]
    fn the_shipped_claims_have_the_declared_sources_rules_and_sixty_day_reviews() {
        let expected = [
            (
                "openai",
                "chatgpt",
                PlaneKind::QuotaProxy,
                "https://help.openai.com/en/articles/20001106",
                1,
            ),
            (
                "anthropic",
                "oauth",
                PlaneKind::QuotaProxy,
                "https://support.anthropic.com/en/articles/9797557",
                0,
            ),
            (
                "xai",
                "apikey",
                PlaneKind::Upstream,
                "https://docs.x.ai/docs/guides/prompt-caching#usage-and-pricing",
                1,
            ),
            (
                "deepseek",
                "apikey",
                PlaneKind::Upstream,
                "https://api-docs.deepseek.com/api/create-chat-completion",
                1,
            ),
        ];
        assert_eq!(planes().len(), expected.len());
        for (provider, auth, kind, source, count) in expected {
            let p = &planes()[&(provider.into(), auth.into())];
            assert_eq!(
                (p.provider_id.as_str(), p.auth_method.as_str()),
                (provider, auth)
            );
            assert_eq!(p.kind, kind);
            assert_eq!(p.source_ref, source);
            assert_eq!(p.established_by, "fusiform");
            assert_eq!(p.established_at_ms, 1_791_331_200_000); // 2026-10-07 UTC
            assert_eq!(p.review_by_ms, 1_796_515_200_000); // 2026-12-06 UTC
            assert_eq!(p.review_by_ms - p.established_at_ms, 60 * 86_400_000);
            assert_eq!(p.rules.len(), count);
            for r in &p.rules {
                assert_eq!(r.class, TokenClass::CacheWrite);
                assert_eq!(r.state, RuleState::BilledAs);
                assert_eq!(r.as_class, Some(TokenClass::Input));
                assert_eq!(r.model_ids, None, "the shipped rules are plane-wide");
                assert_eq!(r.source_ref, source);
                assert_eq!(r.established_by, "fusiform");
                assert_eq!(r.established_at_ms, 1_791_331_200_000);
                assert_eq!(r.review_by_ms, 1_796_515_200_000);
                assert_eq!(r.review_by_ms - r.established_at_ms, 60 * 86_400_000);
            }
        }
        assert!(!planes().contains_key(&("google".into(), "antigravity".into())));
        assert!(planes()
            .values()
            .flat_map(|p| &p.rules)
            .all(|r| r.class != TokenClass::Reasoning));
    }

    #[test]
    fn unreadable_documents_and_malformed_rows_are_counted_not_fatal() {
        let (table, rejects) = load_reporting_rejects("{");
        assert!(table.is_empty());
        assert_reject(&rejects, "line 1 column 1", "not JSON");
        for doc in ["{}", "null", r#"{"planes":{}}"#] {
            let (table, rejects) = load_reporting_rejects(doc);
            assert!(table.is_empty());
            assert_reject(&rejects, "document", "no `planes` array");
        }
        let (table, rejects) = load(vec![json!(false), plane("valid", vec![])]);
        assert_eq!(
            table.len(),
            1,
            "a malformed row does not stop the next plane"
        );
        assert_reject(&rejects, "plane 0", "malformed plane");
        let (table, rejects) = load(vec![plane(
            "valid",
            vec![json!(false), rule("output", "stated_zero")],
        )]);
        assert_eq!(table.values().next().unwrap().rules.len(), 1);
        assert_reject(&rejects, "plane 0 rule 0", "malformed rule");
    }

    #[test]
    fn an_unknown_auth_method_is_rejected_with_position() {
        let mut bad = plane("bad", vec![]);
        bad["auth_method"] = json!("bogus");
        let (table, rejects) = load(vec![plane("good", vec![]), bad]);
        assert_eq!(table.len(), 1);
        assert_reject(&rejects, "plane 1", "unknown auth_method");
        assert_eq!(AUTH_METHODS, &["apikey", "chatgpt", "oauth", "antigravity"]);
        for auth in AUTH_METHODS {
            let mut good = plane("good", vec![]);
            good["auth_method"] = json!(auth);
            good["kind"] = json!("upstream");
            let (table, rejects) = load(vec![good]);
            assert!(rejects.is_empty(), "{rejects:?}");
            assert_eq!(table.len(), 1);
        }
    }

    #[test]
    fn an_unknown_kind_is_rejected_with_position() {
        let mut bad = plane("bad", vec![]);
        bad["kind"] = json!("subscription");
        let (table, rejects) = load(vec![plane("good", vec![]), bad]);
        assert_eq!(table.len(), 1);
        assert_reject(&rejects, "plane 1", "unknown variant `subscription`");
    }

    #[test]
    fn a_quota_proxy_on_apikey_is_rejected_with_position() {
        let mut bad = plane("bad", vec![]);
        bad["auth_method"] = json!("apikey");
        let (table, rejects) = load(vec![plane("good", vec![]), bad]);
        assert_eq!(table.len(), 1);
        assert_reject(&rejects, "plane 1", "quota_proxy is not allowed on apikey");
    }

    #[test]
    fn unknown_classes_and_states_are_rejected_with_position() {
        for (field, value) in [
            ("class", "embedding"),
            ("as_class", "embedding"),
            ("state", "priced"),
        ] {
            let mut bad = billed_as("cache_write", "input");
            bad[field] = json!(value);
            let (table, rejects) = load(vec![
                plane("good", vec![]),
                plane("bad", vec![rule("output", "stated_zero"), bad]),
            ]);
            assert_eq!(table.len(), 2);
            assert_eq!(table[&("bad".into(), "oauth".into())].rules.len(), 1);
            assert_reject(
                &rejects,
                "plane 1 rule 1",
                &format!("unknown variant `{value}`"),
            );
        }
    }

    #[test]
    fn billed_as_without_a_distinct_target_is_rejected_with_position() {
        for bad in [
            rule("cache_write", "billed_as"),
            billed_as("cache_write", "cache_write"),
        ] {
            let (table, rejects) = load(vec![
                plane("good", vec![]),
                plane("bad", vec![rule("output", "stated_zero"), bad]),
            ]);
            assert_eq!(table[&("bad".into(), "oauth".into())].rules.len(), 1);
            assert_reject(
                &rejects,
                "plane 1 rule 1",
                "billed_as needs as_class different from class",
            );
        }
    }

    #[test]
    fn duplicate_planes_name_the_original_document_position() {
        let good = plane("acme", vec![]);
        let mut duplicate = good.clone();
        duplicate["kind"] = json!("upstream");
        let (table, rejects) = load(vec![plane("another", vec![]), good, duplicate]);
        assert_eq!(table.len(), 2);
        assert_eq!(
            table[&("acme".into(), "oauth".into())].kind,
            PlaneKind::QuotaProxy
        );
        assert_reject(&rejects, "plane 2", "duplicates plane 1");
    }

    #[test]
    fn every_plane_provenance_field_is_required_with_position() {
        for field in [
            "source_ref",
            "established_by",
            "established_at_ms",
            "review_by_ms",
        ] {
            let mut bad = plane("bad", vec![]);
            bad.as_object_mut().unwrap().remove(field);
            let (table, rejects) = load(vec![plane("good", vec![]), bad]);
            assert_eq!(table.len(), 1);
            assert_reject(&rejects, "plane 1", &format!("missing field `{field}`"));
        }
    }

    #[test]
    fn every_rule_provenance_field_is_required_with_position() {
        for field in [
            "source_ref",
            "established_by",
            "established_at_ms",
            "review_by_ms",
        ] {
            let mut bad = rule("cache_write", "stated_zero");
            bad.as_object_mut().unwrap().remove(field);
            let (table, rejects) = load(vec![
                plane("good", vec![]),
                plane("bad", vec![rule("output", "stated_zero"), bad]),
            ]);
            assert_eq!(table[&("bad".into(), "oauth".into())].rules.len(), 1);
            assert_reject(
                &rejects,
                "plane 1 rule 1",
                &format!("missing field `{field}`"),
            );
        }
    }

    #[test]
    fn empty_provenance_is_rejected_on_planes_and_rules() {
        for field in ["source_ref", "established_by"] {
            let mut bad_plane = plane("bad", vec![]);
            bad_plane[field] = json!("  ");
            let (table, rejects) = load(vec![bad_plane]);
            assert!(table.is_empty());
            assert_reject(&rejects, "plane 0", "empty identity or provenance field");
            let mut bad_rule = rule("output", "stated_zero");
            bad_rule[field] = json!("");
            let (table, rejects) = load(vec![plane("bad", vec![bad_rule])]);
            assert!(table.values().next().unwrap().rules.is_empty());
            assert_reject(&rejects, "plane 0 rule 0", "empty provenance field");
        }
    }

    #[test]
    fn overlapping_rule_scopes_are_rejected_with_position() {
        let wide = rule("cache_write", "stated_zero");
        let mut narrow = rule("cache_write", "not_established");
        narrow["model_ids"] = json!(["a", "b"]);
        let mut partial = narrow.clone();
        partial["model_ids"] = json!(["b", "c"]);
        for pair in [
            vec![wide.clone(), narrow.clone()],
            vec![narrow.clone(), wide],
            vec![narrow.clone(), partial],
            vec![narrow.clone(), narrow],
        ] {
            let mut rules = vec![json!(false)];
            rules.extend(pair);
            let (table, rejects) = load(vec![plane("acme", rules)]);
            assert_eq!(table.values().next().unwrap().rules.len(), 1);
            assert_eq!(rejects.len(), 2, "{rejects:?}");
            assert!(rejects[0].contains("plane 0 rule 0"), "{rejects:?}");
            assert!(
                rejects[1].contains("plane 0 rule 2") && rejects[1].contains("overlaps rule 1"),
                "{rejects:?}"
            );
        }
    }

    #[test]
    fn disjoint_scopes_load_and_an_empty_scope_never_means_every_model() {
        let mut a = rule("cache_write", "stated_zero");
        a["model_ids"] = json!(["a"]);
        let mut b = billed_as("cache_write", "input");
        b["model_ids"] = json!(["b"]);
        let mut empty = rule("cache_write", "not_established");
        empty["model_ids"] = json!([]);
        let (table, rejects) = load(vec![plane(
            "acme",
            vec![
                a,
                b,
                empty,
                rule("output", "not_established"),
                rule("reasoning", "stated_zero"),
            ],
        )]);
        assert!(rejects.is_empty(), "{rejects:?}");
        let rules = &table.values().next().unwrap().rules;
        assert_eq!(rules.len(), 5);
        assert!(rules[0].applies_to("a"));
        assert!(!rules[0].applies_to("b"));
        assert!(rules[1].applies_to("b"));
        assert!(!rules[2].applies_to("a"));
        assert!(rules[3].applies_to("any-model"));
    }

    #[test]
    fn a_billed_as_target_with_any_overlapping_rule_is_rejected_in_either_order() {
        for target in [
            billed_as("input", "output"),
            rule("input", "stated_zero"),
            rule("input", "not_established"),
        ] {
            for reverse in [false, true] {
                let mut source = billed_as("cache_write", "input");
                source["model_ids"] = json!(["a", "b"]);
                let mut target = target.clone();
                target["model_ids"] = json!(["b", "c"]);
                let rules = if reverse {
                    vec![target, source]
                } else {
                    vec![source, target]
                };
                let (table, rejects) = load(vec![plane("acme", rules)]);
                let position = if reverse {
                    "plane 0 rule 1"
                } else {
                    "plane 0 rule 0"
                };
                assert_reject(&rejects, position, "chains are not allowed");
                assert_eq!(table.values().next().unwrap().rules.len(), 1);
                assert_eq!(
                    table.values().next().unwrap().rules[0].class,
                    TokenClass::Input
                );
            }
        }
        // Disjoint models do not chain: no row can receive both rules.
        let mut source = billed_as("cache_write", "input");
        source["model_ids"] = json!(["a"]);
        let mut target = rule("input", "stated_zero");
        target["model_ids"] = json!(["b"]);
        let (table, rejects) = load(vec![plane("acme", vec![source, target])]);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(table.values().next().unwrap().rules.len(), 2);
    }

    #[test]
    fn plane_wide_chains_and_cycles_are_refused_before_removing_candidates() {
        let (table, rejects) = load(vec![plane(
            "acme",
            vec![
                billed_as("cache_write", "input"),
                billed_as("input", "output"),
                rule("output", "stated_zero"),
            ],
        )]);
        assert_eq!(rejects.len(), 2, "{rejects:?}");
        assert!(rejects[0].contains("plane 0 rule 0"));
        assert!(rejects[1].contains("plane 0 rule 1"));
        assert_eq!(table.values().next().unwrap().rules.len(), 1);
        let (table, rejects) = load(vec![plane(
            "acme",
            vec![billed_as("input", "output"), billed_as("output", "input")],
        )]);
        assert_eq!(rejects.len(), 2, "{rejects:?}");
        assert!(table.values().next().unwrap().rules.is_empty());
    }

    #[test]
    fn malformed_model_scopes_are_counted_with_position() {
        for (ids, reason) in [
            (json!("a"), "expected a sequence"),
            (json!([1]), "expected a string"),
            (json!([" "]), "model_ids contains an empty id"),
        ] {
            let mut bad = rule("output", "stated_zero");
            bad["model_ids"] = ids;
            let (table, rejects) = load(vec![plane("acme", vec![bad])]);
            assert!(table.values().next().unwrap().rules.is_empty());
            assert_reject(&rejects, "plane 0 rule 0", reason);
        }
    }

    /// An unignored lib test: the scheduled CI workspace suite runs this even
    /// when there has been no push during the review window.
    #[test]
    fn no_plane_or_rule_is_past_its_review_date() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_millis() as i64;
        let overdue = overdue_at(planes(), now);
        assert!(overdue.is_empty(), "\n\n{}", overdue_report(&overdue));
    }

    #[test]
    fn overdue_planes_rules_and_ruleless_planes_are_independently_actionable() {
        let mut ruleless = plane("ruleless", vec![]);
        ruleless["review_by_ms"] = json!(999);
        let mut fresh_rule = rule("cache_read", "stated_zero");
        fresh_rule["review_by_ms"] = json!(999_999_999);
        let mut old_plane = plane("old-plane", vec![fresh_rule]);
        old_plane["review_by_ms"] = json!(999);
        let mut fresh_plane = plane("fresh-plane", vec![billed_as("cache_write", "input")]);
        fresh_plane["review_by_ms"] = json!(999_999_999);
        let (table, rejects) = load(vec![ruleless, old_plane, fresh_plane]);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert!(
            overdue_at(&table, 999).is_empty(),
            "the boundary is inclusive"
        );
        let overdue = overdue_at(&table, 1000 + 3 * 86_400_000);
        assert_eq!(overdue.len(), 3, "each claim has its own review clock");
        assert!(overdue.iter().all(|o| o.days_overdue == 3));
        let message = overdue_report(&overdue);
        for expected in [
            "NOT A BUILD FAILURE",
            "ruleless/oauth plane",
            "old-plane/oauth plane",
            "fresh-plane/oauth rule 0 (cache_write)",
            "3 day(s)",
            "https://example.test/plane",
            "https://example.test/rule",
            "billing-planes.json",
            "established_by",
            "established_at_ms",
            "review_by_ms",
            "60 days later",
            "CANNOT tell you",
        ] {
            assert!(
                message.contains(expected),
                "missing {expected:?}: {message}"
            );
        }
    }

    #[test]
    #[should_panic(expected = "acme/oauth plane")]
    fn an_overdue_ruleless_plane_fails_the_review_gate() {
        let (table, rejects) = load(vec![plane("acme", vec![])]);
        assert!(rejects.is_empty(), "{rejects:?}");
        let overdue = overdue_at(&table, 1001);
        assert!(overdue.is_empty(), "\n\n{}", overdue_report(&overdue));
    }
}
