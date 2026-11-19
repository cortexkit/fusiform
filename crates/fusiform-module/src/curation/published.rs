use std::collections::{BTreeMap, BTreeSet};

use fusiform_curate as curate;
use fusiform_release::{Upstream, VendorDocument};
use fusiform_store::ImportedTables;
use serde_json::{json, Value};

type Documents = BTreeMap<String, Value>;

pub(crate) struct PublishedCuration {
    aliases: curate::aliases::Aliases,
    retired_aliases: BTreeMap<(String, String), curate::aliases::RetiredAlias>,
    attachment_limits: curate::attachment_limits::Sources,
    billing_planes: curate::billing_planes::Planes,
    cache_lifetimes: curate::cache_lifetimes::Rules,
    canonical_links: curate::canonical_links::CanonicalLinks,
    creators: curate::creators::Creators,
    curated_models: curate::curated_models::Table,
    fact_overrides: curate::fact_overrides::FactOverrides,
    input_audio: curate::input_audio::InputAudioSources,
    login_methods: curate::login_methods::Rows,
    model_lineage: curate::model_lineage::ModelLineage,
    speed_twins: curate::speed_twins::SpeedTwins,
    temperature_ranges: curate::temperature_ranges::Ranges,
    wire_corrections: curate::wire_corrections::WireCorrections,
    complete_providers: curate::wire_corrections::CompleteProviders,
    wire_families: curate::wire_families::Families,
}

impl PublishedCuration {
    pub(crate) fn load(
        store: &fusiform_store::CatalogStore,
        at: Option<i64>,
    ) -> Result<Option<Self>, String> {
        if !store.is_store_mode() {
            return Ok(None);
        }
        let docs = store
            .store_documents_at(at)
            .map_err(|err| err.to_string())?;
        fn load<T: Default>(
            docs: &BTreeMap<String, String>,
            name: &str,
            f: impl FnOnce(&str) -> (T, Vec<String>),
        ) -> Result<T, String> {
            docs.get(name)
                .map_or_else(|| Ok(T::default()), |doc| accepted(name, f(doc)))
        }
        let at = at.unwrap_or(i64::MAX);
        let curated_models = curate::curated_models::Table {
            models: store
                .curated_models_at(at, None)
                .map_err(|err| err.to_string())?
                .into_iter()
                .filter_map(|era| {
                    era.claim.map(|row| {
                        (
                            (era.provider_id, era.model_id),
                            crate::curated_models::from_store(row),
                        )
                    })
                })
                .collect(),
            held: BTreeMap::new(),
        };
        let wire_corrections = load(
            &docs,
            "wire-corrections",
            curate::wire_corrections::load_reporting_rejects,
        )?;
        let login_methods = load(&docs, "login-methods", |doc| {
            // The release validator already established that these identities
            // occur upstream. A later withdrawal must not invalidate an older
            // signed login document on reload.
            let mut models = json!({});
            if let Ok(raw) = serde_json::from_str::<Value>(doc) {
                for row in raw["models"].as_array().into_iter().flatten() {
                    if let (Some(provider), Some(model)) =
                        (row["provider_id"].as_str(), row["model_id"].as_str())
                    {
                        models[provider]["models"][model] = json!({});
                    }
                }
            }
            curate::login_methods::load_with(doc, &models, &curated_models.models)
        })?;
        Ok(Some(Self {
            aliases: load(
                &docs,
                "model-aliases",
                curate::aliases::load_reporting_rejects,
            )?,
            retired_aliases: docs
                .get("model-aliases")
                .map_or_else(BTreeMap::new, |doc| curate::aliases::load_retired(doc)),
            attachment_limits: load(&docs, "attachment-limits", curate::attachment_limits::load)?,
            billing_planes: load(
                &docs,
                "billing-planes",
                curate::billing_planes::load_reporting_rejects,
            )?,
            cache_lifetimes: crate::cache_lifetimes::at(store, at, None)
                .map_err(|err| err.to_string())?,
            canonical_links: load(
                &docs,
                "canonical-links",
                curate::canonical_links::load_reporting_rejects,
            )?,
            creators: load(&docs, "model-creators", curate::creators::parse_document)?,
            curated_models,
            fact_overrides: crate::fact_overrides::at(store, at, None)
                .map_err(|err| err.to_string())?,
            input_audio: load(
                &docs,
                "input-audio-per-token",
                curate::input_audio::load_reporting_rejects,
            )?,
            login_methods,
            model_lineage: load(
                &docs,
                "model-lineage",
                curate::model_lineage::load_reporting_rejects,
            )?,
            speed_twins: load(
                &docs,
                "speed-twins",
                curate::speed_twins::load_reporting_rejects,
            )?,
            temperature_ranges: load(
                &docs,
                "temperature-ranges",
                curate::temperature_ranges::load_reporting_rejects,
            )?,
            complete_providers: load(&docs, "wire-corrections", |doc| {
                curate::wire_corrections::load_complete_providers_reporting_rejects(
                    doc,
                    &wire_corrections,
                )
            })?,
            wire_corrections,
            wire_families: load(
                &docs,
                "wire-families",
                curate::wire_families::load_reporting_rejects,
            )?,
        }))
    }

