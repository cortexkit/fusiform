//! Curated subscription list prices, keyed `(provider_id, tier)`.
//!
//! A second plane beside the model catalog, for a fact models.dev has no
//! concept of: what a subscription costs per month. Requested by insula for a
//! subscription-to-API cost multiplier, and it lives here rather than there
//! because it is a DATED FACT THAT ROTS — a compiled-in constant answers "what
//! the binary was built with", and the question is "what was in force then".
//!
//! # Why the provider id is models.dev's
//!
//! The quota source names the same vendors differently — `codex` and `claude`
//! against `openai` and `anthropic` — and publishes a separate `apiProvider`
//! field carrying models.dev slugs precisely because the vocabularies differ.
//! Keying on that field means this plane mints no second spelling.
//!
//! # Why the tier string is never normalised
//!
//! The same string names plans an order of magnitude apart across vendors:
//! `pro` is a low consumer tier at one and a top tier at another. Any
//! canonicalisation step is a place those could be brought together, and that
//! error is invisible downstream — it produces a healthy-looking multiplier
//! wrong by 10x.
//!
//! A consequence, deliberate: a RENAMED TIER DOES NOT RESOLVE TO THE OLD PRICE.
//! Exact match only. A renamed tier may be a repriced plan, so failing into
//! "tier observed, no published price" turns an unannounced vendor change into
//! a prompt — and the prompt is cheap while the silent inheritance is the
//! failure this plane exists to prevent.

use std::sync::OnceLock;

/// One curated row: a price, or a positive refusal saying why there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPrice {
    pub provider_id: String,
    pub tier: String,
    /// `None` means the tier was observed and no price is published for it at
    /// `source_ref`. That is a claim rather than a gap, which is why
    /// `refusal_reason` is required alongside it.
    pub price: Option<Amount>,
    /// Joined onto served rows, never part of the stored price claim.
    pub quota_value: Option<fusiform_protocol::QuotaValueWire>,
    pub refusal_reason: Option<String>,
    /// When the price took effect, as best the source allows.
    ///
    /// No vendor in this plane publishes effective dates, so in practice this
    /// is the instant the page was READ — the earliest instant the row can be
    /// defended from. Recording it as an effective date anyway would be the
    /// dressing-up this store's boundary kinds exist to prevent.
    pub boundary_at_ms: i64,
    pub established_by: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
    pub source_ref: String,
}

/// A subscription price in minor units, matching the catalog's money shape.
///
/// A price is money and must not become a float here for the same reason it
/// must not there: 20.00 is not representable in binary floating point, and a
/// figure that feeds a ratio is exactly where that bites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Amount {
    pub minor_units: i64,
    pub exponent: i32,
    pub currency: &'static str,
    pub period: &'static str,
}

const PLAN_PRICES: &str = include_str!("../data/plan-prices.json");

/// What this plane's prices are quoted on, served to consumers verbatim.
///
/// Read from the file rather than restated here. A second copy of a policy
/// sentence is a thing that can disagree with the rows it describes, and the
/// disagreement would be invisible: the file's editor changes the basis, the
/// constant keeps saying the old one, and every consumer reads the constant.
///
/// THE STATEMENT, NOT ITS REASONING. The file also carries `why_this_basis`,
/// which argues for monthly-billed over annual and is for whoever edits the
/// file. It is deliberately NOT on the wire: it ran to 1,243 characters, and a
/// consumer decoding this field got an argument it cannot act on while an
/// operator reading the CLI got a wall of prose under four rows. A policy a
/// reader skips is a policy nobody knows.
pub static TIER_VOCABULARY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    serde_json::from_str::<serde_json::Value>(PLAN_PRICES)
        .ok()
        .and_then(|v| v["tier_vocabulary"].as_str().map(str::to_string))
        .unwrap_or_else(|| "tier vocabulary missing from the curated file".to_string())
});

pub static UNIT_POLICY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    serde_json::from_str::<serde_json::Value>(PLAN_PRICES)
        .ok()
        .and_then(|v| v["unit_policy"].as_str().map(str::to_string))
        .unwrap_or_else(|| {
            // Reached only if the file lost its policy line, which the loader
            // test would already have caught. Says so rather than inventing a
            // basis, because a wrong basis is worse than a missing one.
            "unit policy missing from the curated file".to_string()
        })
});

static LOADED: OnceLock<Vec<PlanPrice>> = OnceLock::new();

/// The curated rows, parsed once per process.
pub fn rows() -> &'static Vec<PlanPrice> {
    LOADED.get_or_init(|| load_reporting_rejects(PLAN_PRICES).0)
}

