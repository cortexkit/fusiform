//! Served history resolves dated reads rather than duplicating their precedence.

use super::*;
use fusiform_protocol::{
    HistoryOverride, HistoryRefusalReason, HistorySegment, HistorySource, HistoryState,
};

pub(super) fn serve(store: &CatalogStore, body: &[u8]) -> Result<HistoryResponse, RouteError> {
    let request: HistoryRequest = serde_json::from_slice(body)
        .map_err(|e| RouteError::bad_request(format!("request did not parse: {e}")))?;
    if request.fact_key.is_some() == request.fact_prefix.is_some() {
        return Err(RouteError::bad_request(
            "exactly one of fact_key and fact_prefix is required",
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
    boundaries.retain(|at| *at >= begin.0);
    boundaries.insert(begin.0);
    let mut reads = Vec::new();
    let identity = format!("{provider}/{model}");
    for at in boundaries {
        // Keep the full row for override guards and TTL rate derivation, and
        // use the dated-get resolver with inputs owned only by this request.
        let request = CatalogGetRequest {
            source: Some(source.as_str().into()),
            provider_id: Some(provider.clone()),
            model_id: Some(model.clone()),
            at_ms: Some(at),
            ..CatalogGetRequest::default()
        };
        let read = match serve_catalog_get_prepared(
            store,
            request,
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
        if let Some(facts) = read.as_ref().and_then(|read| read.models.get(&identity)) {
            keys.extend(facts.keys().cloned());
        }
        reads.push((at, read));
    }
    if let Some(key) = &request.fact_key {
        response.served = Some(timeline(
            store, source, provider, model, key, &reads, &prepared,
        )?);
    } else {
        keys.retain(|key| key.starts_with(request.fact_prefix.as_deref().unwrap()));
        if request.served == Some(true) {
            response.served_by_key = Some(
                keys.iter()
                    .map(|key| {
                        Ok((
                            key.clone(),
                            timeline(store, source, provider, model, key, &reads, &prepared)?,
                        ))
                    })
                    .collect::<Result<_, RouteError>>()?,
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

fn timeline(
    store: &CatalogStore,
    source: SourceId,
    provider: &str,
    model: &str,
    key: &str,
    reads: &[(i64, Option<CatalogGetResponse>)],
    prepared: &dated_model::DatedModel,
) -> Result<Vec<HistorySegment>, RouteError> {
    let identity = format!("{provider}/{model}");
    let mut segments: Vec<HistorySegment> = Vec::new();
    for (at, read) in reads {
        let mut segment = HistorySegment {
            from_ms: *at,
            until_ms: None,
            state: HistoryState::Absent,
            value: None,
            source: None,
            override_detail: None,
            reason: None,
            correction: None,
        };
        if let Some(read) = read {
            if let Some(withheld) = read
                .withheld
                .iter()
                .find(|w| w.model == identity && w.fact_key == key)
            {
                segment.state = HistoryState::Refused;
                segment.reason = Some(HistoryRefusalReason::Correction);
                segment.correction = withheld.corrections.first().cloned();
            } else if let Some(value) = read.models.get(&identity).and_then(|facts| facts.get(key))
            {
                segment.state = HistoryState::Value;
                segment.value = Some(value.clone());
                if let Some(claim) = read
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
                            .curated_at(Timestamp(*at))
                            .and_then(|row| row.facts.get(key).cloned())
                    } else {
                        None
                    };
                    let mut upstream = prepared
                        .raw(&FactKey::from_stored(key.into()), Timestamp(*at))
                        .map(|raw| {
                            let mut value = fact_for_the_wire(raw);
                            name_the_provenance_of_a_pre_0_11_row(&mut value);
                            value
                        });
                    if key == "provider.name" && curated.is_none() {
                        upstream = store
                            .provider_display_name_at(source, provider, Timestamp(*at))
                            .map_err(|e| store_error(&e))?
                            .map(serde_json::Value::from);
                    }
                    // The resolver discloses which override or curated row it
                    // chose. Compare that claim and the rendered upstream value
                    // with the final value; the only remaining dated derivation
                    // is the cache-lifetime rule. No precedence is decided here.
                    segment.source = Some(if curated.as_ref() == Some(value) {
                        HistorySource::Curated
                    } else if upstream.as_ref() == Some(value) {
                        HistorySource::Upstream
                    } else {
                        HistorySource::CacheLifetime
                    });
                }
            }
        } else {
            segment.state = HistoryState::Refused;
            segment.reason = Some(HistoryRefusalReason::NoCoverage);
        }
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
    Ok(segments)
}
