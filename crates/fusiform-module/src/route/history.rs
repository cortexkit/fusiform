//! Served history resolves dated reads rather than duplicating their precedence.

use super::*;
use fusiform_protocol::{
    HistoryOverride, HistoryRefusalReason, HistorySegment, HistorySource, HistoryState,
};

fn is_judgment_key(key: &str) -> bool {
    key.starts_with("quality.") || key.starts_with("workload.")
}

pub(super) fn serve(store: &CatalogStore, body: &[u8]) -> Result<HistoryResponse, RouteError> {
    let request: HistoryRequest = serde_json::from_slice(body)
        .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?;
    if request.fact_key.is_some() == request.fact_prefix.is_some() {
        return Err(RouteError::bad_request(
            "exactly one of fact_key and fact_prefix is required",
        ));
    }
    if request.fact_key.as_deref().is_some_and(is_judgment_key)
        || request.fact_prefix.as_deref().is_some_and(|prefix| {
            is_judgment_key(prefix) || matches!(prefix, "quality" | "workload")
        })
    {
        return Err(RouteError::bad_request(
            "catalog.history does not include judgment facts; use quality.grades with a model to read judgment history",
        ));
    }
    if request.fact_key.is_some() && request.served != Some(true) {
        return upstream_history(store, request);
    }
    let source = parse_source(request.source.as_deref())?;
    let provider = &request.provider_id;
    let model = &request.model_id;
    let mut boundaries = store
        .model_history_boundaries(source, provider, model)
        .map_err(|e| store_error(&e))?;
    let mut keys = store
        .model_history_keys(source, provider, model)
        .map_err(|e| store_error(&e))?;
    keys.retain(|key| !is_judgment_key(key));
    let mut response = if request.fact_key.is_some() {
        match upstream_history(store, request.clone()) {
            Ok(response) => response,
            // A removed curated row can have historical claims without a
            // current card. Its dated resolver decides each interval below.
            Err(error)
                if error.code == fusiform_protocol::CODE_NO_COVERAGE && !boundaries.is_empty() =>
            {
                envelope(&request, source)
            }
            Err(error) => return Err(error),
        }
    } else {
        // Reuse the existing identity checks by probing the existence fact,
        // including current curated and alias ids.
        let mut probe = request.clone();
        probe.fact_key = Some("existence".into());
        probe.fact_prefix = None;
        match upstream_history(store, probe) {
            Ok(mut response) => {
                response.fact_key = None;
                response.eras.clear();
                response.last_changed_at_ms = None;
                response.fact_prefix = request.fact_prefix.clone();
                response
            }
            Err(error)
                if error.code == fusiform_protocol::CODE_NO_COVERAGE && !boundaries.is_empty() =>
            {
                envelope(&request, source)
            }
            Err(error) => return Err(error),
        }
    };
    if response
        .inherited_from
        .as_ref()
        .is_some_and(|origin| origin.basis == "alias")
    {
        if request.served == Some(true) {
            response.inherited_from.as_mut().unwrap().mapping = Some("current".into());
            if request.fact_key.is_some() {
                response.served = Some(Vec::new());
            } else {
                response.served_by_key = Some(BTreeMap::new());
            }
        }
        if request.fact_prefix.is_some() {
            response.fact_keys = Some(Vec::new());
        }
        return Ok(response);
    }
    let prepared = dated_model::DatedModel::load(store, source, provider, model)?;
    let Some(begin) = prepared.begins else {
        response.served = request.fact_key.as_ref().map(|_| Vec::new());
        response.fact_keys = request.fact_prefix.as_ref().map(|_| Vec::new());
        response.served_by_key =
            (request.served == Some(true) && request.fact_prefix.is_some()).then(BTreeMap::new);
        return Ok(response);
    };
    if prepared.curated.iter().any(|era| {
        era.claim
            .as_ref()
            .is_some_and(|claim| !claim.facts.contains_key("provider.name"))
    }) {
        boundaries.extend(
            store
                .provider_label_boundaries(source, provider)
                .map_err(|e| store_error(&e))?,
        );
    }
    prepared.speed_twins.extend_boundaries(&mut boundaries);
    boundaries.retain(|at| *at >= begin.0);
    boundaries.insert(begin.0);
    let mut reads = Vec::new();
    let identity = format!("{provider}/{model}");
    let selected = |key: &str| {
        !is_judgment_key(key)
            && match (&request.fact_key, &request.fact_prefix) {
                (Some(exact), _) => key == exact,
                (_, Some(prefix)) => key.starts_with(prefix),
                _ => false,
            }
    };
    let with_timeline = request.fact_key.is_some() || request.served == Some(true);
    for at in boundaries {
        // Keep the full row for override guards and TTL rate derivation, and
        // use the dated-get resolver with inputs owned only by this request.
        let dated_request = CatalogGetRequest {
            source: Some(source.as_str().into()),
            provider_id: Some(provider.clone()),
            model_id: Some(model.clone()),
            at_ms: Some(at),
            ..CatalogGetRequest::default()
        };
        let read = match serve_catalog_get_prepared(
            store,
            dated_request,
            CATALOG_PAGE_BYTES,
            REPLY_FRAME_LIMIT_BYTES,
            &OVERSIZED_CATALOG,
            None,
            Some(&prepared),
        ) {
            Ok(read) => Some(read),
            Err(error) if error.code == fusiform_protocol::CODE_NO_COVERAGE => None,
            Err(error) => return Err(error),
        };
        let mut matching = BTreeSet::new();
        if let Some(read) = &read {
            if let Some(facts) = read.models.get(&identity) {
                matching.extend(facts.keys().filter(|key| selected(key)).map(String::as_str));
                keys.extend(matching.iter().map(|key| (*key).to_owned()));
            }
            if with_timeline {
                matching.extend(
                    read.withheld
                        .iter()
                        .filter(|w| w.model == identity && selected(&w.fact_key))
                        .map(|w| w.fact_key.as_str()),
                );
            }
        }
        if with_timeline {
            let projection = read
                .as_ref()
                .map(|read| {
                    matching
                        .into_iter()
                        .map(|key| {
                            Ok((
                                key.to_owned(),
                                segment_at(
                                    store, source, provider, &identity, key, at, read, &prepared,
                                )?,
                            ))
                        })
                        .collect::<Result<_, RouteError>>()
                })
                .transpose()?;
            reads.push((at, projection));
        }
    }
    if let Some(key) = &request.fact_key {
        response.served = Some(timeline(key, &reads));
    } else {
        keys.retain(|key| key.starts_with(request.fact_prefix.as_deref().unwrap()));
        if request.served == Some(true) {
            response.served_by_key = Some(
                keys.iter()
                    .map(|key| (key.clone(), timeline(key, &reads)))
                    .collect(),
            );
        }
        response.fact_keys = Some(keys.into_iter().collect());
    }
    Ok(response)
}

