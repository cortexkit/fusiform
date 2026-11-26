//! The embedded billing planes document and its local adapter.

pub use fusiform_curate::billing_planes::*;

#[cfg(test)]
use fusiform_core::TokenClass;
use std::sync::OnceLock;
const BILLING_PLANES: &str = include_str!("../data/billing-planes.json");

#[cfg(test)]
// The plane cites both billing and protocol evidence; rules cite only billing.
pub(crate) const OPENAI_CHATGPT_SOURCE: &str = "Billing: https://help.openai.com/en/articles/20001106. Protocol: OpenAI Codex at 708e3e2c70142c75682a375669ca60754f76976d, https://github.com/openai/codex/blob/708e3e2c70142c75682a375669ca60754f76976d/codex-rs/model-provider-info/src/lib.rs#L85 defines CHATGPT_CODEX_BASE_URL as https://chatgpt.com/backend-api/codex; #L151-L159 and #L467-L486 select it for AuthMode::Chatgpt. https://github.com/openai/codex/blob/708e3e2c70142c75682a375669ca60754f76976d/codex-rs/codex-api/src/endpoint/responses.rs#L135-L150 builds a POST to /responses; codex-rs/cli/src/doctor.rs#L3943-L3952 asserts https://chatgpt.com/backend-api/codex/responses.";
/// The compiled-in table, parsed once per process.
pub fn planes() -> &'static Planes {
    #[cfg(test)]
    if let Some(planes) = PLANTED.with(|slot| slot.get()) {
        return planes;
    }
    static PARSED: OnceLock<Planes> = OnceLock::new();
    PARSED.get_or_init(|| load_reporting_rejects(BILLING_PLANES).0)
}

#[cfg(test)]
thread_local! {
    static PLANTED: std::cell::Cell<Option<&'static Planes>> = const { std::cell::Cell::new(None) };
}

/// Exercise a declared Antigravity plane without making a production billing claim.
#[cfg(test)]
pub(crate) fn plant_antigravity() -> Planted {
    let mut planes = planes().clone();
    let (fixture, rejects) = load_reporting_rejects(
        r#"{"planes":[{"provider_id":"google","auth_method":"antigravity","kind":"upstream",
        "source_ref":"https://example.test/plane","established_by":"reader",
        "established_at_ms":1,"review_by_ms":1000,"rules":[]}]}"#,
    );
    assert!(rejects.is_empty(), "{rejects:?}");
    planes.extend(fixture);
    let planes = Box::leak(Box::new(planes));
    Planted {
        previous: PLANTED.with(|slot| slot.replace(Some(planes))),
        _thread_bound: std::marker::PhantomData,
    }
}

#[cfg(test)]
#[must_use]
pub(crate) struct Planted {
    previous: Option<&'static Planes>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(test)]
impl Drop for Planted {
    fn drop(&mut self) {
        PLANTED.with(|slot| slot.set(self.previous));
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
        assert_eq!(table.len(), 8, "an empty or truncated table must not pass");
        assert_eq!(&table, planes());
        assert!(std::ptr::eq(planes(), planes()), "the table is parsed once");
    }

    #[test]
    fn kimi_apikey_is_a_sourced_cross_provider_quota_stand_in() {
        let plane = &planes()[&("kimi-code-plan-global".into(), "apikey".into())];
        assert_eq!(plane.kind, PlaneKind::QuotaProxy);
        assert_eq!(plane.rates_from_provider.as_deref(), Some("moonshotai"));
        assert_eq!(plane.borrows_rates_from(), Some("moonshotai"));
        assert!(plane.rules.is_empty());
        assert_eq!(
            plane.source_ref,
            "https://www.kimi.com/code/docs/en/kimi-code/membership.html"
        );
        assert_eq!(plane.established_by, "fusiform");
        assert_eq!(plane.established_at_ms, 1_792_368_000_000);
        assert_eq!(plane.review_by_ms, 1_797_552_000_000);
        assert_eq!(
            plane.review_by_ms - plane.established_at_ms,
            60 * 86_400_000
        );
    }

    #[test]
    fn opencode_go_apikey_is_a_sourced_own_rate_quota_stand_in() {
        let plane = &planes()[&("opencode-go".into(), "apikey".into())];
        assert_eq!(plane.kind, PlaneKind::QuotaProxy);
        // The apikey guard needs an explicit price source; naming the plane's
        // own provider states that its own rates are the weights.
        assert_eq!(plane.rates_from_provider.as_deref(), Some("opencode-go"));
        assert_eq!(plane.borrows_rates_from(), None);
        assert!(plane.rules.is_empty());
        assert_eq!(plane.source_ref, "https://opencode.ai/v2/docs/console/go/");
        assert_eq!(plane.established_by, "fusiform");
        assert_eq!(plane.established_at_ms, 1_792_627_200_000); // 2026-10-22 UTC
        assert_eq!(plane.review_by_ms, 1_797_811_200_000); // 2026-12-21 UTC
        assert_eq!(
            plane.review_by_ms - plane.established_at_ms,
            60 * 86_400_000
        );
    }

