use std::collections::BTreeMap;

use crate::{
    aliases::Alias, curated_models::CuratedModel, login_methods::Rows, narrow_to_requested, Facts,
    ServedState, RATE,
};
use fusiform_protocol::CatalogGetResponse;

/// Project the target's rendered facts under the alias identity.
pub fn project_alias(
    mut response: ServedState,
    provider_id: &str,
    model_id: &str,
    alias: &Alias,
    prefixes: Option<&Vec<String>>,
    provider_name: Option<&str>,
) -> Option<ServedState> {
    let target_key = format!("{}/{}", alias.target_provider_id, alias.target_model_id);
    let mut facts = response.models.remove(&target_key)?;
    mark_rates_borrowed_through_alias(&mut facts, alias);
    omit_route_dependent_alias_facts(&mut facts);
    response.wire_corrections.clear();
    emit_alias_route_facts(&mut facts, alias);
    add_provider_name(provider_name, &mut facts);
    narrow_to_requested(&mut facts, prefixes);
    if !facts.contains_key("model.canonical_id") {
        response.canonical_links.clear();
    }

    let alias_key = format!("{provider_id}/{model_id}");
    // Inherited disclosures keep the target as their source when its value
    // survives projection onto the alias.
    response.fact_overrides.retain_mut(|row| {
        let Some(served) = facts.get(&row.fact_key) else {
            return false;
        };
        if served == &row.value {
            return true;
        }
        // Alias prices acquire a borrowed-rate marker above. Keep the
        // source disclosure when that marker is the only difference, and
        // disclose the actual served value rather than the unmarked price.
        if row.fact_key.starts_with(RATE) {
            let mut unmarked = served.clone();
            if let Some(object) = unmarked.as_object_mut() {
                object.remove("inherited_from");
            }
            if unmarked == row.value {
                row.value = served.clone();
                return true;
            }
        }
        false
    });
    for row in &mut response.fact_overrides {
        row.model = alias_key.clone();
    }
    response.models.insert(alias_key.clone(), facts);
    response.aliased.push(fusiform_protocol::AliasedModelWire {
        model: alias_key,
        target_provider_id: alias.target_provider_id.clone(),
        target_model_id: alias.target_model_id.clone(),
        source_ref: alias.source_ref.clone(),
    });
    Some(response)
}

/// Route-dependent claims do not necessarily survive routing through an alias.
/// Whether a route enforces JSON schema depends on the route's own behavior,
/// which the target model's structured-output claim cannot establish; the same
/// holds for which request parameters, such as server-side fallback, the
/// route's backend accepts.
/// Its client package, endpoint and request shape likewise describe the target
/// backend, not the backend reached through the alias.
fn omit_route_dependent_alias_facts(facts: &mut BTreeMap<String, serde_json::Value>) {
    // The target's labels identify a different route and possibly a different
    // provider. Only the alias's own sourced labels can describe this row.
    facts.remove("model.name");
    facts.remove("provider.name");
    facts.retain(|key, _| !key.starts_with("route."));
    facts.remove("capability.structured_output");
    // A false target claim safely tells a runner to omit temperature on any
    // route. A true claim cannot establish acceptance on a different API.
    if facts.get("capability.temperature") != Some(&serde_json::Value::Bool(false)) {
        facts.remove("capability.temperature");
    }
    facts.remove("limit.temperature_min");
    facts.remove("limit.temperature_max");
    // Anthropic documents server-side fallback only on the Claude API and on
    // Claude Models on Google Cloud (Anthropic managed). A route reaching the
    // model through another serving path, such as Antigravity's Google-managed
    // Vertex backend, is not established to accept the `fallbacks` parameter.
    facts.remove("capability.server_side_fallback");
    // The vendor's permitted fallback targets describe its own API. Route-level
    // refusal targets are already dropped with every other `route.` fact above.
    facts.remove("capability.allowed_fallback_models");
    // Whether an effort change keeps the cache depends on the route's own
    // request form (an Antigravity level is its own wire model id), so a
    // target's documented answer does not carry over.
    facts.remove("capability.effort_change_cache");
    // Claude API message support does not establish what a Code Assist alias
    // accepts, even when it routes to the same model.
    facts.remove("capability.mid_conversation_system");
    // A vendor's documented default effort describes its own API. A route
    // reaching the model another way may apply a different default or map
    // efforts differently; an alias states its own in route.reasoning_default.
    facts.remove("capability.reasoning_default");
    facts.remove("wire.npm");
    facts.remove("wire.api");
    facts.remove("wire.shape");
    facts.remove("wire.family");
}

