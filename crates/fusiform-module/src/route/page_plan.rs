//! Cache provider ranges and byte counts, never rendered facts or replies.

use super::*;
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct ProviderRange {
    pub after: Option<String>,
    pub through: Option<String>,
}

impl ProviderRange {
    pub fn contains(&self, provider: &str) -> bool {
        self.after.as_deref().is_none_or(|p| provider > p)
            && self.through.as_deref().is_none_or(|p| provider <= p)
    }
}

struct PagePlan {
    key: WholeCatalogKey,
    budget: usize,
    providers: Vec<(String, ProviderContribution)>,
    ranges: Vec<ProviderRange>,
    envelope_bytes: usize,
    quality_bytes: usize,
}

static PAGE_PLAN: Mutex<Option<Arc<PagePlan>>> = Mutex::new(None);

impl PagePlan {
    fn range_after(&self, after: Option<&str>) -> Result<ProviderRange, RouteError> {
        if let Some(range) = self.ranges.iter().find(|r| r.after.as_deref() == after) {
            return Ok(range.clone());
        }
        // Tokens are versioned provider cursors, not offsets into the cached
        // plan. A valid cursor may name a provider that was not a page boundary.
        self.pack_after(after)
    }

    fn pack_after(&self, after: Option<&str>) -> Result<ProviderRange, RouteError> {
        let remaining: Vec<_> = self
            .providers
            .iter()
            .filter(|(p, _)| after.is_none_or(|after| p.as_str() > after))
            .collect();
        let mut estimate = PageEstimate {
            bytes: self.envelope_bytes,
            present: [false; 10],
            pending_quality_bytes: self.quality_bytes,
        };
        let mut through = None;
        for (index, (provider, contribution)) in remaining.iter().enumerate() {
            let candidate = estimate.with_provider(contribution)?;
            let token_bytes = if index + 1 < remaining.len() {
                let token = format!(
                    "v1:{}:{}:{}",
                    self.key.catalog_version,
                    // Only the width affects packing. No request instant is cached.
                    "0".repeat(self.key.timestamp_width),
                    provider
                );
                compact_size(&token)? + r#","next_page_token":"#.len()
            } else {
                0
            };
            let size = candidate.bytes + token_bytes;
            if size > self.budget && through.is_some() {
                break;
            }
            through = Some(provider.to_string());
            estimate = candidate;
            if size > self.budget {
                break;
            }
        }
        Ok(ProviderRange {
            after: after.map(str::to_string),
            through,
        })
    }
}

