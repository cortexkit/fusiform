//! Curated model aliases: an id a route exposes, and the published row it is.
//!
//! # Why this exists
//!
//! A plugin can expose a model under its own id. The Google Code Assist plugin
//! serves `google/gemini-3.8-flash` as `google/antigravity-gemini-3.8-flash`,
//! and models.dev has no row for that id, so `catalog.get` answered
//! `no_coverage` and a consumer could not learn the model's reasoning options,
//! limits or prices.
//!
//! # Why a table and not inference
//!
//! Names that look alike are not evidence of identity: the same plugin routes
//! the high tier of `antigravity-gemini-3.5-flash` to `gemini-3-flash-agent`, a
//! different model. So every row here is authored by someone who read the
//! routing code, names its target exactly, and cites the file and revision.
//! Guessing identity from names is exactly what this table refuses to do.
//!
//! # How it is served
//!
//! At read time, on current reads only, and never written to the store — the
//! same shape as open-weight rate inheritance. A real row for the
//! alias id always wins over the alias.

use std::collections::BTreeMap;

/// One row as it appears in the file.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AliasRow {
    provider_id: String,
    model_id: String,
    model_name: Option<String>,
    wire_family: Option<String>,
    target: TargetRow,
    basis: String,
    source_ref: String,
    established_at_ms: i64,
    review_by_ms: i64,
    #[serde(default)]
    reasoning_wire_ids: BTreeMap<String, String>,
    reasoning_default: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetRow {
    provider_id: String,
    model_id: String,
}

/// What an alias id resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    pub target_provider_id: String,
    pub target_model_id: String,
    /// The route's own display label from the pinned plugin source, if stated.
    pub model_name: Option<String>,
    /// The alias route's protocol, verified from its own plugin source.
    pub wire_family: Option<String>,
    /// Why this id is that model, in a sentence.
    pub basis: String,
    /// The file and pinned revision the claim was read from.
    pub source_ref: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
    pub reasoning_wire_ids: BTreeMap<String, String>,
    pub reasoning_default: Option<String>,
}

/// `(alias provider_id, alias model_id)` -> what it resolves to.
///
/// Ordered, so a bulk read adds alias entries in the same order every time.
pub type Aliases = BTreeMap<(String, String), Alias>;

/// Parse an alias document, reporting rows that did not load.
///
/// Rejects are counted rather than skipped silently, as the creator loader
/// does: a loader that quietly returns fewer rows turns a schema
/// mistake into a model that mysteriously answers `no_coverage`.
pub fn load_reporting_rejects(doc: &str) -> (Aliases, Vec<String>) {
    load(doc, false)
}

/// Load aliases from a stored release, ignoring the retired `limit_output` field.
pub fn load_stored_reporting_rejects(doc: &str) -> (Aliases, Vec<String>) {
    load(doc, true)
}

