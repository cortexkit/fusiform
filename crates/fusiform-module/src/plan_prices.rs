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

        rows.push(PlanPrice {
            provider_id: provider_id.to_string(),
            tier: tier.to_string(),
            price,
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

#[cfg(test)]
mod tests {
    use super::*;

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
