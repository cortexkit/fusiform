//! Model rows models.dev does not carry, backed by explicit probes and lab
//! records.
//!
//! Provider rows missing from models.dev's API catalog cannot be inferred from
//! lab ids: the provider may require a different model id and login method.
//! These rows carry those restrictions and stand down wherever upstream
//! publishes the model.
//!
//! A current read serves the rows straight from the compiled file. Every boot
//! also records the file in the store ([`apply_to_store`]), so a point-in-time
//! read can serve the claim that was in force at its instant.

use std::collections::BTreeMap;

const DOCUMENT: &str = include_str!("../data/curated-models.json");

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CuratedModel {
    pub provider_id: String,
    pub model_id: String,
    pub auth_methods: Vec<String>,
    pub facts: BTreeMap<String, serde_json::Value>,
    pub source_ref: String,
    /// Who read the sources, when the file records it. Not served.
    #[serde(default)]
    pub established_by: Option<String>,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
}

pub type CuratedModels = BTreeMap<(String, String), CuratedModel>;

/// A row in `models` that fusiform has stopped serving for now.
///
/// The row's data stays in `models` untouched, so serving it again is a
/// one-line removal from `held`. While held, the row is never served as
/// curated. If the store has never recorded the id, a current read reports it
/// the way it reports a retired alias: absent on a retired-inclusive read and
/// listed in `retired` on a named read. If the store has recorded it, the
/// store's own row (present or withdrawn) answers instead. Deleting the row instead would make
/// the id vanish, and a consumer that tracks identities reads a vanished id as
/// a truncated catalog and refuses the refresh.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldModel {
    pub provider_id: String,
    pub model_id: String,
    /// When fusiform put the row on hold; served as `retired_at_ms` when the
    /// store has never recorded the id.
    pub held_since_ms: i64,
    /// Why the row is held and what lets it be served again. Not served.
    pub reason: String,
}

pub type HeldModels = BTreeMap<(String, String), HeldModel>;

/// Everything the curated file carries: its rows, and which of them are held.
#[derive(Debug, Default)]
pub struct Table {
    pub models: CuratedModels,
    pub held: HeldModels,
}

/// Reject unsupported facts rather than silently exposing a new wire vocabulary.
/// A row that admits a subscription login must never claim an API list price,
/// so only an `apikey`-only row may carry `rate.*` facts.
fn invalid_facts(row: &CuratedModel) -> Option<String> {
    for (key, value) in &row.facts {
        if key.starts_with("rate.") && row.auth_methods != ["apikey"] {
            return Some(format!(
                "a row admitting a subscription login must not carry a list price: {key}"
            ));
        }
        if !fusiform_protocol::SERVED_FACTS
            .iter()
            .any(|fact| fact.key == key)
        {
            return Some(format!("fact key is not in SERVED_FACTS: {key}"));
        }
        if key == "model.kind"
            && !value
                .as_str()
                .is_some_and(|kind| fusiform_protocol::MODEL_KINDS.contains(&kind))
        {
            return Some(format!(
                "model.kind must be one of {:?}: {value}",
                fusiform_protocol::MODEL_KINDS
            ));
        }
        if key.starts_with("rate.") {
            if let Some(reason) = invalid_rate(value) {
                return Some(format!("{key}: {reason}"));
            }
        }
    }
    None
}

/// A curated rate is served byte for byte as written, so it must already be in
/// the served shape: a priced amount whose currency the source stated, or a
/// stated zero. Curated rows never borrow a price, never refuse one, and never
/// carry fields the published `RateValue` type would silently drop.
fn invalid_rate(value: &serde_json::Value) -> Option<String> {
    let keys: std::collections::BTreeSet<&str> = value
        .as_object()
        .map(|obj| obj.keys().map(String::as_str).collect())
        .unwrap_or_default();
    match serde_json::from_value::<fusiform_protocol::RateValue>(value.clone()) {
        Ok(fusiform_protocol::RateValue::Priced {
            amount,
            inherited_from: None,
            ..
        }) if amount.units > 0
            && amount.unit_provenance == fusiform_protocol::money::UnitProvenance::Stated
            && keys
                == std::collections::BTreeSet::from([
                    "state",
                    "units",
                    "exponent",
                    "currency",
                    "unit_provenance",
                ]) =>
        {
            None
        }
        Ok(fusiform_protocol::RateValue::StatedZero {
            inherited_from: None,
        }) if keys == std::collections::BTreeSet::from(["state"]) => None,
        _ => Some(format!(
            "must be a priced rate with a stated currency, or a stated zero: {value}"
        )),
    }
}