    #[test]
    fn meta_oauth_is_a_sourced_ruleless_quota_stand_in() {
        let plane = &planes()[&("meta".into(), "oauth".into())];
        assert_eq!(plane.kind, PlaneKind::QuotaProxy);
        assert!(plane.rules.is_empty());
        assert!(plane
            .source_ref
            .contains("operator's Meta coding-plan login"));
        assert!(plane.source_ref.contains("vault credential oauth:meta"));
        assert!(plane.source_ref.contains("16:42:35Z"));
        assert!(plane.source_ref.contains("16:42:45Z"));
        assert!(plane
            .source_ref
            .contains("https://api.meta.ai/v1/responses"));
        assert_eq!(plane.established_by, "fusiform");
        assert_eq!(plane.established_at_ms, 1_791_849_600_000);
        assert_eq!(plane.review_by_ms, 1_797_033_600_000);
        assert_eq!(
            plane.review_by_ms - plane.established_at_ms,
            60 * 86_400_000
        );
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
                "https://docs.x.ai/developers/advanced-api-usage/prompt-caching/usage-and-pricing",
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
        assert_eq!(planes().len(), expected.len() + 4);
        for (provider, auth, kind, source, count) in expected {
            let p = &planes()[&(provider.into(), auth.into())];
            assert_eq!(
                (p.provider_id.as_str(), p.auth_method.as_str()),
                (provider, auth)
            );
            assert_eq!(p.kind, kind);
            assert_eq!(
                p.source_ref,
                if provider == "openai" {
                    OPENAI_CHATGPT_SOURCE
                } else {
                    source
                }
            );
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
    fn anthropic_gateway_is_a_sourced_ruleless_upstream_card() {
        let p = &planes()[&("anthropic".into(), "gateway".into())];
        assert_eq!(p.kind, PlaneKind::Upstream);
        assert_eq!(p.wire_family.as_deref(), Some("anthropic.messages"));
        assert_eq!(p.rates_from_provider, None);
        assert_eq!(
            p.source_ref,
            "https://platform.claude.com/docs/en/about-claude/pricing"
        );
        assert_eq!(p.established_by, "fusiform");
        assert_eq!(p.established_at_ms, 1_794_355_200_000);
        assert_eq!(p.review_by_ms, 1_799_539_200_000);
        assert_eq!(p.review_by_ms - p.established_at_ms, 60 * 86_400_000);
        assert!(p.rules.is_empty());
        assert!(!planes().contains_key(&("anthropic".into(), "apikey".into())));
        for provider in ["openai", "xai"] {
            assert!(!planes().contains_key(&(provider.into(), "gateway".into())));
        }
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
        assert_eq!(
            AUTH_METHODS,
            &["apikey", "chatgpt", "oauth", "antigravity", "gateway"]
        );
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
    fn a_quota_proxy_on_apikey_without_a_price_provider_is_rejected_with_position() {
        let mut bad = plane("bad", vec![]);
        bad["auth_method"] = json!("apikey");
        let (table, rejects) = load(vec![plane("good", vec![]), bad]);
        assert_eq!(table.len(), 1);
        assert_reject(&rejects, "plane 1", "quota_proxy is not allowed on apikey");
    }

    #[test]
    fn a_price_provider_is_only_allowed_nonempty_on_a_quota_proxy() {
        for (kind, provider) in [("upstream", "moonshotai"), ("quota_proxy", " ")] {
            let mut bad = plane("bad", vec![]);
            bad["kind"] = json!(kind);
            bad["rates_from_provider"] = json!(provider);
            let (table, rejects) = load(vec![plane("good", vec![]), bad]);
            assert_eq!(table.len(), 1);
            assert_reject(
                &rejects,
                "plane 1",
                "rates_from_provider needs a non-empty provider on a quota_proxy plane",
            );
        }
        let mut good = plane("subscription", vec![]);
        good["auth_method"] = json!("apikey");
        good["rates_from_provider"] = json!("moonshotai");
        let (table, rejects) = load(vec![good]);
        assert!(rejects.is_empty());
        assert_eq!(
            table
                .values()
                .next()
                .unwrap()
                .rates_from_provider
                .as_deref(),
            Some("moonshotai")
        );
    }

    #[test]
    fn an_apikey_quota_proxy_may_name_its_own_provider_as_the_price_source() {
        let mut own = plane("own", vec![]);
        own["auth_method"] = json!("apikey");
        own["rates_from_provider"] = json!("own");
        let mut other = plane("other", vec![]);
        other["auth_method"] = json!("apikey");
        other["rates_from_provider"] = json!("moonshotai");
        let (table, rejects) = load(vec![own, other]);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(
            table[&("own".into(), "apikey".into())].borrows_rates_from(),
            None
        );
        assert_eq!(
            table[&("other".into(), "apikey".into())].borrows_rates_from(),
            Some("moonshotai")
        );
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

    /// Kept as an ordinary, never-ignored test so the nightly CI run catches a
    /// review date that lapses during a quiet week, when no push would run it.
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