/// Parse, returning the rows and a reason for each one refused.
///
/// Rejects are counted rather than fatal, matching the window overlay: the file
/// is embedded at compile time, so a malformed row is a defect in the tree
/// rather than bad input at runtime, and a test asserting zero rejects fails
/// before anything ships. Panicking here would take down a daemon whose catalog
/// serving has nothing to do with this plane.
///
/// Taking the document as an argument rather than reading the constant makes
/// the malformed cases drivable. A loader that can only be run against the
/// shipped file can only ever be tested on inputs that are already correct.
pub fn load_reporting_rejects(doc: &str) -> (Vec<PlanPrice>, Vec<String>) {
    let mut rows = Vec::new();
    let mut rejects = Vec::new();

    let parsed: serde_json::Value = match serde_json::from_str(doc) {
        Ok(v) => v,
        Err(e) => return (rows, vec![format!("the document is not JSON: {e}")]),
    };

    let Some(cells) = parsed["cells"].as_array() else {
        return (rows, vec!["no `cells` array".to_string()]);
    };

    for (i, cell) in cells.iter().enumerate() {
        let at = |what: &str| format!("cell {i}: {what}");

        let (Some(provider_id), Some(tier)) = (cell["provider_id"].as_str(), cell["tier"].as_str())
        else {
            rejects.push(at("missing provider_id or tier"));
            continue;
        };

        // Every provenance field is required, and a missing one is a REJECT
        // rather than a default. A row whose establisher defaulted to empty
        // would serve exactly like one whose establisher is real, and the whole
        // point of this plane is that a hand-authored fact carries who stated
        // it.
        let (
            Some(boundary_at_ms),
            Some(established_at_ms),
            Some(review_by_ms),
            Some(established_by),
            Some(source_ref),
        ) = (
            cell["boundary_at_ms"].as_i64(),
            cell["established_at_ms"].as_i64(),
            cell["review_by_ms"].as_i64(),
            cell["established_by"].as_str(),
            cell["source_ref"].as_str(),
        )
        else {
            rejects.push(at(
                "missing provenance: boundary, establisher, or review date",
            ));
            continue;
        };

        let price_value = &cell["price"];
        let refusal = cell["refusal_reason"].as_str();

        // The same coherence the table enforces, checked here so a malformed
        // row is named with its cell index rather than arriving as a SQLite
        // constraint violation with no file position in it.
        let price = if price_value.is_null() {
            let Some(reason) = refusal else {
                rejects.push(at(
                    "no price and no refusal_reason: an absent price must say WHICH \
                     absence it is, because 'not observed' and 'observed and unpriced' \
                     are different states and only one is work",
                ));
                continue;
            };
            let _ = reason;
            None
        } else {
            if refusal.is_some() {
                rejects.push(at(
                    "carries both a price and a refusal_reason: a row cannot state a \
                     price and explain why it has none",
                ));
                continue;
            }
            let (Some(minor_units), Some(exponent), Some(currency), Some(period)) = (
                price_value["minor_units"].as_i64(),
                price_value["exponent"].as_i64(),
                price_value["currency"].as_str(),
                price_value["period"].as_str(),
            ) else {
                rejects.push(at(
                    "incomplete price: a value without its exponent, currency and \
                     period is a number rather than an amount",
                ));
                continue;
            };
            // Leaked rather than allocated: these are a closed vocabulary from a
            // compiled-in file, so the alternative is an owned String on a type
            // that is otherwise Copy, for no benefit.
            Some(Amount {
                minor_units,
                exponent: exponent as i32,
                currency: Box::leak(currency.to_string().into_boxed_str()),
                period: Box::leak(period.to_string().into_boxed_str()),
            })
        };

        // The same cell twice is a contradiction in an AUTHORED artifact, and
        // it is refused here rather than by a unique index in the store.
        //
        // The store must accept the same key at the same vendor instant twice,
        // because that is how a mistyped price is corrected: the vendor's
        // effective date did not change, fusiform's reading of it did. A file
        // carrying the shape twice is a different thing — nobody edits a row by
        // pasting a second copy below it — and whichever copy lost would be
        // silently ignored.
        if let Some(prior) = rows.iter().position(|r: &PlanPrice| {
            r.provider_id == provider_id && r.tier == tier && r.boundary_at_ms == boundary_at_ms
        }) {
            rejects.push(at(&format!(
                "duplicates cell {prior}: {provider_id}/{tier} already has a row at \
                 this boundary, and one of the two would be silently ignored"
            )));
            continue;
        }

        // A bad multiplier must not erase an independently sourced price.
        let quota_value = match cell.get("quota_value") {
            None => None,
            Some(value) => match parse_quota_value(value, price.is_some()) {
                Ok(value) => Some(value),
                Err(reason) => {
                    rejects.push(at(&format!("quota_value: {reason}")));
                    None
                }
            },
        };

        rows.push(PlanPrice {
            provider_id: provider_id.to_string(),
            tier: tier.to_string(),
            price,
            quota_value,
            refusal_reason: refusal.map(str::to_string),
            boundary_at_ms,
            established_by: established_by.to_string(),
            established_at_ms,
            review_by_ms,
            source_ref: source_ref.to_string(),
        });
    }

    (rows, rejects)
}