    pub(crate) fn tables(&self) -> curate::CurationSet<'_> {
        curate::CurationSet {
            aliases: &self.aliases,
            retired_aliases: &self.retired_aliases,
            attachment_limits: &self.attachment_limits,
            billing_planes: &self.billing_planes,
            cache_lifetimes: &self.cache_lifetimes,
            canonical_links: &self.canonical_links,
            creators: &self.creators,
            curated_models: &self.curated_models,
            fact_overrides: &self.fact_overrides,
            input_audio: &self.input_audio,
            login_methods: &self.login_methods,
            model_lineage: &self.model_lineage,
            speed_twins: &self.speed_twins,
            temperature_ranges: &self.temperature_ranges,
            wire_corrections: &self.wire_corrections,
            complete_providers: &self.complete_providers,
            wire_families: &self.wire_families,
        }
    }
}

pub(crate) fn validate_published(documents: &Documents, upstream: &Upstream) -> Result<(), String> {
    use VendorDocument::*;

    let models = Value::Object(
        upstream
            .iter()
            .map(|(provider, facts)| (provider.clone(), json!({"models": facts.models})))
            .collect(),
    );
    let curated = documents
        .get("curated-models")
        .map(curated)
        .transpose()?
        .unwrap_or_default();
    for (name, raw) in documents {
        let doc = raw.to_string();
        let rejects = match document_kind(name)? {
            ModelAliases => curate::aliases::load_reporting_rejects(&doc).1,
            AttachmentLimits => curate::attachment_limits::load(&doc).1,
            BillingPlanes => curate::billing_planes::load_reporting_rejects(&doc).1,
            CacheLifetimes => curate::cache_lifetimes::load_reporting_rejects(&doc).1,
            CanonicalLinks => curate::canonical_links::load_reporting_rejects(&doc).1,
            ModelCreators => curate::creators::parse_document(&doc).1,
            CuratedModels => continue,
            CuratedProviders => {
                // The provider vector uses this document; no serve route does.
                // Retain it intact in the signed curation history.
                if raw["schema"] != "curated-providers/v1" || !raw["providers"].is_array() {
                    return Err(format!(
                        "{name}: expected curated-providers/v1 and a providers array"
                    ));
                }
                continue;
            }
            FactOverrides => curate::fact_overrides::load_reporting_rejects(&doc).1,
            InputAudioPerToken => curate::input_audio::load_reporting_rejects(&doc).1,
            OutputAudioPerToken => {
                curate::input_audio::load_for_schema(&doc, "output-audio-per-token/v1").1
            }
            LoginMethods => curate::login_methods::load_with(&doc, &models, &curated.models).1,
            ModelLineage => curate::model_lineage::load_reporting_rejects(&doc).1,
            SpeedTwins => curate::speed_twins::load_reporting_rejects(&doc).1,
            TemperatureRanges => curate::temperature_ranges::load_reporting_rejects(&doc).1,
            WireCorrections => {
                let (corrections, mut rejects) =
                    curate::wire_corrections::load_reporting_rejects(&doc);
                rejects.extend(
                    curate::wire_corrections::load_complete_providers_reporting_rejects(
                        &doc,
                        &corrections,
                    )
                    .1,
                );
                rejects
            }
            WireFamilies => curate::wire_families::load_reporting_rejects(&doc).1,
            PlanPrices => crate::plan_prices::load_reporting_rejects(&doc).1,
        };
        accepted(name, ((), rejects))?;
    }
    Ok(())
}