/// Load the whole file: its rows, then its held entries checked against them.
pub fn load_table_reporting_rejects(doc: &str) -> (Table, Vec<String>) {
    let (models, mut rejects) = load_reporting_rejects(doc);
    let (held, held_rejects) = load_held_reporting_rejects(doc, &models);
    rejects.extend(held_rejects);
    (Table { models, held }, rejects)
}

/// The `held` entries, each naming a row that loaded.
///
/// A held entry naming no row would hold nothing and report an id nobody
/// serves, so it is a reject, as is an id held twice.
pub fn load_held_reporting_rejects(doc: &str, rows: &CuratedModels) -> (HeldModels, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Doc {
        #[serde(default)]
        held: Vec<serde_json::Value>,
    }
    let doc: Doc = match serde_json::from_str(doc) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                HeldModels::new(),
                vec![format!("curated held list unreadable: {error}")],
            )
        }
    };
    let mut held = HeldModels::new();
    let mut rejects = Vec::new();
    for raw in doc.held {
        let entry: HeldModel = match serde_json::from_value(raw) {
            Ok(entry) => entry,
            Err(error) => {
                rejects.push(format!("curated held entry did not parse: {error}"));
                continue;
            }
        };
        let key = (entry.provider_id.clone(), entry.model_id.clone());
        let invalid = if entry.reason.trim().is_empty() || entry.held_since_ms <= 0 {
            Some("a held entry needs a reason and a positive held_since_ms")
        } else if !rows.contains_key(&key) {
            Some("a held entry must name a row in models")
        } else if held.contains_key(&key) {
            Some("duplicate held identity")
        } else {
            None
        };
        if let Some(reason) = invalid {
            rejects.push(format!("curated held {}/{}: {reason}", key.0, key.1));
        } else {
            held.insert(key, entry);
        }
    }
    (held, rejects)
}

/// Malformed rows and duplicate identities are counted, not silently skipped.
pub fn load_reporting_rejects(doc: &str) -> (CuratedModels, Vec<String>) {
    #[derive(serde::Deserialize)]
    struct Doc {
        models: Vec<serde_json::Value>,
    }
    let doc: Doc = match serde_json::from_str(doc) {
        Ok(doc) => doc,
        Err(error) => {
            return (
                BTreeMap::new(),
                vec![format!("curated table unreadable: {error}")],
            )
        }
    };
    let mut rows = CuratedModels::new();
    let mut rejects = Vec::new();
    for raw in doc.models {
        let row: CuratedModel = match serde_json::from_value(raw) {
            Ok(row) => row,
            Err(error) => {
                rejects.push(format!("curated row did not parse: {error}"));
                continue;
            }
        };
        let key = (row.provider_id.clone(), row.model_id.clone());
        let invalid = if [&row.provider_id, &row.model_id, &row.source_ref]
            .iter()
            .any(|field| field.trim().is_empty())
            || row.facts.is_empty()
            || row
                .established_by
                .as_deref()
                .is_some_and(|by| by.trim().is_empty())
        {
            Some("empty identity, source, established_by or facts".to_string())
        } else if row.auth_methods.is_empty()
            || row
                .auth_methods
                .iter()
                .any(|method| !crate::billing_planes::AUTH_METHODS.contains(&method.as_str()))
            || row
                .auth_methods
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != row.auth_methods.len()
        {
            Some(
                "auth_methods must be a nonempty allowlist of distinct known login methods"
                    .to_string(),
            )
        } else if row.established_at_ms <= 0
            || row.review_by_ms.checked_sub(row.established_at_ms) != Some(60 * 86_400_000)
        {
            Some("review_by_ms must be 60 days after established_at_ms".to_string())
        } else if rows.contains_key(&key) {
            Some("duplicate curated identity".to_string())
        } else {
            invalid_facts(&row)
        };
        if let Some(reason) = invalid {
            rejects.push(format!("curated {}/{}: {reason}", key.0, key.1));
        } else {
            rows.insert(key, row);
        }
    }
    (rows, rejects)
}