pub(super) fn serve_planned_page(
    store: &CatalogStore,
    request: CatalogGetRequest,
    source: SourceId,
    cursor: Option<&PageCursor>,
    budget: usize,
    frame_limit: usize,
    oversized: &Mutex<Option<OversizedCatalog>>,
) -> Result<CatalogGetResponse, RouteError> {
    refuse_before_the_record(store, source, request.at_ms.map(Timestamp))?;
    let mut shape = request.clone();
    shape.paged = false;
    shape.page_token = None;
    let mut key = WholeCatalogKey::new(store, &shape, source, frame_limit)?
        .expect("paging validation excludes named and plane requests");
    if let Some(cursor) = cursor {
        key.timestamp_width = cursor.resolved_at_ms.to_string().len();
    }
    let plan = {
        // Hold the lock while building the small plan so concurrent misses do
        // not duplicate that work. Rendered providers are dropped during the
        // build; ordinary page reads hold the lock only to clone the Arc.
        let mut cached = PAGE_PLAN.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(plan) = cached
            .as_ref()
            .filter(|p| p.key == key && p.budget == budget)
        {
            plan.clone()
        } else {
            loop {
                let plan = Arc::new(build_plan(
                    store,
                    &request,
                    source,
                    key.clone(),
                    budget,
                    frame_limit,
                    oversized,
                )?);
                check_page_version(
                    store.catalog_version().map_err(|e| store_error(&e))?,
                    &PageCursor {
                        catalog_version: key.catalog_version,
                        resolved_at_ms: 0,
                        provider: String::new(),
                    },
                )?;
                let latest = store.latest_era_id().map_err(|e| store_error(&e))?;
                if latest != key.latest_era_id {
                    // A correction can change sizes without advancing the catalog
                    // version. Do not publish a plan measured across that write.
                    key.latest_era_id = latest;
                    continue;
                }
                *cached = Some(plan.clone());
                break plan;
            }
        }
    };
    let range = plan.range_after(cursor.map(|c| c.provider.as_str()))?;
    let mut response =
        serve_catalog_get_request(store, request, budget, frame_limit, oversized, Some(&range))?;
    let rendered = PageCursor {
        catalog_version: response.catalog_version,
        resolved_at_ms: response.resolved_at_ms,
        provider: String::new(),
    };
    check_page_version(
        store.catalog_version().map_err(|e| store_error(&e))?,
        &rendered,
    )?;
    check_page_version(
        response.catalog_version,
        &PageCursor {
            catalog_version: plan.key.catalog_version,
            ..rendered
        },
    )?;
    if let Some(cursor) = cursor {
        check_page_version(response.catalog_version, cursor)?;
        // Keep current derivations, but report the instant resolved by page one.
        response.resolved_at_ms = cursor.resolved_at_ms;
    }
    retain_range(&mut response, &range);
    let has_more = range
        .through
        .as_ref()
        .is_some_and(|last| plan.providers.last().is_some_and(|(p, _)| p > last));
    response.next_page_token = if has_more {
        range.through.map(|provider| {
            format!(
                "v1:{}:{}:{provider}",
                response.catalog_version, response.resolved_at_ms
            )
        })
    } else {
        None
    };
    while compact_size(&response)? > budget {
        let mut providers = partition_providers(&mut response);
        if providers.len() <= 1 {
            for rows in providers.into_values() {
                rows.append_to(&mut response);
            }
            break;
        }
        // Keep a bounded fallback if size accounting ever drifts:
        // discard only the last whole provider and resume before it next time.
        providers.pop_last();
        let last = providers.last_key_value().map(|(p, _)| p.clone()).unwrap();
        for rows in providers.into_values() {
            rows.append_to(&mut response);
        }
        retain_quality_disclosure(&mut response);
        response.next_page_token = Some(format!(
            "v1:{}:{}:{last}",
            response.catalog_version, response.resolved_at_ms
        ));
    }
    Ok(response)
}

fn retain_range(response: &mut CatalogGetResponse, range: &ProviderRange) {
    let providers = partition_providers(response);
    for (provider, rows) in providers {
        if range.contains(&provider) {
            rows.append_to(response);
        }
    }
    retain_quality_disclosure(response);
}

