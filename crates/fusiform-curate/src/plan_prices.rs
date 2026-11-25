//! Parse curated subscription prices without depending on their storage.

/// One curated row: a price, or a positive refusal saying why there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPrice {
    pub provider_id: String,
    pub tier: String,
    /// `None` means the tier was observed and no price is published for it at
    /// `source_ref`. That is a claim rather than a gap, which is why
    /// `refusal_reason` is required alongside it.
    pub price: Option<Amount>,
    /// Served on this plan's `plan.prices` row, never part of the stored price claim.
    pub quota_value: Option<fusiform_protocol::QuotaValueWire>,
    /// Speed tiers the plan includes, served on its `plan.prices` row and not
    /// part of the stored price claim.
    pub service_tiers: Option<fusiform_protocol::ServiceTiersWire>,
    /// Concurrent-request limits, served on the plan's `plan.prices` row and
    /// not part of the stored price claim.
    pub concurrency: Option<fusiform_protocol::ConcurrencyWire>,
    pub refusal_reason: Option<String>,
    /// When the price took effect, as best the source allows.
    ///
    /// No vendor of these subscription plans publishes effective dates, so in practice this
    /// is when the source was read, the earliest instant the price can be
    /// defended from. An effective date would imply a date the vendor stated.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Amount {
    pub minor_units: i64,
    pub exponent: i32,
    pub currency: String,
    pub period: String,
}

/// Parse, returning the rows and a reason for each one refused.
///
/// Release validation treats any reject as fatal. Returning the accepted
/// rows separately lets the embedded catalog keep serving usable prices and
/// report defects with their source positions.
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
            // Published documents are reparsed during verification and replay.
            // Keep these strings owned so discarding the rows frees them.
            Some(Amount {
                minor_units,
                exponent: exponent as i32,
                currency: currency.to_owned(),
                period: period.to_owned(),
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

        // Invalid entitlements must not discard an independently sourced price.
        let service_tiers = match cell.get("service_tiers") {
            None => None,
            Some(value) => match parse_service_tiers(value) {
                Ok(value) => Some(value),
                Err(reason) => {
                    rejects.push(at(&format!("service_tiers: {reason}")));
                    None
                }
            },
        };

        // A malformed cap must not erase an independently sourced price.
        let concurrency = match cell.get("concurrency") {
            None => None,
            Some(value) => match parse_concurrency(value) {
                Ok(value) => Some(value),
                Err(reason) => {
                    rejects.push(at(&format!("concurrency: {reason}")));
                    None
                }
            },
        };

        rows.push(PlanPrice {
            provider_id: provider_id.to_string(),
            tier: tier.to_string(),
            price,
            quota_value,
            service_tiers,
            concurrency,
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

fn parse_service_tiers(
    value: &serde_json::Value,
) -> Result<fusiform_protocol::ServiceTiersWire, String> {
    use fusiform_protocol::ServiceTiersWire;
    let value: ServiceTiersWire =
        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    match &value {
        ServiceTiersWire::Established {
            tiers,
            source_ref,
            established_by,
            established_at_ms,
            review_by_ms,
        } => {
            if tiers.is_empty() {
                return Err("an established entitlement must list at least one tier".into());
            }
            for (i, tier) in tiers.iter().enumerate() {
                if tiers[..i].iter().any(|prior| prior.tier == tier.tier) {
                    return Err("a service tier is listed more than once".into());
                }
                for rate in std::iter::once(tier.included_usage_rate).chain(tier.credit_rate) {
                    if rate.units <= 0 || rate.exponent > 6 {
                        return Err(
                            "rates must be positive exact decimals with exponent <= 6".into()
                        );
                    }
                }
            }
            if [source_ref, established_by]
                .iter()
                .any(|s| s.trim().is_empty())
            {
                return Err("required strings must not be empty".into());
            }
            if review_by_ms <= established_at_ms {
                return Err("review_by_ms must be after established_at_ms".into());
            }
        }
        ServiceTiersWire::NotEstablished { refusal_reason, .. } => {
            if refusal_reason.trim().is_empty() {
                return Err("refusal_reason must not be empty".into());
            }
        }
    }
    Ok(value)
}

fn parse_concurrency(
    value: &serde_json::Value,
) -> Result<fusiform_protocol::ConcurrencyWire, String> {
    use fusiform_protocol::{ConcurrencyScope, ConcurrencyWire};
    if value
        .get("per_model")
        .is_some_and(serde_json::Value::is_null)
    {
        return Err("per_model must be a map when present".into());
    }
    let value: ConcurrencyWire =
        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    if value.limit == 0 {
        return Err("limit must be a positive integer".into());
    }
    if let Some(per_model) = &value.per_model {
        if value.scope != ConcurrencyScope::AccountModel {
            return Err("per_model requires account_model scope".into());
        }
        if per_model.values().any(|limit| *limit == 0) {
            return Err("per_model limits must be positive integers".into());
        }
    }
    if [&value.source_ref, &value.quote]
        .iter()
        .any(|s| s.trim().is_empty())
    {
        return Err("source_ref and quote must not be empty".into());
    }
    if value.review_by_ms <= value.established_at_ms {
        return Err("review_by_ms must be after established_at_ms".into());
    }
    Ok(value)
}