fn parse_quota_value(
    value: &serde_json::Value,
    has_price: bool,
) -> Result<fusiform_protocol::QuotaValueWire, String> {
    use fusiform_protocol::QuotaValueWire;
    if !has_price {
        return Err("a refusal row has no subscription dollar to divide by".to_string());
    }
    let value: QuotaValueWire = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    match &value {
        QuotaValueWire::Measured {
            low,
            high,
            basis,
            token_mix,
            established_by,
            source_ref,
            as_of_ms,
            review_by_ms,
        } => {
            if low.units <= 0 || high.units <= 0 || low.exponent > 6 || high.exponent > 6 {
                return Err("endpoints must be positive exact decimals with exponent <= 6".into());
            }
            // Scale in integers, including differently scaled endpoints. i128
            // holds every i64 endpoint multiplied by at most a million.
            if i128::from(low.units) * 10i128.pow(6 - low.exponent)
                > i128::from(high.units) * 10i128.pow(6 - high.exponent)
            {
                return Err("low is greater than high".into());
            }
            if [basis, token_mix, established_by, source_ref]
                .iter()
                .any(|s| s.trim().is_empty())
            {
                return Err("required strings must not be empty".into());
            }
            if review_by_ms <= as_of_ms {
                return Err("review_by_ms must be after as_of_ms".into());
            }
        }
        QuotaValueWire::NotEstablished { refusal_reason, .. } => {
            if refusal_reason.trim().is_empty() {
                return Err("refusal_reason must not be empty".into());
            }
        }
    }
    Ok(value)
}

/// The curated rows as the store records them.
///
/// Lives here rather than at the startup call site because `main.rs` is
/// unreachable by every test in this repo — a mapping written there is correct
/// by inspection and by nothing else. This one already hid a defect: it
/// defaulted `period` to "month" for refusal rows, minting a value the file
/// never stated, and that only surfaced because moving it somewhere testable
/// forced the question.
pub fn store_rows(rows: &[PlanPrice]) -> Vec<fusiform_store::NewPlanPrice> {
    rows.iter()
        .map(|r| fusiform_store::NewPlanPrice {
            provider_id: r.provider_id.clone(),
            tier: r.tier.clone(),
            minor_units: r.price.map(|a| a.minor_units),
            exponent: r.price.map(|a| a.exponent),
            currency: r.price.map(|a| a.currency.to_string()),
            // Absent for a refusal, with the rest of the money group. A
            // defaulted period would be indistinguishable from a stated one.
            period: r.price.map(|a| a.period.to_string()),
            boundary_at_ms: r.boundary_at_ms,
            established_by: r.established_by.clone(),
            established_at_ms: r.established_at_ms,
            review_by_ms: r.review_by_ms,
            source_ref: r.source_ref.clone(),
            refusal_reason: r.refusal_reason.clone(),
        })
        .collect()
}

/// A row whose review date has passed, with everything needed to act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overdue {
    pub subject: &'static str,
    pub provider_id: String,
    pub tier: String,
    pub days_overdue: i64,
    pub source_ref: String,
}

/// Which prices and quota values are past their own review dates at `now_ms`.
///
/// Takes the instant rather than reading the clock, so the gate can be driven
/// in both directions. A check that can only ever be run against the real
/// present can only be tested on data that is currently fine.
pub fn overdue_at(rows: &[PlanPrice], now_ms: i64) -> Vec<Overdue> {
    rows.iter()
        .flat_map(|r| {
            let mut overdue = Vec::new();
            if r.review_by_ms < now_ms {
                overdue.push(Overdue {
                    subject: "price",
                    provider_id: r.provider_id.clone(),
                    tier: r.tier.clone(),
                    days_overdue: (now_ms - r.review_by_ms) / 86_400_000,
                    source_ref: r.source_ref.clone(),
                });
            }
            if let Some(value) = &r.quota_value {
                use fusiform_protocol::QuotaValueWire;
                let (review_by_ms, source) = match value {
                    QuotaValueWire::Measured {
                        review_by_ms,
                        source_ref,
                        ..
                    } => (*review_by_ms, source_ref.clone()),
                    QuotaValueWire::NotEstablished {
                        review_by_ms,
                        refusal_reason,
                    } => (*review_by_ms, format!("not established: {refusal_reason}")),
                };
                if review_by_ms < now_ms {
                    overdue.push(Overdue {
                        subject: "quota_value",
                        provider_id: r.provider_id.clone(),
                        tier: r.tier.clone(),
                        days_overdue: (now_ms - review_by_ms) / 86_400_000,
                        source_ref: source,
                    });
                }
            }
            overdue
        })
        .collect()
}