fn document_kind(name: &str) -> Result<VendorDocument, String> {
    VendorDocument::from_name(name)
        .ok_or_else(|| format!("unrecognized published vendor document {name:?}"))
}

fn accepted<T>(document: &str, (rows, rejects): (T, Vec<String>)) -> Result<T, String> {
    if rejects.is_empty() {
        Ok(rows)
    } else {
        Err(format!("{document}: {}", rejects.join("; ")))
    }
}

fn curated(value: &Value) -> Result<curate::curated_models::Table, String> {
    accepted(
        "curated-models",
        curate::curated_models::load_table_reporting_rejects(&value.to_string()),
    )
}

fn dedicated_table(document: VendorDocument) -> Option<&'static str> {
    match document {
        VendorDocument::CuratedModels => Some("curated_model_era"),
        VendorDocument::FactOverrides => Some("fact_override_era"),
        VendorDocument::PlanPrices => Some("plan_price_era"),
        VendorDocument::CacheLifetimes => Some("cache_lifetime_era"),
        _ => None,
    }
}

pub(crate) fn document_rows(
    out: &mut ImportedTables,
    old: Option<&Documents>,
    current: &Documents,
    at: i64,
) -> Result<(), String> {
    let names: BTreeSet<_> = old
        .into_iter()
        .flat_map(|doc| doc.keys())
        .chain(current.keys())
        .collect();
    for name in names {
        let previous = old.and_then(|docs| docs.get(name));
        let next = current.get(name);
        if previous == next {
            continue;
        }
        if let Some(table) = dedicated_table(document_kind(name)?) {
            // The imported base already contains the original dedicated eras.
            let previous_rows = changeable_claims(name, previous)?;
            let next_rows = changeable_claims(name, next)?;
            let keys: BTreeSet<_> = previous_rows.keys().chain(next_rows.keys()).collect();
            for key in keys {
                let previous = previous_rows.get(key);
                let next = next_rows.get(key);
                if previous == next {
                    continue;
                }
                let mut row = if let Some(next) = next {
                    next.clone()
                } else {
                    tombstone(table, previous.unwrap())
                };
                if old.is_none() && table != "plan_price_era" {
                    continue;
                }
                if table != "plan_price_era" || next.is_none() {
                    row["boundary_at_ms"] = json!(at);
                }
                if table == "cache_lifetime_era" {
                    row["recorded_at_ms"] = json!(at);
                }
                out.entry(table.to_owned())
                    .or_default()
                    .push(row.as_object().unwrap().clone());
            }
        } else {
            let row = json!({"document":name,"document_json":next.map(Value::to_string),"recorded_at_ms":at});
            out.entry("store_document_era".to_owned())
                .or_default()
                .push(row.as_object().unwrap().clone());
        }
    }
    Ok(())
}

type Claims = BTreeMap<(String, String, String), Value>;