fn emit_alias_route_facts(
    facts: &mut BTreeMap<String, serde_json::Value>,
    alias: &crate::aliases::Alias,
) {
    if let Some(family) = &alias.wire_family {
        facts.insert("wire.family".to_string(), serde_json::json!(family));
    }
    if let Some(name) = &alias.model_name {
        facts.insert("model.name".to_string(), serde_json::json!(name));
    }
    if !alias.reasoning_wire_ids.is_empty() {
        // A target may offer more efforts than this route can send. Retain its
        // option shape and value order, but never advertise an unmapped effort.
        if let Some(options) = facts
            .get_mut("capability.reasoning_options")
            .and_then(serde_json::Value::as_array_mut)
        {
            for option in options {
                if option["type"] != "effort" {
                    continue;
                }
                let Some(values) = option["values"].as_array_mut() else {
                    continue;
                };
                values.retain(|value| {
                    value
                        .as_str()
                        .is_some_and(|effort| alias.reasoning_wire_ids.contains_key(effort))
                });
            }
        }
        facts.insert(
            "route.reasoning_wire_ids".to_string(),
            serde_json::json!(alias.reasoning_wire_ids),
        );
    }
    if let Some(default) = &alias.reasoning_default {
        facts.insert(
            "route.reasoning_default".to_string(),
            serde_json::json!(default),
        );
    }
}

/// Mark every rate on an aliased entry as the target's, not the route's.
///
/// The price is the target's API list price, which a consumer uses to relate
/// a subscription route to API cost; read as the route's own published price
/// it would bill calls that may cost nothing at the margin.
///
/// A rate the target itself inherited keeps its ORIGINAL marker: that marker
/// names the provider whose price it really is, and restamping it would hide
/// the first hop.
pub(crate) fn mark_rates_borrowed_through_alias(
    facts: &mut BTreeMap<String, serde_json::Value>,
    alias: &crate::aliases::Alias,
) {
    let family = facts
        .get("model.family")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let marker = serde_json::to_value(fusiform_protocol::money::InheritedFrom {
        provider_id: alias.target_provider_id.clone(),
        family,
        basis: "alias".to_string(),
        model_id: Some(alias.target_model_id.clone()),
        mapping: None,
    })
    .expect("a struct of strings serialises");
    for (key, value) in facts.iter_mut() {
        if !key.starts_with(RATE) {
            continue;
        }
        let Some(obj) = value.as_object_mut() else {
            continue;
        };
        if obj.contains_key("inherited_from") {
            continue;
        }
        obj.insert("inherited_from".to_string(), marker.clone());
    }
}

/// Merge a projected alias, sharing target disclosures already in the response.
pub fn merge_alias(response: &mut ServedState, entry: ServedState) {
    let CatalogGetResponse {
        models,
        aliased,
        withheld,
        uncertain,
        canonical_links,
        fact_overrides,
        ..
    } = entry;
    if models.values().all(|facts| facts.is_empty()) {
        return;
    }
    response.models.extend(models);
    response.aliased.extend(aliased);
    response.fact_overrides.extend(fact_overrides);
    for link in canonical_links {
        if !response.canonical_links.contains(&link) {
            response.canonical_links.push(link);
        }
    }
    for w in withheld {
        if !response.withheld.contains(&w) {
            response.withheld.push(w);
        }
    }
    for u in uncertain {
        if !response.uncertain.contains(&u) {
            response.uncertain.push(u);
        }
    }
}

/// An observed id (including a withdrawal) always takes precedence over an alias.
pub fn alias_applies(upstream_known: bool) -> bool {
    !upstream_known
}

/// An active upstream offer wins; a withdrawal leaves room for curation.
pub fn curated_applies(upstream_present: bool, held: bool) -> bool {
    !upstream_present && !held
}

/// A held row or a retired alias is only a curated retirement when there is no
/// upstream record for the same id.
pub fn retirement_applies(upstream_known: bool) -> bool {
    !upstream_known
}