/// What an operator should read when the gate fires.
///
/// The message is part of the design rather than a detail of it. A
/// time-triggered gate fails on a morning when nobody changed anything, which
/// is the least expected failure a suite can produce — the natural reading is
/// "CI is broken" rather than "a fact expired". A reader who reaches that
/// conclusion reaches for a skip, and the gate will have trained exactly the
/// behaviour it exists to prevent.
pub fn overdue_report(overdue: &[Overdue]) -> String {
    let mut out = String::from(
        "NOT A BUILD FAILURE. Nothing changed; a review date passed.\n\n\
         These curated subscription prices or quota values are past their review date. Nothing \
         reveals a plan reprice — no fetch, no diff, no contradiction — so this \
         date is the only signal that a price may have moved, and the damage is \
         retroactive: every window priced in the stale period used the old \
         number.\n\n",
    );
    for o in overdue {
        out.push_str(&format!(
            "  {}/{} {} — {} day(s) overdue\n    read: {}\n",
            o.provider_id, o.tier, o.subject, o.days_overdue, o.source_ref
        ));
    }
    out.push_str(
        "\nOpen each source, read the number, and commit:\n\
         \x20 - the price, IF it moved\n\
         \x20 - a new established_by, established_at_ms and review_by_ms, ALWAYS\n\n\
         For quota_value, revisit the measurement or operator statement (or the \
         refusal reason). Update its figure and provenance, including as_of_ms \
         for a measured value, and its review_by_ms.\n\n\
         Confirming a price unchanged is a real result and the commit should \
         look like one. If it did not move, the only edit is the provenance.\n\n\
         What this gate CANNOT tell you: whether a price is still correct. It \
         knows only that nobody has looked recently.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured_quota() -> serde_json::Value {
        serde_json::json!({
            "state": "measured",
            "low": {"units": 574, "exponent": 1},
            "high": {"units": 608, "exponent": 1},
            "basis": "weekly usage divided by consumed capacity",
            "token_mix": "agentic coding",
            "established_by": "ASTRO",
            "source_ref": "daily rows",
            "as_of_ms": 1,
            "review_by_ms": 1000
        })
    }

    fn quota_document(quota: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"cells": [{
            "provider_id": "acme", "tier": "pro",
            "price": {"minor_units": 2000, "exponent": 2, "currency": "USD", "period": "month"},
            "quota_value": quota,
            "boundary_at_ms": 1, "established_by": "reader", "established_at_ms": 2,
            "review_by_ms": 9_000_000_000_000i64, "source_ref": "price page"
        }]})
    }

    #[test]
    fn well_formed_quota_values_load() {
        let mut differently_scaled = measured_quota();
        differently_scaled["high"] = serde_json::json!({"units": 61, "exponent": 0});
        for quota in [
            measured_quota(),
            differently_scaled,
            serde_json::json!({"state":"not_established", "refusal_reason":"no measured account", "review_by_ms":1000}),
        ] {
            let (rows, rejects) =
                load_reporting_rejects(&quota_document(quota.clone()).to_string());
            assert!(rejects.is_empty(), "{rejects:?}");
            assert_eq!(rows.len(), 1);
            assert_eq!(
                serde_json::to_value(rows[0].quota_value.as_ref().unwrap()).unwrap(),
                quota
            );
        }
    }

    #[test]
    fn malformed_quota_is_rejected_without_losing_the_price() {
        let mut cases = Vec::new();
        let mut both = measured_quota();
        both["refusal_reason"] = "also refused".into();
        cases.push(("both shapes".to_string(), both));
        cases.push((
            "neither shape".into(),
            serde_json::json!({"review_by_ms":1000}),
        ));
        cases.push(("null".into(), serde_json::Value::Null));
        let mut reversed = measured_quota();
        reversed["low"] = serde_json::json!({"units": 61, "exponent": 0});
        cases.push(("low exceeds high across scales".into(), reversed));
        for endpoint in ["low", "high"] {
            for units in [0, -1] {
                let mut quota = measured_quota();
                quota[endpoint]["units"] = units.into();
                cases.push((format!("{endpoint} units {units}"), quota));
            }
            for exponent in [7, -1] {
                let mut quota = measured_quota();
                quota[endpoint]["exponent"] = exponent.into();
                cases.push((format!("{endpoint} exponent {exponent}"), quota));
            }
        }
        for field in ["basis", "token_mix", "established_by", "source_ref"] {
            let mut missing = measured_quota();
            missing.as_object_mut().unwrap().remove(field);
            cases.push((format!("missing {field}"), missing));
            for empty in ["", "  "] {
                let mut quota = measured_quota();
                quota[field] = empty.into();
                cases.push((format!("empty {field}"), quota));
            }
        }
        for review in [0, 1] {
            let mut quota = measured_quota();
            quota["review_by_ms"] = review.into();
            cases.push((format!("review not after as-of: {review}"), quota));
        }
        for field in ["as_of_ms", "review_by_ms", "low", "high"] {
            let mut quota = measured_quota();
            quota.as_object_mut().unwrap().remove(field);
            cases.push((format!("missing {field}"), quota));
        }
        for reason in [None, Some(""), Some(" ")] {
            let mut quota = serde_json::json!({"state":"not_established", "review_by_ms":1000});
            if let Some(reason) = reason {
                quota["refusal_reason"] = reason.into();
            }
            cases.push((format!("refusal reason {reason:?}"), quota));
        }
        let mut both_refused = serde_json::json!({"state":"not_established", "refusal_reason":"none", "review_by_ms":1000});
        both_refused["low"] = measured_quota()["low"].clone();
        both_refused["high"] = measured_quota()["high"].clone();
        cases.push(("refusal with measured endpoints".into(), both_refused));
        for (case, quota) in cases {
            let (rows, rejects) = load_reporting_rejects(&quota_document(quota).to_string());
            assert_eq!(rejects.len(), 1, "{case}: {rejects:?}");
            assert!(
                rejects[0].contains("cell 0: quota_value"),
                "{case}: {rejects:?}"
            );
            assert_eq!(rows.len(), 1, "{case}: the price must survive");
            assert_eq!(rows[0].price.unwrap().minor_units, 2000, "{case}");
            assert!(
                rows[0].quota_value.is_none(),
                "{case}: invalid quota must not load"
            );
        }
    }

    #[test]
    fn a_price_refusal_cannot_carry_a_quota_value() {
        for quota in [
            measured_quota(),
            serde_json::json!({"state":"not_established", "refusal_reason":"no measured account", "review_by_ms":1000}),
        ] {
            let mut doc = quota_document(quota);
            doc["cells"][0]["price"] = serde_json::Value::Null;
            doc["cells"][0]["refusal_reason"] = "tier is ambiguous".into();
            let (rows, rejects) = load_reporting_rejects(&doc.to_string());
            assert_eq!(rejects.len(), 1);
            assert_eq!(rows.len(), 1);
            assert!(rows[0].price.is_none());
            assert!(rows[0].quota_value.is_none());
            assert_eq!(rows[0].refusal_reason.as_deref(), Some("tier is ambiguous"));
        }
    }

    #[test]
    fn changing_only_quota_value_writes_no_plan_price_era() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let store = fusiform_store::CatalogStore::open(&cortexkit_store_types::StorageDescriptor {
            module_id: "fusiform".into(),
            storage_namespace: "default".into(),
            isolation: cortexkit_store_types::Isolation::Module,
            backend: cortexkit_store_types::StorageBackend::Sqlite {
                path: path.to_string_lossy().into_owned(),
            },
        })
        .unwrap();
        let mut doc: serde_json::Value = serde_json::from_str(PLAN_PRICES).unwrap();
        let (rows, rejects) = load_reporting_rejects(&doc.to_string());
        assert!(rejects.is_empty());
        assert_eq!(
            store.append_plan_prices(&store_rows(&rows)).unwrap(),
            rows.len()
        );
        // Change the quota value, its observation time and review date, without
        // changing the price claim that append_plan_prices compares.
        doc["cells"][0]["quota_value"]["low"]["units"] = 580.into();
        doc["cells"][0]["quota_value"]["as_of_ms"] = 1791590400000i64.into();
        doc["cells"][0]["quota_value"]["review_by_ms"] = 1794268800000i64.into();
        let (changed, rejects) = load_reporting_rejects(&doc.to_string());
        assert!(rejects.is_empty());
        assert_ne!(rows[0].quota_value, changed[0].quota_value);
        assert_eq!(store.append_plan_prices(&store_rows(&changed)).unwrap(), 0);
        let conn = rusqlite::Connection::open(path).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM plan_price_era", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count,
            rows.len() as i64,
            "quota changes must not enter the price history"
        );
    }

    #[test]
    fn overdue_quota_values_fire_for_both_shapes() {
        for quota in [
            measured_quota(),
            serde_json::json!({"state":"not_established", "refusal_reason":"no measured account", "review_by_ms":1000}),
        ] {
            let (rows, rejects) = load_reporting_rejects(&quota_document(quota).to_string());
            assert!(rejects.is_empty(), "{rejects:?}");
            let overdue = overdue_at(&rows, 1000 + 3 * 86_400_000);
            assert_eq!(
                overdue.len(),
                1,
                "the quota expires independently of its price"
            );
            let message = overdue_report(&overdue);
            assert!(
                message.contains("acme/pro quota_value") && message.contains("3 day(s)"),
                "{message}"
            );
            assert!(
                message.contains("daily rows") || message.contains("no measured account"),
                "{message}"
            );
            assert!(
                overdue_at(&rows, 999).is_empty(),
                "an in-date quota must not fire"
            );
        }
    }

    /// One priced row and one refusal, so a mapping test can assert BOTH
    /// directions. A fixture with only one kind lets a mapping that handles
    /// neither pass half the assertions.
    fn rows_for_test() -> &'static [PlanPrice] {
        static ROWS: OnceLock<Vec<PlanPrice>> = OnceLock::new();
        ROWS.get_or_init(|| {
            let doc = r#"{"cells":[
                {"provider_id":"acme","tier":"pro",
                 "price":{"minor_units":2000,"exponent":2,"currency":"USD","period":"month"},
                 "boundary_at_ms":1,"established_by":"x","established_at_ms":2,
                 "review_by_ms":3,"source_ref":"s"},
                {"provider_id":"acme","tier":"enterprise","price":null,
                 "refusal_reason":"tier observed, no published price",
                 "boundary_at_ms":1,"established_by":"x","established_at_ms":2,
                 "review_by_ms":3,"source_ref":"s"}
            ]}"#;
            let (rows, rejects) = load_reporting_rejects(doc);
            assert!(rejects.is_empty(), "{rejects:?}");
            rows
        })
    }

    /// The shipped file parses with nothing rejected.
    ///
    /// The rejects are non-fatal at runtime precisely so this test is what
    /// catches a malformed row, before a release rather than after one.
    #[test]
    fn the_shipped_file_has_no_rejects() {
        let (rows, rejects) = load_reporting_rejects(PLAN_PRICES);
        assert!(
            rejects.is_empty(),
            "the shipped file must parse: {rejects:?}"
        );
        assert!(
            !rows.is_empty(),
            "and it must contain rows — an empty file would satisfy the assertion \
             above while proving nothing"
        );
    }

    /// Every incoherent shape is named with its cell, rather than reaching the
    /// store and failing as a constraint violation with no file position.
    ///
    /// Driven through the loader with synthetic documents, because the shipped
    /// file is by definition correct and cannot exercise any of this.
    #[test]
    fn incoherent_rows_are_rejected_with_their_position() {
        let no_reason = r#"{"cells":[{"provider_id":"a","tier":"t","price":null,
            "boundary_at_ms":1,"established_by":"x","established_at_ms":2,
            "review_by_ms":3,"source_ref":"s"}]}"#;
        let (rows, rejects) = load_reporting_rejects(no_reason);
        assert!(rows.is_empty());
        assert!(
            rejects
                .iter()
                .any(|r| r.contains("cell 0") && r.contains("WHICH")),
            "an absent price must say which absence it is: {rejects:?}"
        );

        let both = r#"{"cells":[{"provider_id":"a","tier":"t",
            "price":{"minor_units":1,"exponent":2,"currency":"USD","period":"month"},
            "refusal_reason":"why","boundary_at_ms":1,"established_by":"x",
            "established_at_ms":2,"review_by_ms":3,"source_ref":"s"}]}"#;
        let (rows, rejects) = load_reporting_rejects(both);
        assert!(rows.is_empty());
        assert!(
            rejects
                .iter()
                .any(|r| r.contains("both a price and a refusal")),
            "a row cannot do both: {rejects:?}"
        );

        let half = r#"{"cells":[{"provider_id":"a","tier":"t",
            "price":{"minor_units":1},"boundary_at_ms":1,"established_by":"x",
            "established_at_ms":2,"review_by_ms":3,"source_ref":"s"}]}"#;
        let (rows, rejects) = load_reporting_rejects(half);
        assert!(rows.is_empty());
        assert!(
            rejects.iter().any(|r| r.contains("incomplete price")),
            "a bare number is not an amount: {rejects:?}"
        );

        let no_provenance = r#"{"cells":[{"provider_id":"a","tier":"t",
            "price":{"minor_units":1,"exponent":2,"currency":"USD","period":"month"},
            "boundary_at_ms":1}]}"#;
        let (rows, rejects) = load_reporting_rejects(no_provenance);
        assert!(rows.is_empty());
        assert!(
            rejects.iter().any(|r| r.contains("missing provenance")),
            "a row without an establisher serves exactly like one with a real \
             establisher, which is the whole thing this plane refuses: {rejects:?}"
        );

        let twice = r#"{"cells":[
            {"provider_id":"a","tier":"t",
             "price":{"minor_units":1,"exponent":2,"currency":"USD","period":"month"},
             "boundary_at_ms":1,"established_by":"x","established_at_ms":2,
             "review_by_ms":3,"source_ref":"s"},
            {"provider_id":"a","tier":"t",
             "price":{"minor_units":9,"exponent":2,"currency":"USD","period":"month"},
             "boundary_at_ms":1,"established_by":"x","established_at_ms":2,
             "review_by_ms":3,"source_ref":"s"}
        ]}"#;
        let (rows, rejects) = load_reporting_rejects(twice);
        assert_eq!(rows.len(), 1, "the first copy loads, the second is refused");
        assert!(
            rejects.iter().any(|r| r.contains("duplicates cell 0")),
            "a repeated cell must name the row it collides with, because the \
             reader has to find the OTHER one to decide which is right: {rejects:?}"
        );

        // CONTROL: a well-formed row must still load. Without this arm every
        // assertion above passes against a loader that rejects everything.
        let good = r#"{"cells":[{"provider_id":"anthropic","tier":"pro",
            "price":{"minor_units":2000,"exponent":2,"currency":"USD","period":"month"},
            "boundary_at_ms":1,"established_by":"x","established_at_ms":2,
            "review_by_ms":3,"source_ref":"s"}]}"#;
        let (rows, rejects) = load_reporting_rejects(good);
        assert!(rejects.is_empty(), "control: {rejects:?}");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].price.expect("priced").minor_units, 2000);
    }

    /// No curated price or quota value is past its review date.
    ///
    /// CI runs this test on pushes and on its daily 06:00 schedule. The daily
    /// run catches a review date passing even when no edit triggers a push:
    /// curated facts can go stale without anyone changing the repository.
    /// It reads the real clock because a fixed instant would test only the
    /// comparison, not whether the shipped facts are still within review.
    #[test]
    fn no_curated_price_is_past_its_review_date() {
        let (rows, rejects) = load_reporting_rejects(PLAN_PRICES);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert!(!rows.is_empty(), "a file with no rows cannot be overdue");

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_millis() as i64;

        let overdue = overdue_at(&rows, now);
        assert!(overdue.is_empty(), "\n\n{}", overdue_report(&overdue));
    }

    /// The gate fires, and its message says the four things a reader needs.
    ///
    /// Without this the test above passes every day until the first lapse, and
    /// the first person to meet a real failure meets an untested message at the
    /// moment they are least inclined to read carefully.
    #[test]
    fn an_overdue_row_fires_and_the_message_is_actionable() {
        let doc = r#"{"cells":[{"provider_id":"acme","tier":"pro",
            "price":{"minor_units":2000,"exponent":2,"currency":"USD","period":"month"},
            "boundary_at_ms":1,"established_by":"x","established_at_ms":2,
            "review_by_ms":1000,"source_ref":"https://example/pricing"}]}"#;
        let (rows, rejects) = load_reporting_rejects(doc);
        assert!(rejects.is_empty(), "{rejects:?}");

        let overdue = overdue_at(&rows, 1000 + 86_400_000 * 3);
        assert_eq!(overdue.len(), 1, "an expired row must be reported");
        assert_eq!(overdue[0].days_overdue, 3);

        let msg = overdue_report(&overdue);
        assert!(
            msg.contains("NOT A BUILD FAILURE"),
            "must lead with this, or a reader spends the first minutes \
             debugging a build that is fine: {msg}"
        );
        assert!(
            msg.contains("acme/pro") && msg.contains("3 day(s)"),
            "must name the row and how far overdue, or someone hunts a file: {msg}"
        );
        assert!(
            msg.contains("https://example/pricing"),
            "must carry the source, so checking is a click rather than a search: {msg}"
        );
        assert!(
            msg.contains("unchanged is a real result"),
            "must say what to commit when the price did NOT move, or 'nothing \
             changed' reads as wasted work and the next reader skips it: {msg}"
        );
        assert!(
            msg.contains("CANNOT tell you"),
            "must state its own limit: it knows nobody looked recently, not \
             that the price is correct: {msg}"
        );

        // CONTROL: a row still in date must NOT fire, or the gate reports
        // everything and its output becomes noise.
        assert!(
            overdue_at(&rows, 999).is_empty(),
            "a row inside its review window must not be reported"
        );
    }

    /// A refusal maps to the store with NO money parts, period included.
    ///
    /// The mapping defaulted `period` to "month" for refusals when it lived at
    /// the startup call site, where nothing could reach it. A minted period is
    /// indistinguishable downstream from one the source stated — the
    /// fill-a-slot-to-satisfy-the-type class, in a plane whose entire subject
    /// is which claims have a page behind them.
    #[test]
    fn a_refusal_carries_no_money_parts_at_all() {
        let rows = store_rows(rows_for_test());

        let refusal = rows
            .iter()
            .find(|r| r.minor_units.is_none())
            .expect("the fixture must contain a refusal");
        assert!(refusal.exponent.is_none());
        assert!(refusal.currency.is_none());
        assert!(
            refusal.period.is_none(),
            "a refusal has no price and therefore no period; minting one makes \
             it indistinguishable from a stated period"
        );
        assert!(refusal.refusal_reason.is_some());

        // CONTROL: a priced row must carry all four, or the assertions above
        // pass against a mapping that drops the money parts entirely.
        let priced = rows
            .iter()
            .find(|r| r.minor_units.is_some())
            .expect("the fixture must contain a priced row");
        assert!(priced.exponent.is_some());
        assert!(priced.currency.is_some());
        assert_eq!(priced.period.as_deref(), Some("month"));
        assert!(priced.refusal_reason.is_none());
    }

    /// The served policy is the STATEMENT, and its argument stays in the file.
    ///
    /// The two were one field, and driving the CLI against a live daemon showed
    /// what that cost: four rows of prices under a 1,243-character paragraph
    /// arguing for monthly-billed over annual. A consumer decoding it got
    /// reasoning it cannot act on; an operator got a wall they would skip. A
    /// policy a reader skips is a policy nobody knows.
    ///
    /// 400 SITS BETWEEN THE TWO MEASURED VALUES, with margin on both sides:
    /// the policy-as-argument ran to 1,243 characters and the policy-as-
    /// statement is 188. So the bound is derived rather than chosen — it is
    /// twice the good value and a third of the bad one, which leaves room for a
    /// legitimate edit while catching a relapse into prose.
    ///
    /// Deliberately loose for that reason. A tighter number would fail on a
    /// sentence someone adds for a real reason, and a bound that fires on good
    /// work teaches its reader to raise it.
    #[test]
    fn the_served_policy_is_a_statement_rather_than_an_argument() {
        let doc: serde_json::Value =
            serde_json::from_str(PLAN_PRICES).expect("the shipped file parses");

        let served = doc["unit_policy"].as_str().expect("a policy is served");
        assert!(
            served.len() < 400,
            "the served policy is {} chars; the reasoning belongs in \
             why_this_basis, which is not on the wire",
            served.len()
        );

        // It must still STATE the basis. Without this the assertion above
        // passes against an empty string, and a consumer assuming a basis is
        // the failure the field exists to prevent.
        for term in ["US list", "monthly-billed", "web subscription"] {
            assert!(
                served.contains(term),
                "the served policy must name {term:?}: {served}"
            );
        }

        // And the argument must survive SOMEWHERE, or splitting it out becomes
        // a way to quietly delete it.
        let why = doc["why_this_basis"]
            .as_str()
            .expect("the reasoning is kept in the file");
        assert!(
            why.contains("FAILURE DIRECTION"),
            "the reason monthly-billed was chosen must survive the split"
        );
    }

    /// The file carries a concise tier-vocabulary statement for consumers.
    /// `tier` names a vendor tier, not an account's API plan string; leaving
    /// that distinction implicit would invite consumers to make a wrong join.
    /// The response serves the statement, while the longer worked example
    /// stays in `why_tier_vocabulary` for whoever curates the file.
    #[test]
    fn the_tier_vocabulary_is_a_served_statement() {
        let doc: serde_json::Value =
            serde_json::from_str(PLAN_PRICES).expect("the shipped file parses");

        let served = doc["tier_vocabulary"]
            .as_str()
            .expect("a tier vocabulary is served");
        assert!(
            served.len() < 400,
            "the served vocabulary is {} chars; the example belongs in \
             why_tier_vocabulary, which is not on the wire",
            served.len()
        );

        // It must still say the two things a consumer acts on: what the column
        // IS, and that mapping is theirs. Without these the length assertion
        // passes against an empty string.
        // CASE-INSENSITIVE, and that is not laziness. I made this exact
        // mistake in a route assertion earlier today: pinning to CASING fails
        // on an edit that changes nothing the test cares about, which teaches
        // its reader to edit the test rather than the code.
        let lower = served.to_lowercase();
        for term in ["tier name", "api plan string", "map it"] {
            assert!(
                lower.contains(term),
                "the served vocabulary must name {term:?}: {served}"
            );
        }

        // And the example must survive the split, or splitting becomes a way to
        // delete it quietly.
        let why = doc["why_tier_vocabulary"]
            .as_str()
            .expect("the worked example is kept in the file");
        assert!(
            why.contains("Pro 5x") || why.contains("pro_5x"),
            "the concrete case must survive: {why}"
        );
    }

    /// A refusal cell is a ROW, not a dropped one.
    ///
    /// The distinction is the plane's reason for existing: a tier nobody has
    /// looked at and a tier whose vendor publishes no price are different
    /// states, and only the second is a completed job. Dropping refusals would
    /// collapse them into one absence and produce a backlog indistinguishable
    /// from finished work.
    /// # Driven from a synthetic document, and it took a failure to see why
    ///
    /// This ran against the SHIPPED file and asserted it carried a refusal.
    /// That passed while two rows were refusals and broke the moment both were
    /// sourced properly — a test about LOADER BEHAVIOUR coupled to whether the
    /// curated data happened to contain the case.
    ///
    /// The data is free to have no refusals; the loader must still handle one.
    /// Tying the two made correcting a row look like breaking a test, which is
    /// the pressure that eventually keeps a wrong row in a file.
    #[test]
    fn a_refusal_is_a_row_rather_than_a_silence() {
        let doc = r#"{"cells":[
            {"provider_id":"acme","tier":"enterprise","price":null,
             "refusal_reason":"tier observed, no published price at this source",
             "boundary_at_ms":1,"established_by":"x","established_at_ms":2,
             "review_by_ms":3,"source_ref":"https://example/pricing"},
            {"provider_id":"acme","tier":"pro",
             "price":{"minor_units":2000,"exponent":2,"currency":"USD","period":"month"},
             "boundary_at_ms":1,"established_by":"x","established_at_ms":2,
             "review_by_ms":3,"source_ref":"https://example/pricing"}
        ]}"#;
        let (rows, rejects) = load_reporting_rejects(doc);
        assert!(rejects.is_empty(), "{rejects:?}");
        assert_eq!(
            rows.len(),
            2,
            "a refusal is a ROW: dropping it would collapse 'nobody looked' into \
             'looked and found nothing published', and only the second is a \
             completed job"
        );

        let refusals: Vec<_> = rows.iter().filter(|r| r.price.is_none()).collect();
        assert_eq!(refusals.len(), 1, "the refusal must survive loading");
        for r in &refusals {
            assert!(
                r.refusal_reason.is_some(),
                "{}/{} refuses without saying why",
                r.provider_id,
                r.tier
            );
            assert!(
                !r.source_ref.is_empty(),
                "{}/{} must name the page that does NOT publish the price, so a \
                 reviewer can check the same source rather than guess at one",
                r.provider_id,
                r.tier
            );
        }
    }
}
