//! Curated billing claims for a provider's authentication planes.
//!
//! These claims are applied only when serving a current plane view, never at
//! ingest. A subscription's API rates are quota weights rather than a bill,
//! and a cache write billed as input is not the same thing as a free write.
//! Both distinctions need sources and independent review dates, including on
//! a plane that has no per-class rules.

use std::collections::BTreeMap;

use fusiform_core::TokenClass;

/// The login methods a caller may name to select a billing plane. The set is
/// closed: an unlisted method is refused, never treated as an API key.
pub const AUTH_METHODS: &[&str] = &["apikey", "chatgpt", "oauth", "antigravity", "gateway"];

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

/// How one provider bills calls made with one login method, as curated in
/// `data/billing-planes.json` with the source the claim rests on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    pub provider_id: String,
    pub auth_method: String,
    pub kind: PlaneKind,
    /// Borrow only the exact canonical row from this provider on a quota view.
    pub rates_from_provider: Option<String>,
    /// A sourced protocol specific to this login plane, not its API price card.
    pub wire_family: Option<String>,
    pub source_ref: String,
    pub established_by: String,
    pub established_at_ms: i64,
    pub review_by_ms: i64,
    pub rules: Vec<BillingRule>,
}

impl Plane {
    /// The different provider whose canonical API card this plane uses in place
    /// of its own rates, if one is configured. Where a row has no matching card
    /// there, the route serves its rates as unpriced instead.
    ///
    /// A `rates_from_provider` that names the plane's own provider borrows
    /// nothing: it states explicitly that the provider's own published rates
    /// are the quota weights. An `apikey` quota proxy must say where its prices
    /// come from, and for a subscription sold as an API key (OpenCode Go) the
    /// answer is the provider itself.
    pub fn borrows_rates_from(&self) -> Option<&str> {
        self.rates_from_provider
            .as_deref()
            .filter(|provider| *provider != self.provider_id)
    }
}

/// How one token class (cache writes, say) is billed on a plane. It carries
/// its own source and review date, because a plane can be right about its
/// prices while one class rule rests on a different page.
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

#[derive(serde::Deserialize)]
struct PlaneRow {
    provider_id: String,
    auth_method: String,
    kind: PlaneKind,
    rates_from_provider: Option<String>,
    wire_family: Option<String>,
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
                "unknown auth_method {:?}; expected apikey, chatgpt, oauth, antigravity or gateway",
                row.auth_method
            )));
            continue;
        }
        if let Some(provider) = &row.rates_from_provider {
            if row.kind != PlaneKind::QuotaProxy || provider.trim().is_empty() {
                rejects.push(at(
                    "rates_from_provider needs a non-empty provider on a quota_proxy plane",
                ));
                continue;
            }
        }
        if row.kind == PlaneKind::QuotaProxy
            && row.auth_method == "apikey"
            && row.rates_from_provider.is_none()
        {
            rejects.push(at("quota_proxy is not allowed on apikey: an API price is upstream, not a subscription quota weight"));
            continue;
        }
        let key = (row.provider_id.clone(), row.auth_method.clone());
        if row
            .wire_family
            .as_deref()
            .is_some_and(|family| !crate::wire_families::is_known(family))
        {
            rejects.push(at("unknown wire_family"));
            continue;
        }
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
                rates_from_provider: row.rates_from_provider,
                wire_family: row.wire_family,
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

/// A plane or rule whose review date has passed, carrying the identity and
/// source a reviewer needs to find the claim and check it again.
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
        TokenClass::InputAudio => "input_audio",
        TokenClass::OutputAudio => "output_audio",
    }
}
