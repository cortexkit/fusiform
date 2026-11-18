use std::collections::BTreeMap;

use fusiform_core::SourceId;
use fusiform_protocol::{UncertainFactWire, WithheldFactWire};

use crate::{
    alias_applies, annotate_speed_twin, apply_curated_row, apply_login_methods, apply_speed_twin,
    curated_applies, derive_row_facts, finish_view, inherit_rates, inheritance_source, merge_alias,
    project_alias, render_row, response_envelope, CurationSet, Quality, RawFacts, ServedState,
    RATE,
};

/// The upstream state at the instant being published, before any serve-only
/// additions or replacements. Include withdrawn upstream ids: they suppress
/// live aliases, even though their upstream facts are not served by default.
pub struct UpstreamState {
    pub source: SourceId,
    pub resolved_at_ms: i64,
    pub catalog_version: i64,
    pub models: BTreeMap<String, BTreeMap<String, RawFacts>>,
    pub provider_names: BTreeMap<String, String>,
    pub withheld: Vec<WithheldFactWire>,
    pub uncertain: Vec<UncertainFactWire>,
}

impl UpstreamState {
    fn model(&self, provider: &str, model: &str) -> Option<&RawFacts> {
        self.models.get(provider)?.get(model)
    }

    fn present(&self, provider: &str, model: &str) -> bool {
        self.model(provider, model).is_some_and(is_present)
    }

    fn provider_name(&self, provider: &str) -> Option<&str> {
        self.provider_names.get(provider).map(String::as_str)
    }
}

fn is_present(row: &RawFacts) -> bool {
    row.get("existence")
        .is_some_and(|value| value == "\"present\"")
}

/// Build the unfiltered, unpaged current catalog without a billing plane.
/// Consume upstream rows as they are rendered so their stored JSON doesn't
/// stay resident alongside the entire served view. The caller supplies current
/// operator judgments and the version and instant to put on the envelope.
pub fn curate(
    mut upstream: UpstreamState,
    curation: &CurationSet<'_>,
    quality: &Quality,
) -> ServedState {
    let origins = inheritance_cards(&upstream, curation);
    let aliases = projected_aliases(&upstream, curation);
    let curated = curation
        .curated_models
        .models
        .iter()
        .filter_map(|(id, row)| {
            curated_applies(
                upstream.present(&id.0, &id.1),
                curation.curated_models.held.contains_key(id),
            )
            .then_some(row)
        })
        .collect::<Vec<_>>();
    let twins = curation
        .speed_twins
        .values()
        .map(|row| (row, upstream.present(&row.provider_id, &row.twin_model_id)))
        .collect::<Vec<_>>();

    let mut response = response_envelope(
        upstream.source,
        upstream.resolved_at_ms,
        upstream.catalog_version,
        std::mem::take(&mut upstream.withheld),
        std::mem::take(&mut upstream.uncertain),
    );
    for (provider, models) in std::mem::take(&mut upstream.models) {
        for (model, raw) in models {
            if is_present(&raw) {
                render_current(&origins, curation, &mut response, &provider, &model, &raw);
            }
        }
    }
    response.models.retain(|_, facts| !facts.is_empty());
    for alias in aliases {
        merge_alias(&mut response, alias);
    }
    for row in curated {
        apply_curated_row(
            &mut response,
            row,
            upstream.provider_name(&row.provider_id),
            None,
            None,
            false,
        );
    }
    apply_login_methods(curation.login_methods, &mut response, None);
    derive_row_facts(
        &mut response,
        Some(curation),
        curation.cache_lifetimes,
        quality,
        None,
    );
    for (twin, present) in twins {
        annotate_speed_twin(twin, &mut response);
        let base = format!("{}/{}", twin.provider_id, twin.base_model_id);
        if present && response.models.contains_key(&base) {
            let id = format!("{}/{}", twin.provider_id, twin.twin_model_id);
            if let Some(card) = response.models.get(&id).cloned() {
                apply_speed_twin(twin, &base, card, &mut response);
            }
        }
    }
    finish_view(&mut response, Some(curation.speed_twins), None);
    response
}

