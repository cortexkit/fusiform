#![forbid(unsafe_code)]

//! In-memory curation of catalog facts. The caller supplies the upstream rows,
//! curation documents, and operator judgments; no stage opens a store or reads
//! a clock. The smaller stages also serve scoped and dated daemon reads.

use std::collections::BTreeMap;

pub use fusiform_protocol::CatalogGetResponse as ServedState;
use serde_json::Value;

pub mod aliases;
pub mod attachment_limits;
pub mod billing_planes;
pub mod cache_lifetimes;
pub mod canonical_links;
pub mod creators;
pub mod curated_models;
pub mod fact_overrides;
pub mod input_audio;
pub mod login_methods;
pub mod model_lineage;
pub mod plan_prices;
pub mod quota_pools;
pub mod speed_twins;
pub mod temperature_ranges;
pub mod wire_corrections;
pub mod wire_families;

mod catalog;
mod finish;
mod project;
mod quality_efforts;
mod render;

pub use catalog::{curate, UpstreamState};
pub use finish::*;
pub use project::*;
pub use quality_efforts::*;
pub use render::*;

pub type Facts = BTreeMap<String, Value>;
/// Fact keys and their raw JSON values, exactly as the store records upstream facts.
pub type RawFacts = BTreeMap<String, String>;

/// Already parsed curation tables. Nothing here reads a file or a built-in copy
/// of the models.dev catalog: the caller supplies every table, so the same
/// curation can run inside fusiform or inside a publisher with no store.
pub struct CurationSet<'a> {
    pub aliases: &'a aliases::Aliases,
    pub retired_aliases: &'a BTreeMap<(String, String), aliases::RetiredAlias>,
    pub attachment_limits: &'a attachment_limits::Sources,
    pub billing_planes: &'a billing_planes::Planes,
    pub cache_lifetimes: &'a cache_lifetimes::Rules,
    pub canonical_links: &'a canonical_links::CanonicalLinks,
    pub creators: &'a creators::Creators,
    pub curated_models: &'a curated_models::Table,
    pub fact_overrides: &'a fact_overrides::FactOverrides,
    pub input_audio: &'a input_audio::InputAudioSources,
    pub login_methods: &'a login_methods::Rows,
    pub model_lineage: &'a model_lineage::ModelLineage,
    pub speed_twins: &'a speed_twins::SpeedTwins,
    pub temperature_ranges: &'a temperature_ranges::Ranges,
    pub wire_corrections: &'a wire_corrections::WireCorrections,
    pub wire_families: &'a wire_families::Families,
}

const RATE: &str = "rate.";

fn is_mode_rate(key: &str) -> bool {
    key.starts_with(RATE) && key.contains(".mode.")
}

/// Drop inputs that were only fetched to derive the requested facts.
pub fn narrow_to_requested(facts: &mut Facts, prefixes: Option<&Vec<String>>) {
    if let Some(prefixes) = prefixes {
        facts.retain(|key, _| prefixes.iter().any(|p| key.starts_with(p)));
    }
}