/// The embedded file, parsed once per process. Current reads serve from it
/// directly; the store holds its history for point-in-time reads.
///
/// A unit test may plant another document for its own thread with [`plant`],
/// so route tests can exercise the rows both as served and as held.
pub fn table() -> &'static Table {
    #[cfg(test)]
    if let Some(planted) = PLANTED.with(std::cell::Cell::get) {
        return planted;
    }
    static PARSED: std::sync::OnceLock<Table> = std::sync::OnceLock::new();
    PARSED.get_or_init(|| load_table_reporting_rejects(DOCUMENT).0)
}

/// Every row in the file, held or not.
pub fn models() -> &'static CuratedModels {
    &table().models
}

/// The rows not served while held.
pub fn held() -> &'static HeldModels {
    &table().held
}

#[cfg(test)]
thread_local! {
    static PLANTED: std::cell::Cell<Option<&'static Table>> = const { std::cell::Cell::new(None) };
}

/// While the returned guard lives, `doc` stands in for the shipped file, on
/// this thread only.
///
/// The planted document must load without rejects, so a test cannot pass on a
/// document that silently lost the rows it means to exercise.
#[cfg(test)]
#[must_use = "the planted document is withdrawn when the guard drops"]
pub(crate) fn plant(doc: &str) -> Planted {
    let (table, rejects) = load_table_reporting_rejects(doc);
    assert!(rejects.is_empty(), "planted curated document: {rejects:?}");
    // Leaked: a planted table must outlive every `&'static` borrow the route
    // takes from it, and a test process is short-lived.
    let table: &'static Table = Box::leak(Box::new(table));
    Planted(PLANTED.with(|planted| planted.replace(Some(table))))
}