fn changeable_claims(name: &str, doc: Option<&Value>) -> Result<Claims, String> {
    let Some(doc) = doc else {
        return Ok(BTreeMap::new());
    };
    let text = doc.to_string();
    let empty = String::new;
    let rows = match name {
        "curated-models" => {
            let table = curated(doc)?;
            table.models.iter().filter(|(key, _)| !table.held.contains_key(*key)).map(|((p, m), row)| {
                ((p.clone(), m.clone(), empty()), json!({"provider_id":p,"model_id":m,"facts_json":json!(row.facts).to_string(),
                    "auth_methods_json":json!(row.auth_methods).to_string(),"source_ref":row.source_ref,"established_by":row.established_by,
                    "established_at_ms":row.established_at_ms,"review_by_ms":row.review_by_ms}))
            }).collect()
        }
        "fact-overrides" => accepted(name, curate::fact_overrides::load_reporting_rejects(&text))?.into_iter().map(|((p, m, fact), row)| {
            let value = json!({"provider_id":p,"model_id":m,"fact_key":fact,"value_json":row.value.to_string(),
                "replaces_json":(!row.replaces.is_null()).then(|| row.replaces.to_string()),"reason":row.reason,
                "source_ref":row.source_ref,"quote":row.quote,"notes":row.notes,"established_by":row.established_by,
                "established_at_ms":row.established_at_ms,"review_by_ms":row.review_by_ms});
            ((p, m, fact), value)
        }).collect(),
        "cache-lifetimes" => accepted(name, curate::cache_lifetimes::load_reporting_rejects(&text))?.into_iter().map(|((p, m), row)| {
            let value = json!({"provider_id":p,"model_id":m,"rule_json":json!(row).to_string()});
            ((p,m,empty()), value)
        }).collect(),
        "plan-prices" => {
            let mut rows = BTreeMap::new();
            let prices = accepted(name, crate::plan_prices::load_reporting_rejects(&text))?;
            for (plan, row) in prices.iter().zip(crate::plan_prices::store_rows(&prices)) {
                let key = (row.provider_id.clone(), row.tier.clone(), empty());
                if rows.get(&key).is_some_and(|(at, _)| *at > row.boundary_at_ms) { continue; }
                let value = json!({"provider_id":row.provider_id,"tier":row.tier,"minor_units":row.minor_units,"exponent":row.exponent,
                    "currency":row.currency,"period":row.period,"boundary_kind":"asserted","boundary_at_ms":row.boundary_at_ms,"established_by":row.established_by,
                    "established_at_ms":row.established_at_ms,"review_by_ms":row.review_by_ms,"source_ref":row.source_ref,"refusal_reason":row.refusal_reason,
                    "quota_value_json":plan.quota_value.as_ref().map(|v| json!(v).to_string()),
                    "service_tiers_json":plan.service_tiers.as_ref().map(|v| json!(v).to_string()),
                    "concurrency_json":plan.concurrency.as_ref().map(|v| json!(v).to_string())});
                rows.insert(key, (row.boundary_at_ms, value));
            }
            rows.into_iter().map(|(k, (_, v))| (k, v)).collect()
        }
        _ => unreachable!("document has no dedicated era table"),
    };
    Ok(rows)
}