pub fn apply_curated_row(
    response: &mut ServedState,
    row: &CuratedModel,
    provider_name: Option<&str>,
    auth_method: Option<&str>,
    prefixes: Option<&Vec<String>>,
    named: bool,
) {
    let id = format!("{}/{}", row.provider_id, row.model_id);
    // The curated row replaces a withdrawn upstream row whole. A
    // retired-inclusive read rendered that row and its disclosures above;
    // drop them so no stored fact or disclosure of the withdrawn row
    // survives beside the curated one, even when a login or fact filter
    // then leaves the curated row out.
    response.models.remove(&id);
    response.withheld.retain(|w| w.model != id);
    response
        .uncertain
        .retain(|u| u.provider_id != row.provider_id || u.model_id != row.model_id);
    response.canonical_links.retain(|link| link.model != id);
    // Endpoint corrections and completeness withholding describe the
    // withdrawn upstream row, not the curated one.
    response
        .wire_corrections
        .retain(|c| c.provider != row.provider_id || c.model != row.model_id);
    // Remove fact-override disclosures too: they describe the withdrawn
    // upstream row, while the curated row's facts are fusiform's own and
    // replace no upstream value.
    response.fact_overrides.retain(|o| o.model != id);
    // A plain read lists availability; a plane read must respect the login
    // restriction even when the provider has other unrestricted rows.
    if auth_method.is_some_and(|method| !row.auth_methods.iter().any(|m| m == method)) {
        return;
    }
    let mut facts = row.facts.clone();
    add_provider_name(provider_name, &mut facts);
    narrow_to_requested(&mut facts, prefixes);
    if !named && facts.is_empty() {
        return;
    }
    let disclosure = curated_disclosure(row);
    response.models.insert(disclosure.model.clone(), facts);
    response.curated.push(disclosure);
}

pub fn add_provider_name(provider_name: Option<&str>, facts: &mut Facts) {
    if let Some(name) = provider_name {
        facts
            .entry("provider.name".into())
            .or_insert_with(|| serde_json::json!(name));
    }
}

pub fn apply_retired_id(
    response: &mut ServedState,
    identity: (&str, &str),
    retired_at_ms: i64,
    successor: Option<&String>,
    include_retired: bool,
    named: bool,
    prefixes: Option<&Vec<String>>,
) {
    let (provider, model) = identity;
    let id = format!("{provider}/{model}");
    if response.models.contains_key(&id) {
        return;
    }
    if include_retired {
        // The same shape as a withdrawn upstream model: the identity is
        // present and its existence fact says absent.
        let mut facts = BTreeMap::from([(
            "existence".to_string(),
            serde_json::Value::String("absent".to_string()),
        )]);
        if let Some(successor) = successor {
            facts.insert("successor".to_string(), serde_json::json!(successor));
        }
        narrow_to_requested(&mut facts, prefixes);
        if named || !facts.is_empty() {
            response.models.insert(id, facts);
        }
    } else if named {
        response.retired.push(fusiform_protocol::RetiredModelWire {
            model: id,
            retired_at_ms,
            successor: successor.cloned(),
        });
    }
}

pub fn curated_disclosure(
    row: &crate::curated_models::CuratedModel,
) -> fusiform_protocol::CuratedModelWire {
    fusiform_protocol::CuratedModelWire {
        model: format!("{}/{}", row.provider_id, row.model_id),
        auth_methods: row.auth_methods.clone(),
        source_ref: row.source_ref.clone(),
        review_by_ms: row.review_by_ms,
    }
}

