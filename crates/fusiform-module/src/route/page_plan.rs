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
            let plan = Arc::new(build_plan(
                store,
                &request,
                source,
                key,
                budget,
                frame_limit,
                oversized,
            )?);
            *cached = Some(plan.clone());
            plan
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
    // Width rather than a cached timestamp keeps page one's resolved_at a read
    // instant, while allowing the plan to survive ordinary clock ticks.
    let envelope_bytes =
        compact_size(&envelope)? - envelope.resolved_at_ms.to_string().len() + key.timestamp_width;
    let mut plan = PagePlan {
        key,
        budget,
        providers,
        ranges: Vec::new(),
        envelope_bytes,
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