fn envelope(request: &HistoryRequest, source: SourceId) -> HistoryResponse {
    HistoryResponse {
        source: source.as_str().into(),
        provider_id: request.provider_id.clone(),
        model_id: request.model_id.clone(),
        fact_key: request.fact_key.clone(),
        fact_prefix: request.fact_prefix.clone(),
        ..HistoryResponse::default()
    }
}

fn default_segment(at: i64, covered: bool) -> HistorySegment {
    HistorySegment {
        from_ms: at,
        until_ms: None,
        state: if covered {
            HistoryState::Absent
        } else {
            HistoryState::Refused
        },
        value: None,
        source: None,
        override_detail: None,
        reason: (!covered).then_some(HistoryRefusalReason::NoCoverage),
        correction: None,
        corrections: Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn segment_at(
    store: &CatalogStore,
    source: SourceId,
    provider: &str,
    identity: &str,
    key: &str,
    at: i64,
    read: &CatalogGetResponse,
    prepared: &dated_model::DatedModel,
) -> Result<HistorySegment, RouteError> {
    let mut segment = default_segment(at, true);
    if let Some(withheld) = read
        .withheld
        .iter()
        .find(|w| w.model == identity && w.fact_key == key)
    {
        segment.state = HistoryState::Refused;
        segment.reason = Some(HistoryRefusalReason::Correction);
        segment.correction = withheld.corrections.first().cloned();
        segment.corrections.clone_from(&withheld.corrections);
    } else if let Some(value) = read.models.get(identity).and_then(|facts| facts.get(key)) {
        segment.state = HistoryState::Value;
        segment.value = Some(value.clone());
        if prepared
            .speed_twins
            .has_rate_at(identity, key, Timestamp(at))
        {
            segment.source = Some(HistorySource::SpeedTwin);
        } else if let Some(claim) = read
            .fact_overrides
            .iter()
            .find(|o| o.model == identity && o.fact_key == key && &o.value == value)
        {
            segment.source = Some(HistorySource::Override);
            segment.override_detail = Some(HistoryOverride {
                source_ref: claim.source_ref.clone(),
                replaces: claim.replaces.clone(),
            });
        } else {
            let curated = if read.curated.iter().any(|row| row.model == identity) {
                prepared
                    .curated_at(Timestamp(at))
                    .and_then(|row| row.facts.get(key).cloned())
            } else {
                None
            };
            let mut upstream = prepared
                .raw(&FactKey::from_stored(key.into()), Timestamp(at))
                .map(|raw| {
                    let mut value = fact_for_the_wire(raw);
                    name_the_provenance_of_a_pre_0_11_row(&mut value);
                    value
                });
            if key == "provider.name" && curated.is_none() {
                upstream = store
                    .provider_display_name_at(source, provider, Timestamp(at))
                    .map_err(|e| store_error(&e))?
                    .map(serde_json::Value::from);
            }
            // The resolver discloses which override or curated row it
            // chose. Compare that claim and the rendered upstream value
            // with the final value. After checking speed twins above, the
            // remaining dated rate derivation is the cache-lifetime rule.
            segment.source = Some(if curated.as_ref() == Some(value) {
                HistorySource::Curated
            } else if upstream.as_ref() == Some(value) {
                HistorySource::Upstream
            } else {
                HistorySource::CacheLifetime
            });
        }
    }
    Ok(segment)
}

type ProjectedRead = (i64, Option<BTreeMap<String, HistorySegment>>);

fn timeline(key: &str, reads: &[ProjectedRead]) -> Vec<HistorySegment> {
    let mut segments: Vec<HistorySegment> = Vec::new();
    for (at, read) in reads {
        let segment = read
            .as_ref()
            .and_then(|facts| facts.get(key))
            .cloned()
            .unwrap_or_else(|| default_segment(*at, read.is_some()));
        if let Some(last) = segments.last_mut() {
            let mut comparable = segment.clone();
            comparable.from_ms = last.from_ms;
            if *last == comparable {
                continue;
            }
            last.until_ms = Some(*at);
        }
        segments.push(segment);
    }
    segments
}
