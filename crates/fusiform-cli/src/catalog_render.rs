//! Text rendering for catalog reads.

use std::{collections::BTreeMap, fmt::Write as _};

use super::{count, format_instant};

pub(super) fn print_catalog(
    out: &mut impl std::io::Write,
    response: &serde_json::Value,
) -> std::io::Result<()> {
    let resolved = response
        .get("resolved_at_ms")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let empty = serde_json::Map::new();
    let models = response
        .get("models")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);

    writeln!(
        out,
        "{} at {}  (catalog version {})",
        count(models.len(), "model"),
        format_instant(resolved),
        response
            .get("catalog_version")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1)
    )?;

    write!(out, "{}", render_catalog_models(response, models))
}

pub(super) fn render_catalog_models(
    response: &serde_json::Value,
    models: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let mut out = render_billing_plane(response);
    // A single-model read shows its facts. Bulk reads list identities so the
    // output remains readable, but both need the same disclosures.
    if models.len() == 1 {
        out.push_str(&render_single_model(models));
    } else {
        out.push('\n');
        for (identity, facts) in models {
            let _ = write!(out, "  {identity}");
            for key in ["quality.elo", "quality.eq"] {
                if let Some(value) = facts.get(key) {
                    let _ = write!(out, "  {key}={}", render_fact_value(value));
                }
            }
            out.push('\n');
        }
    }
    out.push_str(&render_disclosures(response));
    out
}

fn render_disclosures(response: &serde_json::Value) -> String {
    let mut out = render_login_methods(response);
    out.push_str(&render_retired(response));
    out.push_str(&render_quality_scores(response));
    out.push_str(&render_aliased(response));
    out.push_str(&render_fact_overrides(response));
    render_catalog_provenance(&mut out, response);
    out.push_str(&render_withheld(response));
    out.push_str(&render_uncertain(response));
    out
}

fn render_catalog_provenance(out: &mut String, response: &serde_json::Value) {
    let rows = |key: &str| response[key].as_array().into_iter().flatten();
    let value = |v: &serde_json::Value| {
        if v.is_null() {
            "absent".into()
        } else {
            render_fact_value(v)
        }
    };
    for row in rows("overrides") {
        let _ = writeln!(
            out,
            "\nlocal override: {} {}: {} -> {} (set {}); {}",
            text(&row["key"]),
            text(&row["fact"]),
            value(&row["published"]),
            value(&row["local"]),
            format_instant(row["set_at_ms"].as_i64().unwrap_or(0)),
            text(&row["reason"])
        );
    }
    for row in rows("wire_corrections") {
        let source = &row["source_ref"];
        let action = if row["withheld"] == true {
            "withheld"
        } else {
            "corrected"
        };
        let _ = write!(
            out,
            "\nwire correction: {}/{} {} {action}; source: {}@{}:{}",
            text(&row["provider"]),
            text(&row["model"]),
            text(&row["field"]),
            text(&source["repo"]),
            text(&source["commit"]),
            text(&source["file"])
        );
        if let Some(notes) = row["notes"].as_str() {
            let _ = write!(out, "; {notes}");
        }
        out.push('\n');
    }
    for row in rows("canonical_links") {
        let _ = writeln!(
            out,
            "\ncanonical link: {} -> {} (curated); sources: {}, {}; review by {}",
            text(&row["model"]),
            text(&row["canonical_id"]),
            text(&row["source_ref"]),
            text(&row["lab_source_ref"]),
            format_instant(row["review_by_ms"].as_i64().unwrap_or(0))
        );
    }
    for row in rows("speed_twins") {
        let _ = writeln!(
            out,
            "\nspeed twin: {} -> {} ({}; {}); source: {}; review by {}",
            text(&row["model"]),
            text(&row["twin"]),
            text(&row["mode"]),
            text(&row["label"]),
            text(&row["source_ref"]),
            format_instant(row["review_by_ms"].as_i64().unwrap_or(0))
        );
    }
}