fn projected_aliases(upstream: &UpstreamState, curation: &CurationSet<'_>) -> Vec<ServedState> {
    curation
        .aliases
        .iter()
        .filter_map(|((provider, model), alias)| {
            if !alias_applies(upstream.model(provider, model).is_some()) {
                return None;
            }
            let p = &alias.target_provider_id;
            let m = &alias.target_model_id;
            let raw = upstream.model(p, m).filter(|row| is_present(row))?;
            let target_id = format!("{p}/{m}");
            let mut target = response_envelope(
                upstream.source,
                upstream.resolved_at_ms,
                upstream.catalog_version,
                upstream
                    .withheld
                    .iter()
                    .filter(|w| w.model == target_id)
                    .cloned()
                    .collect(),
                upstream
                    .uncertain
                    .iter()
                    .filter(|u| &u.provider_id == p && &u.model_id == m)
                    .cloned()
                    .collect(),
            );
            render_current(&upstream.models, curation, &mut target, p, m, raw);
            project_alias(
                target,
                provider,
                model,
                alias,
                None,
                upstream.provider_name(provider),
            )
        })
        .collect()
}

type RawCatalog = BTreeMap<String, BTreeMap<String, RawFacts>>;

fn inheritance_cards(upstream: &UpstreamState, curation: &CurationSet<'_>) -> RawCatalog {
    let mut cards = RawCatalog::new();
    let mut scratch = response_envelope(
        upstream.source,
        upstream.resolved_at_ms,
        upstream.catalog_version,
        upstream.withheld.clone(),
        Vec::new(),
    );
    for (provider, models) in &upstream.models {
        for (model, raw) in models {
            // Rendering can add a rate or replace its value but cannot remove
            // a published rate key. Only unpriced rows can need a creator card.
            if !is_present(raw) || raw.keys().any(|key| key.starts_with(RATE)) {
                continue;
            }
            let facts = decode_current(curation, &mut scratch, provider, model, raw);
            scratch.fact_overrides.clear();
            scratch.wire_corrections.clear();
            scratch.canonical_links.clear();
            let Some((creator, _)) = inheritance_source(curation.creators, &facts) else {
                continue;
            };
            if let Some(origin) = upstream.model(creator, model) {
                cards
                    .entry(creator.to_owned())
                    .or_default()
                    .entry(model.clone())
                    .or_insert_with(|| {
                        origin
                            .iter()
                            .filter(|(key, _)| key.starts_with(RATE))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    });
            }
        }
    }
    cards
}

fn decode_current(
    curation: &CurationSet<'_>,
    response: &mut ServedState,
    provider: &str,
    model: &str,
    raw: &RawFacts,
) -> crate::Facts {
    let identity = format!("{provider}/{model}");
    let withheld: Vec<_> = response
        .withheld
        .iter()
        .filter(|w| w.model == identity)
        .map(|w| w.fact_key.clone())
        .collect();
    let keys: Vec<_> = withheld.iter().map(String::as_str).collect();
    render_row(
        response,
        (provider, model),
        raw.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        &keys,
        None,
        None,
        Some(curation),
    )
}

fn render_current(
    origins: &RawCatalog,
    curation: &CurationSet<'_>,
    response: &mut ServedState,
    provider: &str,
    model: &str,
    raw: &RawFacts,
) {
    let mut facts = decode_current(curation, response, provider, model, raw);
    if let Some((creator, family)) = inheritance_source(curation.creators, &facts) {
        if let Some(origin) = origins.get(creator).and_then(|rows| rows.get(model)) {
            inherit_rates(
                &mut facts,
                creator,
                &family,
                origin.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            );
        }
    }
    response.models.insert(format!("{provider}/{model}"), facts);
}