fn tombstone(table: &str, previous: &Value) -> Value {
    let mut row = previous.clone();
    let clear: &[&str] = match table {
        "fact_override_era" => &[
            "value_json",
            "replaces_json",
            "reason",
            "quote",
            "notes",
            "review_by_ms",
        ],
        "curated_model_era" => &["facts_json", "auth_methods_json", "review_by_ms"],
        "cache_lifetime_era" => &["rule_json"],
        "plan_price_era" => &[
            "minor_units",
            "exponent",
            "currency",
            "period",
            "quota_value_json",
            "service_tiers_json",
            "concurrency_json",
        ],
        _ => unreachable!("document has no dedicated era table"),
    };
    for &column in clear {
        row[column] = Value::Null;
    }
    if table == "plan_price_era" {
        row["refusal_reason"] = json!(fusiform_store::UNCURATED);
    }
    if matches!(table, "fact_override_era" | "curated_model_era") {
        row["tombstone_reason"] = json!("withdrawn by published curation");
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
    use fusiform_store::{CatalogStore, ReleaseArrival, StoreRelease};

    #[test]
    fn published_provider_vector_is_accepted_but_unknown_document_refused() {
        let document =
            serde_json::from_str(include_str!("../../data/curated-providers.json")).unwrap();
        let documents = BTreeMap::from([("curated-providers".into(), document)]);
        validate_published(&documents, &Upstream::new()).unwrap();

        let unknown = BTreeMap::from([("unknown-document".into(), json!({}))]);
        assert!(validate_published(&unknown, &Upstream::new())
            .unwrap_err()
            .contains("unrecognized published vendor document"));
    }

    #[test]
    fn every_published_document_is_applied_and_loaded_for_its_role() {
        use VendorDocument::*;

        let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
        let mut documents: Documents = VendorDocument::ALL
            .iter()
            .map(|&(_, name)| {
                let bytes = std::fs::read(data.join(format!("{name}.json"))).unwrap();
                (name.to_owned(), serde_json::from_slice(&bytes).unwrap())
            })
            .collect();
        // The committed canonical-link document is allowed to be empty. A
        // synthetic claim makes a missing load visible even in that case.
        let links = documents.get_mut("canonical-links").unwrap();
        links["links"].as_array_mut().unwrap().push(json!({
            "provider_id": "test-proxy", "model_id": "sample", "canonical_id": "test-lab/sample",
            "source_ref": "https://example.test/proxy", "quote": "Synthetic proxy listing",
            "lab_source_ref": "https://example.test/lab", "lab_quote": "Synthetic lab listing",
            "established_by": "fusiform", "established_at_ms": 1,
            "review_by_ms": 1 + 60 * 86_400_000_i64,
        }));
        let mut upstream = Upstream::new();
        for row in documents["login-methods"]["models"].as_array().unwrap() {
            upstream
                .entry(row["provider_id"].as_str().unwrap().into())
                .or_insert_with(|| fusiform_release::ProviderFacts {
                    provider: BTreeMap::new(),
                    models: BTreeMap::new(),
                })
                .models
                .insert(row["model_id"].as_str().unwrap().into(), BTreeMap::new());
        }
        validate_published(&documents, &upstream).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let store = CatalogStore::open_for_store(&StorageDescriptor {
            module_id: "fusiform".into(),
            storage_namespace: "default".into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.path().join("store.db").to_str().unwrap().into(),
            },
        })
        .unwrap();
        let at = 1_795_017_199_166;
        let mut rows = ImportedTables::new();
        // Model a curation change after bootstrap. On the first release the
        // dedicated tables (plan prices, curated rows, overrides) are already
        // filled from the store's history base file, whose hash the snapshot
        // attests, so this test passes no previous documents and only checks
        // what a later curation change writes.
        document_rows(&mut rows, Some(&Documents::new()), &documents, at).unwrap();
        store
            .apply_store_releases(
                Some(&ImportedTables::new()),
                &[ReleaseArrival {
                    published_at_ms: at,
                    release: StoreRelease {
                        release: 1,
                        state_hash: "a".repeat(64),
                        curation_hash: "b".repeat(64),
                        applied_at_ms: at,
                    },
                    rows,
                    upstream: None,
                }],
            )
            .unwrap();
        let persisted = store.store_documents_at(Some(at)).unwrap();
        let served = PublishedCuration::load(&store, Some(at)).unwrap().unwrap();
        for &(kind, name) in VendorDocument::ALL {
            if dedicated_table(kind).is_some() {
                assert!(
                    !persisted.contains_key(name),
                    "{name} belongs in its dedicated era"
                );
            } else {
                let raw: Value = serde_json::from_str(&persisted[name]).unwrap();
                assert_eq!(raw, documents[name], "{name} must round-trip intact");
            }
            let loaded = match kind {
                AttachmentLimits => !served.attachment_limits.is_empty(),
                BillingPlanes => !served.billing_planes.is_empty(),
                CacheLifetimes => !served.cache_lifetimes.is_empty(),
                CanonicalLinks => served
                    .canonical_links
                    .get(&("test-proxy".into(), "sample".into()))
                    .is_some_and(|link| link.canonical_id == "test-lab/sample"),
                CuratedModels => !served.curated_models.models.is_empty(),
                FactOverrides => !served.fact_overrides.is_empty(),
                InputAudioPerToken => !served.input_audio.is_empty(),
                LoginMethods => !served.login_methods.is_empty(),
                ModelAliases => !served.aliases.is_empty(),
                ModelCreators => !served.creators.is_empty(),
                ModelLineage => {
                    let (model, lineage) = documents[name]["models"]["entries"]
                        .as_object()
                        .unwrap()
                        .iter()
                        .next()
                        .unwrap();
                    served.model_lineage.lookup(model, None, None, false) == lineage.as_str()
                }
                PlanPrices => {
                    let body =
                        serde_json::to_vec(&json!({"name":"plan.prices","arguments":{"at_ms":at}}))
                            .unwrap();
                    let response = crate::route::serve_tool_call(&store, &body).unwrap();
                    let response = serde_json::to_value(response).unwrap();
                    !response["prices"].as_array().unwrap().is_empty()
                }
                SpeedTwins => !served.speed_twins.is_empty(),
                TemperatureRanges => !served.temperature_ranges.is_empty(),
                WireCorrections => !served.wire_corrections.is_empty(),
                WireFamilies => !served.wire_families.is_empty(),
                // Test-vector and upstream-ingest inputs, respectively. The
                // consumer retains the signed documents without serving them.
                CuratedProviders | OutputAudioPerToken => continue,
            };
            assert!(loaded, "{name} did not reach its serving input");
        }
    }

    #[test]
    fn plan_metadata_revision_keeps_price_origin_and_real_changes_break_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let make = |name: &str| StorageDescriptor {
            module_id: "fusiform".into(),
            storage_namespace: name.into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.path().join(name).to_string_lossy().into_owned(),
            },
        };
        let legacy = make("legacy.db");
        let plan_prices = crate::plan_prices::store_rows(crate::plan_prices::rows());
        let store = CatalogStore::open(&legacy).unwrap();
        assert!(store.append_plan_prices(&plan_prices).unwrap() > 0);
        drop(store);
        let store = CatalogStore::open(&legacy).unwrap();
        assert_eq!(store.append_plan_prices(&plan_prices).unwrap(), 0);
        assert!(store.store_documents_at(None).unwrap().is_empty());
        assert!(store.latest_store_release().unwrap().is_none());
        let db = rusqlite::Connection::open(store.path()).unwrap();
        let added: i64 = db.query_row("SELECT count(*) FROM plan_price_era WHERE quota_value_json IS NOT NULL OR service_tiers_json IS NOT NULL OR concurrency_json IS NOT NULL", [], |row| row.get(0)).unwrap();
        assert_eq!(added, 0);

        let fixture: Value =
            serde_json::from_str(include_str!("../../data/plan-prices.json")).unwrap();
        let mut raw = fixture["cells"]
            .as_array()
            .unwrap()
            .iter()
            .find(|cell| cell["quota_value"]["low"]["units"].is_u64())
            .unwrap()
            .clone();
        raw["boundary_at_ms"] = json!(1000);
        let provider = raw["provider_id"].as_str().unwrap();
        let tier = raw["tier"].as_str().unwrap();
        let low = raw["quota_value"]["low"]["units"].as_u64().unwrap();
        let original_amount = raw["price"]["minor_units"].as_i64().unwrap();
        let documents = |cell| BTreeMap::from([("plan-prices".into(), json!({"cells":[cell]}))]);
        let first = documents(raw.clone());
        let mut base_row = changeable_claims("plan-prices", first.get("plan-prices"))
            .unwrap()
            .into_values()
            .next()
            .unwrap();
        base_row["id"] = json!(1);
        base_row["boundary_at_ms"] = json!(1000);
        for key in ["quota_value_json", "concurrency_json", "service_tiers_json"] {
            base_row.as_object_mut().unwrap().remove(key);
        }
        let base = ImportedTables::from([(
            "plan_price_era".into(),
            vec![base_row.as_object().unwrap().clone()],
        )]);

        let mut raised = raw.clone();
        raised["price"]["minor_units"] = json!(original_amount + 1);
        raised["boundary_at_ms"] = json!(2500);
        let raised = documents(raised);
        let mut returned = raw.clone();
        returned["boundary_at_ms"] = json!(3500);
        let returned = documents(returned);
        let mut metadata = raw.clone();
        metadata["boundary_at_ms"] = json!(3500);
        metadata["quota_value"]["low"]["units"] = json!(low + 1);
        let metadata = documents(metadata);
        let consumer = CatalogStore::open_for_store(&make("consumer.db")).unwrap();
        let prepare = |n, old, current, at| {
            let mut rows = ImportedTables::new();
            document_rows(&mut rows, old, current, at).unwrap();
            assert!(!rows.contains_key("store_document_era"));
            ReleaseArrival {
                published_at_ms: at,
                release: StoreRelease {
                    release: n,
                    state_hash: "a".repeat(64),
                    curation_hash: "b".repeat(64),
                    applied_at_ms: at,
                },
                rows,
                upstream: None,
            }
        };
        let read = |store: &CatalogStore, at: Option<i64>| {
            let mut args = json!({"provider_id":provider});
            if let Some(at) = at {
                args["at_ms"] = json!(at);
            }
            let request = json!({"name":"plan.prices","arguments":args});
            let response =
                crate::route::serve_tool_call(store, &serde_json::to_vec(&request).unwrap())
                    .unwrap();
            let value = serde_json::to_value(response).unwrap();
            value["prices"]
                .as_array()
                .unwrap()
                .iter()
                .find(|cell| cell["tier"] == tier)
                .unwrap()
                .clone()
        };
        let legacy = read(&store, None);
        let asserted = crate::plan_prices::rows()
            .iter()
            .filter(|plan| plan.provider_id == provider && plan.tier == tier)
            .map(|p| p.boundary_at_ms)
            .max()
            .unwrap();
        assert_eq!(legacy["boundary_at_ms"], asserted);

        consumer
            .apply_store_releases(Some(&base), &[prepare(1, None, &first, 2000)])
            .unwrap();
        let prior = read(&consumer, Some(1500));
        assert!(prior.get("quota_value").is_none());
        assert_eq!(prior["boundary_at_ms"], 1000);
        let current = read(&consumer, None);
        assert_eq!(current["quota_value"]["low"]["units"], low);
        assert_eq!(current["boundary_at_ms"], 1000);

        consumer
            .apply_store_releases(
                None,
                &[
                    prepare(2, Some(&first), &raised, 3000),
                    prepare(3, Some(&raised), &returned, 4000),
                    prepare(4, Some(&returned), &metadata, 5000),
                ],
            )
            .unwrap();
        let raised = read(&consumer, Some(2600));
        assert_eq!(raised["price"]["minor_units"], original_amount + 1);
        assert_eq!(raised["boundary_at_ms"], 2500);
        assert_eq!(
            read(&consumer, Some(4500))["quota_value"]["low"]["units"],
            low
        );
        let current = read(&consumer, None);
        assert_eq!(current["price"]["minor_units"], original_amount);
        assert_eq!(current["quota_value"]["low"]["units"], low + 1);
        assert_eq!(current["boundary_at_ms"], 3500);
    }
}