fn text(value: &serde_json::Value) -> &str {
    value.as_str().unwrap_or("?")
}

pub(super) fn render_quality_scores(response: &serde_json::Value) -> String {
    let Some(grades) = response
        .get("quality_scores")
        .filter(|value| value.is_object())
    else {
        return String::new();
    };
    format!(
        "\nquality scores: graded by {} at {} (routing judgments, not upstream facts)\n",
        grades["graded_by"].as_str().unwrap_or("?"),
        format_instant(grades["graded_at_ms"].as_i64().unwrap_or(0))
    )
}

/// An alias serves another model's facts under its own id, and its prices are
/// borrowed. Without this line an operator sees ordinary-looking facts and no
/// sign of which model they really describe or what established that.
fn render_aliased(response: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(rows) = response.get("aliased").and_then(|v| v.as_array()) {
        for row in rows {
            let _ = writeln!(
                out,
                "\naliased: {} serves {}/{} (prices borrowed)",
                row["model"].as_str().unwrap_or("?"),
                row["target_provider_id"].as_str().unwrap_or("?"),
                row["target_model_id"].as_str().unwrap_or("?")
            );
            let _ = writeln!(
                out,
                "  source: {}",
                row["source_ref"].as_str().unwrap_or("?")
            );
        }
    }
    out
}