fn build_plan(
    store: &CatalogStore,
    request: &CatalogGetRequest,
    source: SourceId,
    key: WholeCatalogKey,
    budget: usize,
    frame_limit: usize,
    oversized: &Mutex<Option<OversizedCatalog>>,
) -> Result<PagePlan, RouteError> {
    let mut identities: BTreeSet<String> = store
        .catalog_providers(source)
        .map_err(|e| store_error(&e))?
        .into_iter()
        .collect();
    for (provider, _) in crate::aliases::aliases()
        .keys()
        .chain(crate::curated_models::models().keys())
    {
        identities.insert(provider.clone());
    }
    for row in crate::aliases::retired_aliases().values() {
        identities.insert(row.provider_id.clone());
    }
    for row in crate::curated_models::held().values() {
        identities.insert(row.provider_id.clone());
    }
    let mut providers = Vec::new();
    let mut quality = None;
    let mut previous = None;
    let mut raw = request.clone();
    raw.page_token = None;
    for provider in identities {
        let range = ProviderRange {
            after: previous,
            through: Some(provider.clone()),
        };
        let mut response = serve_catalog_get_request(
            store,
            raw.clone(),
            budget,
            frame_limit,
            oversized,
            Some(&range),
        )?;
        retain_range(&mut response, &range);
        if response.quality_scores.is_some() {
            quality = response.quality_scores.clone();
        }
        for (provider, rows) in partition_providers(&mut response) {
            providers.push((provider, rows.contribution()?));
        }
        previous = Some(provider);
    }
    let snapshot = CatalogSnapshot {
        source,
        resolved_at: Timestamp(request.at_ms.unwrap_or_else(now_ms)),
        catalog_version: key.catalog_version,
        models: Vec::new(),
        withheld: Vec::new(),
        uncertain: Vec::new(),
    };
    let mut envelope = render(source, snapshot, request.at_ms.map(Timestamp), None, None);
    envelope.quality_scores = quality.clone();
    let quality_bytes = quality_disclosure_size(quality.as_ref())?;
    // Width rather than a cached timestamp keeps page one's resolved_at a read
    // instant, while allowing the plan to survive ordinary clock ticks.
    let envelope_bytes =
        compact_size(&envelope)? - quality_bytes - envelope.resolved_at_ms.to_string().len()
            + key.timestamp_width;
    let mut plan = PagePlan {
        key,
        budget,
        providers,
        ranges: Vec::new(),
        envelope_bytes,
        quality_bytes,
    };
    let mut after = None;
    loop {
        let range = plan.pack_after(after.as_deref())?;
        after = range.through.clone();
        let done = after.is_none()
            || plan
                .providers
                .last()
                .is_some_and(|(p, _)| Some(p) == after.as_ref());
        plan.ranges.push(range);
        if done {
            break;
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};

    #[test]
    fn ungraded_provider_pages_do_not_pay_for_quality_disclosure() {
        let (store, _dir) = super::super::paging_tests::fixture();
        let mut request = CatalogGetRequest::default();
        let cache = Mutex::new(None);
        let whole = serve_catalog_get_request(
            &store,
            request.clone(),
            usize::MAX,
            usize::MAX,
            &cache,
            None,
        )
        .unwrap();
        assert!(
            whole.quality_scores.is_some(),
            "later providers must actually carry grades"
        );
        let mut expected = whole.clone();
        scope_page(
            &mut expected,
            &BTreeSet::from(["cloudflare-workers-ai".into(), "elevenlabs".into()]),
        );
        assert!(expected.quality_scores.is_none());
        expected.next_page_token = Some(format!(
            "v1:{}:{}:elevenlabs",
            whole.catalog_version, whole.resolved_at_ms,
        ));
        // Both ungraded providers fit exactly when measured as a real reply;
        // charging a later provider's quality disclosure would evict ElevenLabs.
        let budget = serde_json::to_vec(&expected).unwrap().len();
        request.paged = true;
        let page =
            serve_catalog_get_request(&store, request, budget, usize::MAX, &cache, None).unwrap();
        assert_eq!(
            page.models.keys().collect::<Vec<_>>(),
            expected.models.keys().collect::<Vec<_>>()
        );
        assert!(page.quality_scores.is_none());
        assert_eq!(serde_json::to_vec(&page).unwrap().len(), budget);
    }

    #[test]
    fn planned_seed_pages_match_the_original_whole_render_packer() {
        let dir = tempfile::tempdir().unwrap();
        let store = CatalogStore::open(&StorageDescriptor {
            module_id: "fusiform".into(),
            storage_namespace: "default".into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.path().join("store.db").to_string_lossy().into_owned(),
            },
        })
        .unwrap();
        crate::seed::seed_if_empty(&store).unwrap();
        crate::curated_models::apply_to_store(&store, crate::curated_models::table(), now_ms())
            .unwrap();
        for prefixes in [
            None,
            Some(vec!["rate.".to_string()]),
            Some(vec!["quality.".to_string()]),
        ] {
            let request = CatalogGetRequest {
                fact_prefixes: prefixes,
                ..Default::default()
            };
            let cache = Mutex::new(None);
            let whole = serve_catalog_get_request(
                &store,
                request.clone(),
                CATALOG_PAGE_BYTES,
                usize::MAX,
                &cache,
                None,
            )
            .unwrap();
            let mut request = request;
            request.paged = true;
            let mut cursor = None;
            loop {
                let page = serve_catalog_get_request(
                    &store,
                    request.clone(),
                    CATALOG_PAGE_BYTES,
                    usize::MAX,
                    &cache,
                    None,
                )
                .unwrap();
                assert!(
                    serde_json::to_vec(&page).unwrap().len() <= CATALOG_PAGE_BYTES,
                    "the real seed page exceeds the 3 MiB budget"
                );
                let mut reference = whole.clone();
                reference.resolved_at_ms = page.resolved_at_ms;
                let reference =
                    page_catalog(reference, cursor.as_ref(), CATALOG_PAGE_BYTES).unwrap();
                assert_eq!(
                    serde_json::to_vec(&page).unwrap(),
                    serde_json::to_vec(&reference).unwrap()
                );
                request.page_token = page.next_page_token;
                cursor = request
                    .page_token
                    .as_deref()
                    .map(parse_page_token)
                    .transpose()
                    .unwrap();
                if cursor.is_none() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod correction_tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
    use fusiform_core::{BoundaryKind, Correction, FieldId, TokenClass};
    use fusiform_store::{FactKey, NewEra};

    #[test]
    fn a_correction_replans_boundaries_without_a_version_change() {
        let dir = tempfile::tempdir().unwrap();
        let store = CatalogStore::open(&StorageDescriptor {
            module_id: "fusiform".into(),
            storage_namespace: "default".into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.path().join("store.db").to_string_lossy().into_owned(),
            },
        })
        .unwrap();
        let mut rows = Vec::new();
        for provider in ["a", "b", "c"] {
            for (key, value) in [
                (FactKey::existence(), "\"present\""),
                (
                    FactKey::rate(TokenClass::Input),
                    "{\"state\":\"stated_zero\"}",
                ),
            ] {
                rows.push(NewEra {
                    source: SourceId::ModelsDev,
                    provider_id: provider.into(),
                    model_id: "model".into(),
                    fact_key: key,
                    value_json: value.into(),
                    boundary_at: Timestamp(1_000),
                    boundary_kind: BoundaryKind::Seed,
                    observation_id: None,
                });
            }
        }
        store.append_eras(&rows).unwrap();
        let request = CatalogGetRequest {
            paged: true,
            at_ms: Some(1_500),
            ..Default::default()
        };
        let cache = Mutex::new(None);
        let before =
            serve_catalog_get_request(&store, request.clone(), 1_000, usize::MAX, &cache, None)
                .unwrap();
        let version = store.catalog_version().unwrap();
        store
            .append_eras(&[NewEra {
                source: SourceId::ModelsDev,
                provider_id: "a".into(),
                model_id: "model".into(),
                fact_key: FactKey::rate(TokenClass::Input),
                value_json: "{\"state\":\"stated_zero\"}".into(),
                boundary_at: Timestamp(3_000),
                boundary_kind: BoundaryKind::Corrected(Correction {
                    fields: vec![FieldId::Rate {
                        class: TokenClass::Input,
                    }],
                    affected_from: Timestamp(1_000),
                    affected_until: Timestamp(2_000),
                    reason: "invalid rate ".repeat(100),
                }),
                observation_id: None,
            }])
            .unwrap();
        assert_eq!(store.catalog_version().unwrap(), version);
        let after =
            serve_catalog_get_request(&store, request.clone(), 1_000, usize::MAX, &cache, None)
                .unwrap();
        let whole = serve_catalog_get_request(
            &store,
            CatalogGetRequest {
                paged: false,
                ..request
            },
            1_000,
            usize::MAX,
            &cache,
            None,
        )
        .unwrap();
        let expected = page_catalog(whole, None, 1_000).unwrap();
        assert_eq!(
            serde_json::to_vec(&after).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        assert_ne!(before.next_page_token, after.next_page_token);
        assert_eq!(after.models.keys().collect::<Vec<_>>(), vec!["a/model"]);
    }
}