fn load(doc: &str, stored: bool) -> (Aliases, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Doc {
        aliases: Vec<serde_json::Value>,
    }
    let doc: Doc = match serde_json::from_str(doc) {
        Ok(d) => d,
        Err(e) => return (Aliases::new(), vec![format!("alias table unreadable: {e}")]),
    };

    let mut out = Aliases::new();
    let mut rejects = Vec::new();
    for mut raw in doc.aliases {
        if let Some(row) = raw.as_object_mut().filter(|_| stored) {
            // Older signed releases carry `limit_output`. Replay must accept
            // it without overriding the target model's output limit.
            row.remove("limit_output");
        }
        let row: AliasRow = match serde_json::from_value(raw.clone()) {
            Ok(r) => r,
            Err(e) => {
                rejects.push(format!("alias row did not parse ({e}): {raw}"));
                continue;
            }
        };
        let fields = [
            &row.provider_id,
            &row.model_id,
            &row.target.provider_id,
            &row.target.model_id,
            &row.basis,
            &row.source_ref,
        ];
        if fields.iter().any(|f| f.trim().is_empty()) {
            rejects.push(format!(
                "alias row with an empty field: {}/{}",
                row.provider_id, row.model_id
            ));
            continue;
        }
        if row.provider_id == row.target.provider_id && row.model_id == row.target.model_id {
            rejects.push(format!(
                "alias {}/{} names itself as its target",
                row.provider_id, row.model_id
            ));
            continue;
        }
        let key = (row.provider_id.clone(), row.model_id.clone());
        if row
            .wire_family
            .as_deref()
            .is_some_and(|family| !crate::wire_families::is_known(family))
        {
            rejects.push(format!("alias {}/{} has unknown wire_family", key.0, key.1));
            continue;
        }
        if row.reasoning_wire_ids.is_empty() != row.reasoning_default.is_none()
            || row
                .reasoning_default
                .as_ref()
                .is_some_and(|default| !row.reasoning_wire_ids.contains_key(default))
            || row
                .reasoning_wire_ids
                .iter()
                .any(|(effort, wire)| effort.trim().is_empty() || wire.trim().is_empty())
        {
            rejects.push(format!(
                "alias {}/{} has invalid reasoning tiers or default",
                key.0, key.1
            ));
            continue;
        }
        let alias = Alias {
            target_provider_id: row.target.provider_id,
            target_model_id: row.target.model_id,
            model_name: row.model_name.filter(|name| !name.is_empty()),
            wire_family: row.wire_family,
            basis: row.basis,
            source_ref: row.source_ref,
            established_at_ms: row.established_at_ms,
            review_by_ms: row.review_by_ms,
            reasoning_wire_ids: row.reasoning_wire_ids,
            reasoning_default: row.reasoning_default,
        };
        if let Some(existing) = out.get(&key) {
            // Two rows for one alias id is a curation mistake, not a
            // preference order. Keeping either would make the served identity
            // depend on file ordering, so the id keeps its FIRST row and the
            // duplicate is reported for someone to resolve.
            rejects.push(format!(
                "alias {}/{} appears twice: -> {}/{} and -> {}/{}",
                key.0,
                key.1,
                existing.target_provider_id,
                existing.target_model_id,
                alias.target_provider_id,
                alias.target_model_id
            ));
            continue;
        }
        out.insert(key, alias);
    }

    // A target that is itself an alias would need a second hop, and the serve
    // path reads the target from the store only, so the entry would silently
    // serve nothing. Refused here so the mistake has a message.
    let chained: Vec<(String, String)> = out
        .iter()
        .filter(|(_, a)| {
            out.contains_key(&(a.target_provider_id.clone(), a.target_model_id.clone()))
        })
        .map(|(k, _)| k.clone())
        .collect();
    for key in chained {
        rejects.push(format!(
            "alias {}/{} targets another alias; aliases resolve one hop to a real row",
            key.0, key.1
        ));
        out.remove(&key);
    }

    (out, rejects)
}

/// An alias id fusiform used to serve and no longer does.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetiredAlias {
    pub provider_id: String,
    pub model_id: String,
    /// When fusiform stopped serving the alias.
    pub retired_at_ms: i64,
    /// Why it was retired, for a reader of the file.
    pub reason: String,
    pub successor: Option<String>,
    pub successor_source_ref: Option<String>,
}

/// Each reasoning tier exposed by an alias must also be supported by its target model.
pub fn reasoning_option_drift(rows: &Aliases, seed: &serde_json::Value) -> Vec<String> {
    rows.iter().filter_map(|((provider, model), alias)| {
        let reasoning = &seed[&alias.target_provider_id]["models"][&alias.target_model_id]["capabilities"]["reasoning"];
        let missing: Vec<_> = alias.reasoning_wire_ids.keys().filter(|effort| {
            reasoning["supported"].as_bool() != Some(true)
                || !reasoning["effort"].as_array().is_some_and(|values| values.iter().any(|value| value.as_str() == Some(effort.as_str())))
        }).cloned().collect();
        (!missing.is_empty()).then(|| format!("{provider}/{model}: reasoning wire efforts absent from capability.reasoning_options: {missing:?}"))
    }).collect()
}