/// Login restrictions must be visible even on a plain read: the provider id
/// alone does not establish that an API-key credential can call this model.
fn render_login_methods(response: &serde_json::Value) -> String {
    let mut out = String::new();
    for key in ["curated", "login_methods"] {
        let Some(rows) = response.get(key).and_then(|v| v.as_array()) else {
            continue;
        };
        for row in rows {
            let methods = row["auth_methods"]
                .as_array()
                .map(|methods| {
                    methods
                        .iter()
                        .filter_map(|method| method.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let model = row["model"].as_str().unwrap_or("?");
            if key == "curated" {
                let _ = writeln!(out, "\ncurated: {model} (not stored upstream)");
            } else {
                let _ = writeln!(out, "\nupstream model: {model}");
            }
            let _ = writeln!(
                out,
                "  allowed login methods: {methods} (all others refused)"
            );
            let _ = writeln!(
                out,
                "  source: {}",
                row["source_ref"].as_str().unwrap_or("?")
            );
            let _ = writeln!(out, "  review by: {}", row["review_by_ms"]);
        }
    }
    out
}

fn render_retired(response: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(rows) = response.get("retired").and_then(|v| v.as_array()) {
        for row in rows {
            let _ = write!(
                out,
                "\nretired: {} (observed {})",
                row["model"].as_str().unwrap_or("?"),
                format_instant(row["retired_at_ms"].as_i64().unwrap_or(0)),
            );
            if let Some(successor) = row["successor"].as_str() {
                let _ = write!(out, "; successor: {successor}");
            }
            out.push('\n');
        }
    }
    out
}

/// Disclose the billing claims beside the catalog they qualify, even when a
/// bulk read only lists model identities. A quota proxy is not a billed price,
/// and a rule's source is what lets an operator inspect that distinction.
pub(super) fn render_billing_plane(response: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(plane) = response.get("plane").and_then(|v| v.as_object()) {
        let _ = writeln!(
            out,
            "\nbilling plane: {}/{} ({})",
            plane
                .get("provider_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?"),
            plane
                .get("auth_method")
                .and_then(|v| v.as_str())
                .unwrap_or("?"),
            plane.get("kind").and_then(|v| v.as_str()).unwrap_or("?"),
        );
        if plane.get("kind").and_then(|v| v.as_str()) == Some("quota_proxy") {
            out.push_str("  API rates are quota proxies, not amounts billed.\n");
        }
        // An undeclared API-key plane is the upstream itself and has no
        // curated provenance. Do not invent a source or a review date for it.
        if let Some(source) = plane.get("source_ref").and_then(|v| v.as_str()) {
            let _ = writeln!(out, "  source {source}");
        }
        if let Some(review) = plane.get("review_by_ms").and_then(|v| v.as_i64()) {
            let _ = writeln!(out, "  review by {}", format_instant(review));
        }
    }

    if let Some(rules) = response.get("billing_rules").and_then(|v| v.as_array()) {
        if !rules.is_empty() {
            out.push_str("\nbilling rules:\n");
        }
        for rule in rules {
            let fact = rule["fact_key"].as_str().unwrap_or("?");
            let suffix = fact
                .strip_prefix("rate.")
                .and_then(|key| key.split_once('.'))
                .map(|(_, suffix)| format!(".{suffix}"))
                .unwrap_or_default();
            let target = rule["as_class"]
                .as_str()
                .map(|class| format!("{class}{suffix}"));
            let state = match rule["state"].as_str().unwrap_or("?") {
                "billed_as" => format!("billed as {}", target.as_deref().unwrap_or("?")),
                "stated_zero" => "stated zero".to_string(),
                "not_established" => match target {
                    Some(target) => format!("not established (would be billed as {target})"),
                    None => "not established".to_string(),
                },
                other => other.to_string(),
            };
            let _ = writeln!(
                out,
                "  {}  {fact}  {state}\n      established by {} on {}, review by {}\n      source {}",
                rule["model"].as_str().unwrap_or("?"),
                rule["established_by"].as_str().unwrap_or("?"),
                format_instant(rule["established_at_ms"].as_i64().unwrap_or(0)),
                format_instant(rule["review_by_ms"].as_i64().unwrap_or(0)),
                rule["source_ref"].as_str().unwrap_or("?"),
            );
        }
    }
    out
}

fn render_fact_value(value: &serde_json::Value) -> String {
    if value.get("state").and_then(|v| v.as_str()) == Some("billed_as") {
        if let Some(key) = value.get("key").and_then(|v| v.as_str()) {
            // Keep any `.above_context.<size>` suffix: a tiered rule bills at
            // the target's price for the same context size, and dropping the
            // suffix would point at the target's cheaper base rate instead.
            return format!("billed as {}", key.strip_prefix("rate.").unwrap_or(key));
        }
    }
    value.to_string()
}

/// Report facts whose served value is a sourced override, not the upstream's.
///
/// # Why a silent override is the failure this exists to prevent
///
/// The value IS in the model list above, and it is the intended one — which is
/// exactly the problem. An operator reading `limit.output  65536` has no way to
/// know models.dev publishes a different number there and fusiform replaced
/// it. If the override is ever wrong, the error is silent: nothing fails and
/// nobody looks. So each override names the value it replaces and its source,
/// on every read that served one, single-model and bulk alike.
///
/// Returned rather than printed so the composed text an operator reads can be
/// asserted, and the caller prints the assembled report once.
fn render_fact_overrides(response: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(rows) = response.get("fact_overrides").and_then(|v| v.as_array()) {
        for row in rows {
            let model = row["model"].as_str().unwrap_or("?");
            let declared = format!(
                "{}/{}",
                row["provider_id"].as_str().unwrap_or("?"),
                row["model_id"].as_str().unwrap_or("?")
            );
            // A null `replaces` marks an addition: upstream publishes nothing for
            // this key, which "publishes null" would misstate.
            let upstream = if row["replaces"].is_null() {
                "upstream publishes no value".to_owned()
            } else {
                format!("upstream publishes {}", row["replaces"])
            };
            let _ = writeln!(
                out,
                "\noverridden: {model} {} = {} ({upstream}; not stored upstream)",
                row["fact_key"].as_str().unwrap_or("?"),
                row["value"],
            );
            // On an alias entry the override is declared for the target row.
            if declared != model {
                let _ = writeln!(out, "  declared for: {declared}");
            }
            let _ = writeln!(out, "  reason: {}", row["reason"].as_str().unwrap_or("?"));
            let _ = writeln!(
                out,
                "  source: {}",
                row["source_ref"].as_str().unwrap_or("?")
            );
            let _ = writeln!(
                out,
                "  established by {} on {}, review by {}",
                row["established_by"].as_str().unwrap_or("?"),
                format_instant(row["established_at_ms"].as_i64().unwrap_or(0)),
                format_instant(row["review_by_ms"].as_i64().unwrap_or(0)),
            );
        }
    }
    out
}

fn render_single_model(models: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut out = String::new();
    for (identity, facts) in models {
        out.push_str(&format!("\n{identity}\n"));
        let facts: BTreeMap<_, _> = facts
            .as_object()
            .map(|o| o.iter().collect())
            .unwrap_or_default();
        for (key, value) in facts {
            let value = render_fact_value(value);
            out.push_str(&format!("  {key:<34}  {value}\n"));
        }
    }
    out
}

/// Report facts the read refused to answer.
///
/// Printed after the models rather than folded into them, because a withheld
/// fact is absent from that list. Without this an operator sees a model with no
/// input rate and concludes the upstream publishes none.
fn render_withheld(response: &serde_json::Value) -> String {
    let mut out = String::new();
    let Some(withheld) = response.get("withheld").and_then(|v| v.as_array()) else {
        return String::new();
    };
    if withheld.is_empty() {
        return String::new();
    }

    let _ = writeln!(
        out,
        "\n{} withheld — the record fusiform holds for these is known bad:",
        count(withheld.len(), "fact")
    );
    for item in withheld {
        let model = item.get("model").and_then(|v| v.as_str()).unwrap_or("?");
        let fact = item.get("fact_key").and_then(|v| v.as_str()).unwrap_or("?");
        let _ = writeln!(out, "  {model}  {fact}");
        if let Some(corrections) = item.get("corrections").and_then(|v| v.as_array()) {
            for c in corrections {
                let from = c
                    .get("affected_from_ms")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let until = c
                    .get("affected_until_ms")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let reason = c.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                let _ = writeln!(
                    out,
                    "      {} to {}: {reason}",
                    format_instant(from),
                    format_instant(until)
                );
            }
        }
    }
    out
}

/// Facts whose value at the read instant may already have been superseded.
///
/// Printed after the models rather than folded into them, because unlike a
/// withheld fact the value IS returned — this qualifies an answer rather than
/// replacing one. An operator who sees the value alone reads it as what was in
/// force; what fusiform actually knows is that it was in force at the start of
/// an interval containing the read instant, and it did not look again until the
/// end.
///
/// Empty for a read of the current catalog, always: uncertainty needs a later
/// recorded value to bound the interval, and the value in force now has none
/// yet. So this only appears on a historical read, which is the question a
/// wrong answer costs money on.
fn render_uncertain(response: &serde_json::Value) -> String {
    let mut out = String::new();
    let Some(uncertain) = response.get("uncertain").and_then(|v| v.as_array()) else {
        return String::new();
    };
    if uncertain.is_empty() {
        return String::new();
    }

    let _ = writeln!(
        out,
        "\n{} uncertain at this instant — the value is real, and \
         fusiform did not look during the window it may have changed in:",
        count(uncertain.len(), "fact")
    );
    for item in uncertain {
        let provider = item
            .get("provider_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let model = item.get("model_id").and_then(|v| v.as_str()).unwrap_or("?");
        let fact = item.get("fact_key").and_then(|v| v.as_str()).unwrap_or("?");
        let after = item
            .get("superseded_after_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let by = item
            .get("superseded_by_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let _ = writeln!(out, "  {provider}/{model}  {fact}");
        let _ = writeln!(
            out,
            "      last confirmed {}, changed by {}",
            format_instant(after),
            format_instant(by)
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn get_text_discloses_replaced_withheld_and_linked_facts() {
        let reply = json!({
            "source":"models.dev", "catalog_version":3, "resolved_at_ms":2000,
            "models":{"p/m":{"limit.context":2}},
            "overrides":[{"key":"p/m","fact":"limit.context","published":1,"local":2,
                          "reason":"local judgment","set_at_ms":1000}],
            "wire_corrections":[
                {"provider":"p","model":"m","field":"wire.api","withheld":true,
                 "source_ref":{"repo":"r","commit":"c","file":"f"},"notes":"not routed",
                 "established_by":"e","established_at_ms":1000,"review_by_ms":2000},
                {"provider":"p","model":"m","field":"wire.model","withheld":false,
                 "source_ref":{"repo":"r","commit":"c","file":"f"},
                 "established_by":"e","established_at_ms":1000,"review_by_ms":2000}],
            "canonical_links":[{"model":"p/m","canonical_id":"lab/m","source_ref":"provider-docs",
                "quote":"q","lab_source_ref":"lab-docs","lab_quote":"lq","review_by_ms":2000}],
            "speed_twins":[{"model":"p/m","twin":"p/m-fast","mode":"fast","label":"Highspeed",
                "source_ref":"speed-docs","review_by_ms":2000}]
        });
        // A new response field must be classified here before it can ship.
        let fusiform_protocol::CatalogGetResponse {
            source: _,
            next_page_token: _,
            resolved_at_ms: _,
            catalog_version: _,
            models: _,
            curated: _,
            login_methods: _,
            quality_scores: _,
            overrides,
            withheld: _,
            retired: _,
            uncertain: _,
            aliased: _,
            canonical_links,
            speed_twins,
            wire_corrections,
            fact_overrides: _,
            plane: _,
            billing_rules: _,
        } = serde_json::from_value(reply.clone()).unwrap();
        assert_eq!(overrides.len(), 1);
        assert_eq!(wire_corrections.len(), 2);
        assert_eq!(canonical_links.len(), 1);
        assert_eq!(speed_twins.len(), 1);
        let text = render_catalog_models(&reply, reply["models"].as_object().unwrap());
        for part in [
            "local override: p/m limit.context: 1 -> 2",
            "local judgment",
            "1970-01-01 00:00:01Z",
            "wire.api withheld",
            "wire.model corrected",
            "not routed",
            "r@c:f",
            "canonical link: p/m -> lab/m",
            "provider-docs",
            "lab-docs",
            "speed twin: p/m -> p/m-fast",
            "Highspeed",
            "speed-docs",
        ] {
            assert!(text.contains(part), "missing {part:?}: {text}");
        }
    }

    #[test]
    fn login_allowlists_and_retirements_are_disclosed_for_every_model_count() {
        for models in [json!({"p/m":{}}), json!({"p/m":{},"p/other":{}}), json!({})] {
            let mut reply = json!({
                "models": models, "source":"models.dev", "catalog_version":3, "resolved_at_ms":2000,
                "retired":[{"model":"p/old","retired_at_ms":1000,"successor":"p/new"},
                           {"model":"p/older","retired_at_ms":2000}]
            });
            if !models.as_object().unwrap().is_empty() {
                reply["login_methods"] = json!([{
                    "model":"p/m","auth_methods":["oauth"],"source_ref":"provider-docs","review_by_ms":3000
                }]);
            }
            serde_json::from_value::<fusiform_protocol::CatalogGetResponse>(reply.clone()).unwrap();
            let text = render_catalog_models(&reply, reply["models"].as_object().unwrap());
            if !models.as_object().unwrap().is_empty() {
                for part in [
                    "upstream model: p/m",
                    "allowed login methods: oauth",
                    "provider-docs",
                    "review by:",
                ] {
                    assert!(text.contains(part), "missing {part:?}: {text}");
                }
            }
            assert!(
                text.contains(
                    "retired: p/old (observed 1970-01-01 00:00:01Z [1000 ms]); successor: p/new"
                ),
                "{text}"
            );
            assert!(
                text.lines().any(
                    |line| line == "retired: p/older (observed 1970-01-01 00:00:02Z [2000 ms])"
                ),
                "{text}"
            );
        }
    }
}