/// Restores whatever was planted before, or the shipped file, when dropped.
#[cfg(test)]
pub(crate) struct Planted(Option<&'static Table>);

#[cfg(test)]
impl Drop for Planted {
    fn drop(&mut self) {
        PLANTED.with(|planted| planted.set(self.0));
    }
}

/// The four System One rows, as (provider, model): the rows the held tests
/// hold. They were held in the shipped file from 2026-10-21 until broca's
/// catalog assembly accepted rows missing a context or capability fact.
#[cfg(test)]
pub(crate) const TEST_HELD: &[(&str, &str)] = &[
    ("cloudflare-workers-ai", "@cf/cloudflare/clairic"),
    ("cloudflare-workers-ai", "@cf/cloudflare/clef"),
    ("cloudflare-workers-ai", "@cf/cloudflare/clef-flash"),
    ("typesafe", "jev-1.13.0"),
];

/// The `held_since_ms` the held tests use, 2026-10-21T20:30:00Z.
#[cfg(test)]
pub(crate) const TEST_HELD_SINCE_MS: i64 = 1_792_614_600_000;

/// The shipped document with `TEST_HELD` held, whatever the shipped file
/// holds today. The held tests plant this so they test the mechanism, not
/// whichever rows happen to be held when they run.
#[cfg(test)]
pub(crate) fn document_holding_system_one() -> String {
    let mut doc: serde_json::Value = serde_json::from_str(DOCUMENT).unwrap();
    doc["held"] = TEST_HELD
        .iter()
        .map(|(provider, model)| {
            serde_json::json!({
                "provider_id": provider,
                "model_id": model,
                "held_since_ms": TEST_HELD_SINCE_MS,
                "reason": "held by a test: a consumer's catalog assembly refused this row; re-listed once it accepts it",
            })
        })
        .collect();
    serde_json::to_string(&doc).unwrap()
}

/// `document_holding_system_one` with the held entries for these ids removed,
/// so those rows are served as curated again wherever upstream does not
/// currently publish them.
#[cfg(test)]
pub(crate) fn document_releasing(ids: &[(&str, &str)]) -> String {
    let mut doc: serde_json::Value = serde_json::from_str(&document_holding_system_one()).unwrap();
    let held = doc["held"]
        .as_array_mut()
        .expect("the test document holds rows");
    let before = held.len();
    held.retain(|entry| {
        !ids.iter().any(|(provider, model)| {
            entry["provider_id"] == *provider && entry["model_id"] == *model
        })
    });
    assert_eq!(
        before - held.len(),
        ids.len(),
        "every released id must be held in the test document"
    );
    serde_json::to_string(&doc).unwrap()
}

/// The shipped document with nothing held: every row is served as curated
/// wherever upstream does not currently publish it.
#[cfg(test)]
pub(crate) fn document_without_held() -> String {
    let mut doc: serde_json::Value = serde_json::from_str(DOCUMENT).unwrap();
    doc.as_object_mut().unwrap().remove("held");
    serde_json::to_string(&doc).unwrap()
}

/// A loaded row as the store records it.
pub fn store_row(row: &CuratedModel) -> fusiform_store::NewCuratedModel {
    fusiform_store::NewCuratedModel {
        provider_id: row.provider_id.clone(),
        model_id: row.model_id.clone(),
        facts: row.facts.clone(),
        auth_methods: row.auth_methods.clone(),
        source_ref: row.source_ref.clone(),
        established_by: row.established_by.clone(),
        established_at_ms: row.established_at_ms,
        review_by_ms: row.review_by_ms,
    }
}

/// A stored claim as the row shape the route serves.
pub fn from_store(row: fusiform_store::NewCuratedModel) -> CuratedModel {
    CuratedModel {
        provider_id: row.provider_id,
        model_id: row.model_id,
        auth_methods: row.auth_methods,
        facts: row.facts,
        source_ref: row.source_ref,
        established_by: row.established_by,
        established_at_ms: row.established_at_ms,
        review_by_ms: row.review_by_ms,
    }
}

/// Record this table in the store as it stands at a boot dated `boot_at_ms`,
/// returning how many rows were written.
///
/// Held rows are not in effect, so they are passed as not served: the store
/// tombstones them. See `CatalogStore::append_curated_models` for the dating
/// rule and why an unchanged or merely re-dated row writes nothing.
pub fn apply_to_store(
    store: &fusiform_store::CatalogStore,
    table: &Table,
    boot_at_ms: i64,
) -> Result<usize, fusiform_store::CatalogError> {
    let rows: Vec<_> = table
        .models
        .iter()
        .filter(|(key, _)| !table.held.contains_key(*key))
        .map(|(_, row)| store_row(row))
        .collect();
    let held: Vec<_> = table.held.keys().cloned().collect();
    store.append_curated_models(&rows, &held, boot_at_ms)
}

pub fn upstream_collisions(rows: &CuratedModels, seed: &serde_json::Value) -> Vec<String> {
    rows.keys()
        .filter(|(provider, model)| seed[provider]["models"].get(model).is_some())
        .map(|(provider, model)| {
            format!("{provider}/{model}: upstream now publishes this row; remove the curated row")
        })
        .collect()
}

pub fn overdue_at(rows: &CuratedModels, now_ms: i64) -> Vec<String> {
    rows.values().filter(|row| row.review_by_ms < now_ms)
        .map(|row| format!("{}/{}: re-verify {} and re-date established_at_ms and review_by_ms (60 days later), or remove the curated row", row.provider_id, row.model_id, row.source_ref))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rows the shipped file carries, named so a row that silently fails to
    /// load (or a new one) changes this test rather than passing a count.
    const SHIPPED: &[(&str, &str)] = &[
        ("cloudflare-workers-ai", "@cf/cloudflare/clairic"),
        ("cloudflare-workers-ai", "@cf/cloudflare/clef"),
        ("cloudflare-workers-ai", "@cf/cloudflare/clef-flash"),
        ("meta", "muse-wonder-full-1.0"),
        ("typesafe", "jev-1.13.0"),
    ];

    /// The shipped document with one edit applied to the row of that identity.
    fn planted_in(
        provider: &str,
        model: &str,
        edit: impl FnOnce(&mut serde_json::Value),
    ) -> String {
        let mut doc: serde_json::Value = serde_json::from_str(DOCUMENT).unwrap();
        let row = doc["models"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|row| row["provider_id"] == provider && row["model_id"] == model)
            .expect("the planted row is shipped");
        edit(row);
        serde_json::to_string(&doc).unwrap()
    }

    fn planted(edit: impl FnOnce(&mut serde_json::Value)) -> String {
        planted_in("meta", "muse-wonder-full-1.0", edit)
    }

    const SHIPPED_HELD: &[(&str, &str)] = TEST_HELD;
    const HELD_SINCE_MS: i64 = TEST_HELD_SINCE_MS;

    /// The holding test document with one edit applied to its `held` array.
    fn planted_held(edit: impl FnOnce(&mut Vec<serde_json::Value>)) -> String {
        let mut doc: serde_json::Value =
            serde_json::from_str(&document_holding_system_one()).unwrap();
        edit(doc["held"].as_array_mut().unwrap());
        serde_json::to_string(&doc).unwrap()
    }

    #[test]
    fn the_shipped_file_loads_and_its_held_list_names_rows() {
        // What the shipped file holds changes over time, so this checks only
        // that whatever it holds is valid.
        let (_, rejects) = load_table_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
    }

    #[test]
    fn held_entries_load_and_keep_their_rows() {
        let (table, rejects) = load_table_reporting_rejects(&document_holding_system_one());
        assert!(rejects.is_empty(), "{rejects:?}");
        let ids: Vec<(&str, &str)> = table
            .held
            .keys()
            .map(|(provider, model)| (provider.as_str(), model.as_str()))
            .collect();
        assert_eq!(ids, SHIPPED_HELD);
        for entry in table.held.values() {
            assert_eq!(entry.held_since_ms, HELD_SINCE_MS);
        }
        // A held row keeps its full data in `models`.
        let ids: Vec<(&str, &str)> = table
            .models
            .keys()
            .map(|(provider, model)| (provider.as_str(), model.as_str()))
            .collect();
        assert_eq!(ids, SHIPPED);
    }

    #[test]
    fn planted_held_entry_naming_no_row_is_rejected() {
        let doc = planted_held(|held| {
            let mut stray = held[0].clone();
            stray["model_id"] = "@cf/cloudflare/not-a-row".into();
            held.push(stray);
        });
        let (table, rejects) = load_table_reporting_rejects(&doc);
        assert_eq!(rejects.len(), 1, "{rejects:?}");
        assert!(rejects[0].contains("cloudflare-workers-ai/@cf/cloudflare/not-a-row"));
        assert!(rejects[0].contains("must name a row in models"));
        assert_eq!(table.held.len(), SHIPPED_HELD.len());
        assert_eq!(table.models.len(), SHIPPED.len());
    }

    #[test]
    fn planted_duplicate_or_malformed_held_entry_is_rejected() {
        let doc = planted_held(|held| {
            let duplicate = held[0].clone();
            let mut no_reason = held[1].clone();
            no_reason["reason"] = " ".into();
            held.extend([duplicate, serde_json::json!({"provider_id":"typesafe"})]);
            held[1] = no_reason;
        });
        let (table, rejects) = load_table_reporting_rejects(&doc);
        assert_eq!(rejects.len(), 3, "{rejects:?}");
        assert!(rejects[0].contains("needs a reason"), "{rejects:?}");
        assert!(
            rejects[1].contains("duplicate held identity"),
            "{rejects:?}"
        );
        assert!(rejects[2].contains("did not parse"), "{rejects:?}");
        assert_eq!(table.held.len(), SHIPPED_HELD.len() - 1);
        // Control: removing an entry is not a reject; the row is simply served.
        let doc = planted_held(|held| {
            held.pop();
        });
        let (table, rejects) = load_table_reporting_rejects(&doc);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(table.held.len(), SHIPPED_HELD.len() - 1);
        assert!(!table
            .held
            .contains_key(&("typesafe".to_string(), "jev-1.13.0".to_string())));
    }

    /// Load a planted document and return its single reject, after checking
    /// that exactly the planted row was dropped and every other row loaded.
    fn single_reject(doc: &str, provider: &str, model: &str) -> String {
        let (rows, rejects) = load_reporting_rejects(doc);
        assert_eq!(rejects.len(), 1, "{rejects:?}");
        assert_eq!(rows.len(), SHIPPED.len() - 1);
        assert!(!rows.contains_key(&(provider.to_string(), model.to_string())));
        assert!(rejects[0].contains(&format!("{provider}/{model}")));
        rejects[0].clone()
    }

    #[test]
    fn shipped_curated_rows_load_without_rejects() {
        let (rows, rejects) = load_reporting_rejects(DOCUMENT);
        assert!(rejects.is_empty(), "{rejects:?}");
        let ids: Vec<(&str, &str)> = rows
            .keys()
            .map(|(provider, model)| (provider.as_str(), model.as_str()))
            .collect();
        assert_eq!(ids, SHIPPED);
    }

    #[test]
    fn upstream_now_publishes_this_row_remove_the_curated_row() {
        let seed = serde_json::from_str(include_str!("../data/models-dev-seed.json")).unwrap();
        let collisions = upstream_collisions(models(), &seed);
        assert!(collisions.is_empty(), "{}", collisions.join("\n"));
    }

    #[test]
    fn planted_upstream_row_triggers_the_removal_fence() {
        // Every shipped identity, one at a time, so the fence is shown to see
        // each row rather than only the first one in the file.
        for (provider, model) in SHIPPED {
            let mut seed = serde_json::json!({});
            seed[*provider]["models"][*model] = serde_json::json!({});
            let collisions = upstream_collisions(models(), &seed);
            assert_eq!(collisions.len(), 1, "{provider}/{model}: {collisions:?}");
            assert!(collisions[0].starts_with(&format!("{provider}/{model}:")));
            assert!(
                collisions[0].contains("upstream now publishes this row; remove the curated row")
            );
        }
    }

    #[test]
    fn curated_rows_are_within_their_review_dates() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let overdue = overdue_at(models(), now);
        assert!(overdue.is_empty(), "{}", overdue.join("\n"));
    }

    #[test]
    fn planted_overdue_row_requires_reverification_or_removal() {
        // Checked per row: rows carry different review dates, so an instant
        // that makes one row overdue may make others overdue too.
        assert_eq!(models().len(), SHIPPED.len());
        for row in models().values() {
            let id = format!("{}/{}:", row.provider_id, row.model_id);
            assert!(!overdue_at(models(), row.review_by_ms)
                .iter()
                .any(|line| line.starts_with(&id)));
            let overdue = overdue_at(models(), row.review_by_ms + 1);
            let line = overdue
                .iter()
                .find(|line| line.starts_with(&id))
                .unwrap_or_else(|| panic!("{id} must be overdue after its review date"));
            assert!(line.contains("re-verify"));
            assert!(line.contains("or remove"));
        }
    }

    #[test]
    fn planted_unknown_fact_is_rejected() {
        let doc = planted(|row| row["facts"]["model.unpublished"] = "unpublished".into());
        let reject = single_reject(&doc, "meta", "muse-wonder-full-1.0");
        assert!(reject.contains("not in SERVED_FACTS"));
    }

    #[test]
    fn planted_subscription_price_is_rejected() {
        let doc = planted(|row| {
            row["facts"]["rate.input"] = serde_json::json!({"state":"priced","units":1})
        });
        let reject = single_reject(&doc, "meta", "muse-wonder-full-1.0");
        assert!(reject.contains("must not carry a list price"));
        // An apikey row that also admits a subscription login is refused too:
        // the price would be served to the subscription read.
        let doc = planted_in("typesafe", "jev-1.13.0", |row| {
            row["auth_methods"] = serde_json::json!(["apikey", "oauth"])
        });
        let reject = single_reject(&doc, "typesafe", "jev-1.13.0");
        assert!(reject.contains("must not carry a list price"));
    }

    #[test]
    fn planted_model_kind_outside_the_vocabulary_is_rejected() {
        for bad in [
            serde_json::json!("chat"),
            serde_json::json!("Decision"),
            serde_json::json!(""),
            serde_json::json!(null),
            serde_json::json!(["decision"]),
        ] {
            let doc = planted_in("cloudflare-workers-ai", "@cf/cloudflare/clef", |row| {
                row["facts"]["model.kind"] = bad.clone()
            });
            let reject = single_reject(&doc, "cloudflare-workers-ai", "@cf/cloudflare/clef");
            assert!(
                reject.contains("model.kind must be one of"),
                "{bad}: {reject}"
            );
        }
        // Control: every value in the vocabulary loads.
        for kind in fusiform_protocol::MODEL_KINDS {
            let doc = planted_in("cloudflare-workers-ai", "@cf/cloudflare/clef", |row| {
                row["facts"]["model.kind"] = (*kind).into()
            });
            let (rows, rejects) = load_reporting_rejects(&doc);
            assert!(rejects.is_empty(), "{kind}: {rejects:?}");
            assert_eq!(rows.len(), SHIPPED.len());
        }
    }

    #[test]
    fn planted_rate_outside_the_served_shape_is_rejected() {
        let priced = serde_json::json!({"state":"priced", "units":42000000, "exponent":9,
            "currency":"USD", "unit_provenance":{"kind":"stated"}});
        let mut assumed = priced.clone();
        assumed["unit_provenance"] = serde_json::json!({"kind":"assumed_by_policy",
            "policy":"models-dev-usd-v1"});
        let mut no_provenance = priced.clone();
        no_provenance
            .as_object_mut()
            .unwrap()
            .remove("unit_provenance");
        let mut extra = priced.clone();
        extra["floor"] = serde_json::json!({"kind":"unknown"});
        let mut zero_units = priced.clone();
        zero_units["units"] = 0.into();
        let borrowed_zero = serde_json::json!({"state":"stated_zero", "inherited_from":
            {"provider_id":"x", "family":"y", "basis":"alias"}});
        for bad in [
            assumed,
            no_provenance,
            extra,
            zero_units,
            borrowed_zero,
            serde_json::json!({"state":"unpriced", "reason":"missing_rate"}),
            serde_json::json!(0.042),
        ] {
            let doc = planted_in("typesafe", "jev-1.13.0", |row| {
                row["facts"]["rate.input"] = bad.clone()
            });
            let reject = single_reject(&doc, "typesafe", "jev-1.13.0");
            assert!(
                reject.contains("rate.input: must be a priced rate"),
                "{bad}: {reject}"
            );
        }
        // Control: the shipped shape itself loads.
        let doc = planted_in("typesafe", "jev-1.13.0", |row| {
            row["facts"]["rate.input"] = priced
        });
        assert!(load_reporting_rejects(&doc).1.is_empty());
    }

    #[test]
    fn duplicate_and_malformed_rows_are_counted_as_rejects() {
        let mut doc: serde_json::Value = serde_json::from_str(DOCUMENT).unwrap();
        let duplicate = doc["models"][0].clone();
        doc["models"]
            .as_array_mut()
            .unwrap()
            .extend([duplicate, serde_json::json!({})]);
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert_eq!(rows.len(), SHIPPED.len());
        assert_eq!(rejects.len(), 2);
        assert!(rejects[0].contains("duplicate"));
        assert!(rejects[1].contains("did not parse"));
    }
}
