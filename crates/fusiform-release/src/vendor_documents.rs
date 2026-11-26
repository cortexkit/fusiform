//! Names used by the importer and the consumer for curation-set documents.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VendorDocument {
    AttachmentLimits,
    BillingPlanes,
    CacheLifetimes,
    CanonicalLinks,
    CuratedModels,
    CuratedProviders,
    FactOverrides,
    InputAudioPerToken,
    LoginMethods,
    ModelAliases,
    ModelCreators,
    ModelLineage,
    OutputAudioPerToken,
    PlanPrices,
    QuotaPools,
    SpeedTwins,
    TemperatureRanges,
    WireCorrections,
    WireFamilies,
}

impl VendorDocument {
    /// The published names are also the stems of the source JSON files.
    pub const ALL: &[(Self, &str)] = &[
        (Self::AttachmentLimits, "attachment-limits"),
        (Self::BillingPlanes, "billing-planes"),
        (Self::CacheLifetimes, "cache-lifetimes"),
        (Self::CanonicalLinks, "canonical-links"),
        (Self::CuratedModels, "curated-models"),
        (Self::CuratedProviders, "curated-providers"),
        (Self::FactOverrides, "fact-overrides"),
        (Self::InputAudioPerToken, "input-audio-per-token"),
        (Self::LoginMethods, "login-methods"),
        (Self::ModelAliases, "model-aliases"),
        (Self::ModelCreators, "model-creators"),
        (Self::ModelLineage, "model-lineage"),
        (Self::OutputAudioPerToken, "output-audio-per-token"),
        (Self::PlanPrices, "plan-prices"),
        (Self::QuotaPools, "quota-pools"),
        (Self::SpeedTwins, "speed-twins"),
        (Self::TemperatureRanges, "temperature-ranges"),
        (Self::WireCorrections, "wire-corrections"),
        (Self::WireFamilies, "wire-families"),
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find_map(|&(document, key)| (key == name).then_some(document))
    }
}