/// Login evidence follows the upstream identity, never an alias of that row.
pub fn apply_login_methods(rows: &Rows, response: &mut CatalogGetResponse, method: Option<&str>) {
    for row in rows.values() {
        let id = format!("{}/{}", row.provider_id, row.model_id);
        if !response.models.contains_key(&id)
            || response.aliased.iter().any(|alias| alias.model == id)
            || response.curated.iter().any(|curated| curated.model == id)
        {
            continue;
        }
        if method.is_some_and(|method| !row.auth_methods.iter().any(|m| m == method)) {
            response.models.remove(&id);
            response.canonical_links.retain(|link| link.model != id);
            response.fact_overrides.retain(|claim| claim.model != id);
            response
                .wire_corrections
                .retain(|claim| claim.provider != row.provider_id || claim.model != row.model_id);
            response.withheld.retain(|claim| claim.model != id);
            response.uncertain.retain(|claim| {
                claim.provider_id != row.provider_id || claim.model_id != row.model_id
            });
            continue;
        }
        response
            .login_methods
            .push(fusiform_protocol::LoginMethodsWire {
                model: id,
                auth_methods: row.auth_methods.clone(),
                source_ref: row.source_ref.clone(),
                review_by_ms: row.review_by_ms,
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_effort_restriction_preserves_option_shapes_and_order() {
        let (aliases, _) = crate::aliases::load_reporting_rejects(include_str!(
            "../../fusiform-module/data/model-aliases.json"
        ));
        let mut alias = aliases[&(
            "google".to_string(),
            "antigravity-gemini-3.8-flash".to_string(),
        )]
            .clone();
        alias.reasoning_wire_ids = BTreeMap::from([
            ("low".to_string(), "custom-low".to_string()),
            ("high".to_string(), "custom-high".to_string()),
        ]);
        alias.reasoning_default = Some("low".to_string());
        let original = serde_json::json!([
            {"type":"effort","values":["max","high","medium","low","xhigh"]},
            {"type":"budget_tokens","min":1024,"max":8192},
            {"type":"toggle","values":[true,false]},
        ]);
        let mut facts =
            BTreeMap::from([("capability.reasoning_options".to_string(), original.clone())]);
        emit_alias_route_facts(&mut facts, &alias);
        assert_eq!(
            facts["capability.reasoning_options"],
            serde_json::json!([
                {"type":"effort","values":["high","low"]},
                {"type":"budget_tokens","min":1024,"max":8192},
                {"type":"toggle","values":[true,false]},
            ])
        );

        // Without a tier map the route makes no narrower effort claim.
        alias.reasoning_wire_ids.clear();
        alias.reasoning_default = None;
        facts.insert("capability.reasoning_options".to_string(), original.clone());
        emit_alias_route_facts(&mut facts, &alias);
        assert_eq!(facts["capability.reasoning_options"], original);
    }

    #[test]
    fn planted_target_route_facts_are_not_projected_onto_an_alias() {
        let mut projected = BTreeMap::from([
            (
                "route.reasoning_wire_ids".to_string(),
                serde_json::json!({"high":"target-only"}),
            ),
            (
                "route.reasoning_default".to_string(),
                serde_json::json!("high"),
            ),
            ("limit.context".to_string(), serde_json::json!(1000)),
        ]);
        omit_route_dependent_alias_facts(&mut projected);
        assert_eq!(
            projected,
            BTreeMap::from([("limit.context".to_string(), serde_json::json!(1000))])
        );
    }

    /// A rate the target itself inherited keeps the marker naming the real
    /// origin, rather than being restamped as the target's own price.
    #[test]
    fn a_target_rate_that_was_already_inherited_keeps_its_marker() {
        let alias = crate::aliases::Alias {
            target_provider_id: "ollama-cloud".to_string(),
            target_model_id: "glm-x".to_string(),
            model_name: None,
            wire_family: None,
            basis: "b".to_string(),
            source_ref: "s".to_string(),
            established_at_ms: 1,
            review_by_ms: 2,
            reasoning_wire_ids: BTreeMap::new(),
            reasoning_default: None,
        };
        let original =
            serde_json::json!({"provider_id":"zai","family":"glm","basis":"open_weights"});
        let mut facts = BTreeMap::from([
            ("model.family".to_string(), serde_json::json!("glm")),
            (
                "rate.input".to_string(),
                serde_json::json!({"state":"priced","units":1,"inherited_from": original.clone()}),
            ),
            (
                "rate.output".to_string(),
                serde_json::json!({"state":"priced","units":2}),
            ),
        ]);
        mark_rates_borrowed_through_alias(&mut facts, &alias);
        assert_eq!(facts["rate.input"]["inherited_from"], original);
        assert_eq!(facts["rate.output"]["inherited_from"]["basis"], "alias");
        assert!(
            facts["model.family"].get("inherited_from").is_none(),
            "only rates are marked"
        );
    }
}
