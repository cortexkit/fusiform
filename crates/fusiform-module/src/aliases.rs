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
//! same shape as open-weight rate inheritance in `route.rs`. A real row for the
//! alias id always wins over the alias.

use std::collections::BTreeMap;

const ALIASES: &str = include_str!("../data/model-aliases.json");

/// One row as it appears in the file.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AliasRow {
    provider_id: String,
    model_id: String,
    target: TargetRow,
    basis: String,
    source_ref: String,
    established_at_ms: i64,
    review_by_ms: i64,
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
    /// Why this id is that model, in a sentence.
    pub basis: String,
    /// The file and pinned revision the claim was read from.
    pub source_ref: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

/// `(alias provider_id, alias model_id)` -> what it resolves to.
///
/// Ordered, so a bulk read adds alias entries in the same order every time.
pub type Aliases = BTreeMap<(String, String), Alias>;

/// Parse an alias document, reporting rows that did not load.
///
/// Rejects are COUNTED rather than skipped silently, as the creator and
/// overlay loaders do: a loader that quietly returns fewer rows turns a schema
/// mistake into a model that mysteriously answers `no_coverage`.
pub fn load_reporting_rejects(doc: &str) -> (Aliases, Vec<String>) {
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
    for raw in doc.aliases {
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
        let alias = Alias {
            target_provider_id: row.target.provider_id,
            target_model_id: row.target.model_id,
            basis: row.basis,
            source_ref: row.source_ref,
            established_at_ms: row.established_at_ms,
            review_by_ms: row.review_by_ms,
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

/// The shipped table, parsed once per process.
///
/// Embedded, so a re-parse on every catalog read would cost the same answer
/// repeatedly.
pub fn aliases() -> &'static Aliases {
    static PARSED: std::sync::OnceLock<Aliases> = std::sync::OnceLock::new();
    PARSED.get_or_init(|| load_reporting_rejects(ALIASES).0)
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

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = include_str!("../data/models-dev-seed.json");

    /// Ids read and deliberately NOT aliased, with the reason.
    ///
    /// Kept beside the loader so the next person reading the routing code
    /// sees what was already ruled out and why, and so a row for one of these
    /// cannot be added without deleting its reason first.
    const DECLARED_EXCLUSIONS: &[(&str, &str, &str)] = &[
        (
            "google",
            "antigravity-gemini-3.5-flash",
            "GEMINI_35_FLASH_ROUTES sends the high tier (and the default model) to \
             gemini-3-flash-agent, a different model from gemini-3.5-flash",
        ),
        (
            "google",
            "antigravity-gemini-3.1-pro",
            "models.dev publishes no google/gemini-3.1-pro row to alias to, and the \
             high tier routes to gemini-pro-agent",
        ),
        (
            "google",
            "antigravity-gpt-oss-120b-medium",
            "models.dev publishes no openai/gpt-oss-120b row, and the id fixes \
             effort at medium, so the target's reasoning options would be false \
             for it",
        ),
        (
            "google",
            "antigravity-claude-opus-4-6-thinking",
            "the resolver has no RESOLVER_ALIASES entry for claude-opus-4-6-thinking, \
             so every tier reaches the claude-opus-4-6-thinking backend rather than \
             claude-opus-4-6; the routing code does not establish they are one model",
        ),
    ];

    fn seed_has(provider_id: &str, model_id: &str) -> bool {
        let seed: serde_json::Value = serde_json::from_str(SEED).expect("the seed parses");
        seed.get(provider_id)
            .and_then(|p| p.get("models"))
            .and_then(|m| m.get(model_id))
            .is_some()
    }

    #[test]
    fn the_shipped_table_loads_without_rejects() {
        let (aliases, rejects) = load_reporting_rejects(ALIASES);
        assert!(
            rejects.is_empty(),
            "the shipped alias table must parse cleanly: {rejects:?}"
        );
        assert!(
            !aliases.is_empty(),
            "an empty table would make every assertion below vacuous"
        );
        assert_eq!(
            aliases
                .get(&(
                    "google".to_string(),
                    "antigravity-gemini-3.8-flash".to_string()
                ))
                .map(|a| (a.target_provider_id.as_str(), a.target_model_id.as_str())),
            Some(("google", "gemini-3.8-flash")),
            "the alias the table exists for must resolve"
        );
    }

    /// Every target is a row the embedded seed publishes.
    ///
    /// An alias to a row that does not exist serves nothing, so a typo in a
    /// target would look exactly like a model the catalog does not cover.
    #[test]
    fn every_alias_target_exists_in_the_embedded_seed() {
        let aliases = aliases();
        let missing: Vec<String> = aliases
            .values()
            .filter(|a| !seed_has(&a.target_provider_id, &a.target_model_id))
            .map(|a| format!("{}/{}", a.target_provider_id, a.target_model_id))
            .collect();
        assert!(
            missing.is_empty(),
            "alias targets absent from the embedded models.dev seed: {missing:?}"
        );
    }

    /// No alias id is a row the embedded seed publishes.
    ///
    /// The serve path lets a real row win, so an alias shadowing a published
    /// id would be dead at runtime and would still read, in the file, as a
    /// claim about identity. Either the alias is wrong or it is redundant, and
    /// both are curation errors.
    #[test]
    fn no_alias_id_exists_in_the_embedded_seed() {
        let shadowing: Vec<String> = aliases()
            .keys()
            .filter(|(p, m)| seed_has(p, m))
            .map(|(p, m)| format!("{p}/{m}"))
            .collect();
        assert!(
            shadowing.is_empty(),
            "alias ids that models.dev publishes as real rows: {shadowing:?}"
        );
    }

    /// The fence above can fail: a seed row is found when it exists.
    #[test]
    fn the_seed_lookup_finds_a_row_that_exists() {
        assert!(seed_has("google", "gemini-3.8-flash"));
        assert!(!seed_has("google", "antigravity-gemini-3.8-flash"));
    }

    #[test]
    fn no_declared_exclusion_is_in_the_table() {
        for (p, m, why) in DECLARED_EXCLUSIONS {
            assert!(
                !aliases().contains_key(&(p.to_string(), m.to_string())),
                "{p}/{m} was ruled out ({why}); remove it from the exclusions \
                 with a reason before adding it to the table"
            );
        }
    }

    #[test]
    fn a_duplicate_alias_id_is_reported_rather_than_resolved() {
        let row = |target: &str| {
            format!(
                r#"{{"provider_id":"google","model_id":"x","target":{{"provider_id":"google","model_id":"{target}"}},
                "basis":"b","source_ref":"s","established_at_ms":1,"review_by_ms":2}}"#
            )
        };
        let doc = format!(r#"{{"aliases":[{},{}]}}"#, row("a"), row("b"));
        let (aliases, rejects) = load_reporting_rejects(&doc);
        assert_eq!(
            rejects.len(),
            1,
            "the duplicate must be reported: {rejects:?}"
        );
        assert!(rejects[0].contains("appears twice"), "{rejects:?}");
        assert_eq!(aliases.len(), 1);
    }

    #[test]
    fn a_malformed_row_is_counted_not_fatal() {
        let doc = r#"{"aliases":[
            {"provider_id":"google","model_id":"x","target":{"provider_id":"google","model_id":"y"},
             "basis":"b","source_ref":"s","established_at_ms":1,"review_by_ms":2},
            {"provider_id":"google","model_id":"z"}
        ]}"#;
        let (aliases, rejects) = load_reporting_rejects(doc);
        assert_eq!(aliases.len(), 1, "the good row still loads");
        assert_eq!(rejects.len(), 1, "the bad row is reported: {rejects:?}");
    }

    #[test]
    fn a_chained_alias_is_refused() {
        let doc = r#"{"aliases":[
            {"provider_id":"google","model_id":"a","target":{"provider_id":"google","model_id":"b"},
             "basis":"x","source_ref":"s","established_at_ms":1,"review_by_ms":2},
            {"provider_id":"google","model_id":"b","target":{"provider_id":"google","model_id":"c"},
             "basis":"x","source_ref":"s","established_at_ms":1,"review_by_ms":2}
        ]}"#;
        let (aliases, rejects) = load_reporting_rejects(doc);
        assert!(!aliases.contains_key(&("google".to_string(), "a".to_string())));
        assert_eq!(rejects.len(), 1, "{rejects:?}");
    }

    /// The review gate on the shipped table.
    ///
    /// Time-triggered: it fails on a day nobody changed anything. The message
    /// says so, and says what to do.
    #[test]
    fn no_alias_is_past_its_review_date() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_millis() as i64;
        let overdue = overdue_at(aliases(), now);
        assert!(overdue.is_empty(), "\n\n{}", overdue_report(&overdue));
    }

    /// The gate fires, and its message names the row and what to do.
    #[test]
    fn an_overdue_alias_fires_and_the_message_is_actionable() {
        let doc = r#"{"aliases":[{"provider_id":"google","model_id":"x",
            "target":{"provider_id":"google","model_id":"y"},"basis":"b",
            "source_ref":"repo@abc file.ts","established_at_ms":1,"review_by_ms":1000}]}"#;
        let (aliases, rejects) = load_reporting_rejects(doc);
        assert!(rejects.is_empty(), "{rejects:?}");

        let overdue = overdue_at(&aliases, 1000 + 86_400_000 * 3);
        assert_eq!(overdue.len(), 1);
        assert_eq!(overdue[0].days_overdue, 3);
        let msg = overdue_report(&overdue);
        assert!(msg.contains("NOT A BUILD FAILURE"), "{msg}");
        assert!(msg.contains("google/x -> google/y"), "{msg}");
        assert!(msg.contains("3 day(s)"), "{msg}");
        assert!(msg.contains("repo@abc file.ts"), "{msg}");
        assert!(msg.contains("CANNOT tell you"), "{msg}");

        // CONTROL: a row inside its window does not fire.
        assert!(overdue_at(&aliases, 999).is_empty());
    }
}