/// A replacement must be sourced and resolve to a currently served identity.
pub fn invalid_successors(
    retired: &BTreeMap<(String, String), RetiredAlias>,
    served: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    retired
        .values()
        .filter(|row| match (&row.successor, &row.successor_source_ref) {
            (None, None) => false,
            (Some(id), Some(source)) => !served.contains(id) || source.trim().is_empty(),
            _ => true,
        })
        .map(|row| {
            format!(
                "{}/{}: successor must name a currently served row and cite its source",
                row.provider_id, row.model_id
            )
        })
        .collect()
}

/// Alias ids that were served and then retired, keyed like the live table.
///
/// An alias is computed at read time and never stored, so deleting its row
/// would make the id vanish from every read. A consumer that tracks identities
/// across reads, and asks for retired rows, reads a vanished identity as a
/// truncated response and refuses the refresh. Listing retired ids here lets
/// the read report them the way a withdrawn upstream model is reported:
/// present as an identity, marked absent.
pub fn load_retired(doc: &str) -> BTreeMap<(String, String), RetiredAlias> {
    #[derive(serde::Deserialize)]
    struct Doc {
        #[serde(default)]
        retired: Vec<RetiredAlias>,
    }
    serde_json::from_str::<Doc>(doc)
        .map(|d| {
            d.retired
                .into_iter()
                .map(|r| ((r.provider_id.clone(), r.model_id.clone()), r))
                .collect()
        })
        .unwrap_or_default()
}

/// One alias row past its review date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overdue {
    pub alias: String,
    pub target: String,
    pub days_overdue: i64,
    pub source_ref: String,
}

/// Which rows are past their review date at `now_ms`.
///
/// Takes the instant rather than reading the clock, so the gate can be driven
/// in both directions.
pub fn overdue_at(aliases: &Aliases, now_ms: i64) -> Vec<Overdue> {
    aliases
        .iter()
        .filter(|(_, a)| a.review_by_ms < now_ms)
        .map(|((p, m), a)| Overdue {
            alias: format!("{p}/{m}"),
            target: format!("{}/{}", a.target_provider_id, a.target_model_id),
            days_overdue: (now_ms - a.review_by_ms) / 86_400_000,
            source_ref: a.source_ref.clone(),
        })
        .collect()
}

/// What an operator should read when the review gate fires.
///
/// Leads with the fact that nothing broke: a time-triggered gate fails on a
/// morning when nobody changed anything, and a reader who concludes "CI is
/// broken" reaches for a skip.
pub fn overdue_report(overdue: &[Overdue]) -> String {
    let mut out = String::from(
        "NOT A BUILD FAILURE. Nothing changed; a review date passed.\n\n\
         These curated model aliases are past their review date. An alias \
         claims a route's id IS another row's model, and the route's code can \
         change what it sends without anything in fusiform noticing, so this \
         date is the only signal that the claim may have gone stale.\n\n",
    );
    for o in overdue {
        out.push_str(&format!(
            "  {} -> {} — {} day(s) overdue\n    read: {}\n",
            o.alias, o.target, o.days_overdue, o.source_ref
        ));
    }
    out.push_str(
        "\nRe-read the routing code at the current revision and commit:\n\
         \x20 - the row removed, IF any tier of the alias no longer reaches the \
         target's own backend model\n\
         \x20 - a new source_ref revision, established_at_ms and review_by_ms \
         (60 days on), ALWAYS\n\n\
         Confirming a route unchanged is a real result and the commit should \
         look like one.\n\n\
         What this gate CANNOT tell you: whether the alias is still true. It \
         knows only that nobody has looked recently.\n",
    );
    out
}
