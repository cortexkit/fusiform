//! The daemon embeds the reviewed documents; the merge itself only sees the
//! parsed tables, so a publisher can supply tables from another source.

use fusiform_curate::CurationSet;

mod published;
pub(crate) use published::document_rows;
pub use published::validate_published;
pub(crate) use published::PublishedCuration;

pub fn embedded() -> CurationSet<'static> {
    CurationSet {
        aliases: crate::aliases::aliases(),
        retired_aliases: crate::aliases::retired_aliases(),
        attachment_limits: crate::attachment_limits::sources(),
        billing_planes: crate::billing_planes::planes(),
        cache_lifetimes: crate::cache_lifetimes::rules(),
        canonical_links: crate::canonical_links::links(),
        creators: crate::creators::creators(),
        curated_models: crate::curated_models::table(),
        fact_overrides: crate::fact_overrides::overrides(),
        input_audio: crate::input_audio::sources(),
        login_methods: crate::login_methods::rows(),
        model_lineage: crate::model_lineage::classifications(),
        speed_twins: crate::speed_twins::rows(),
        temperature_ranges: crate::temperature_ranges::ranges(),
        wire_corrections: crate::wire_corrections::corrections(),
        complete_providers: crate::wire_corrections::complete_providers(),
        wire_families: crate::wire_families::families(),
    }
}
