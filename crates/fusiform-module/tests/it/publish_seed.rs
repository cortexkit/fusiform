use std::collections::BTreeMap;

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{normalize_models_dev_with, SourceId, Timestamp};
use fusiform_module::fetch::FetchOutcome;
use fusiform_module::loop_::{apply, TickOutcome};
use fusiform_module::signals::Signals;
use fusiform_module::{input_audio, seed};
use fusiform_publish::{publish_step, FactIndex};
use fusiform_release::{fact_hash, upstream_hash_sorted, ChangeOp, ProviderFacts, Upstream};
use fusiform_store::CatalogStore;
use serde_json::{
    json,
    value::{to_raw_value, RawValue},
    Value,
};

const SEED: &[u8] = include_bytes!("../../data/models-dev-seed.json");
const PROVIDER: &str = "openai";
const MODEL: &str = "gpt-6.1-sol";

#[test]
fn the_publisher_hashes_exactly_what_a_seeded_store_holds() {
    let (_dir, store) = blank_store();
    seed::seed_if_empty(&store).unwrap();

    let held = store.current_values(SourceId::ModelsDev).unwrap();
    let held_hash = hash_held(&held);
    let model_count = held.len();
    let fact_count = held.values().map(|facts| facts.len()).sum::<usize>();
    let upstream = held_as_upstream(held);
    let previous = FactIndex::from_snapshot_upstream(&upstream).unwrap();
    assert_eq!(previous.upstream_hash(), held_hash);
    let published = publish_step(
        SEED,
        &previous,
        &input_audio::evidence(),
        seed::meta().unwrap().fetched_at_ms,
    )
    .unwrap();
    assert_eq!(published.upstream_hash, held_hash);
    assert_eq!(published.model_count, model_count);
    assert_eq!(published.index.model_count(), model_count);
    assert_eq!(published.index.fact_count(), fact_count);
    assert!(published.ops.is_empty());
}

fn blank_store() -> (tempfile::TempDir, CatalogStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_str().unwrap().into(),
        },
    })
    .unwrap();
    (dir, store)
}

fn hash_held(held: &fusiform_store::ingest::CurrentValues) -> fusiform_release::Hash {
    upstream_hash_sorted(held.iter().flat_map(|((provider, model), facts)| {
        facts.iter().map(move |(key, raw)| {
            let value = serde_json::from_str(raw).unwrap();
            (
                (provider.as_str(), Some(model.as_str()), key.as_str()),
                fact_hash(&value),
            )
        })
    }))
}

fn held_as_upstream(held: fusiform_store::ingest::CurrentValues) -> Upstream {
    let mut upstream = Upstream::new();
    for ((provider, model), facts) in held {
        upstream
            .entry(provider)
            .or_insert_with(|| ProviderFacts {
                provider: Default::default(),
                models: Default::default(),
            })
            .models
            .insert(
                model,
                facts
                    .into_iter()
                    .map(|(key, raw)| (key.as_str().into(), serde_json::from_str(&raw).unwrap()))
                    .collect(),
            );
    }
    upstream
}

#[test]
fn one_seed_edit_sets_one_fact_and_a_removed_model_loses_all_its_facts() {
    let evidence = input_audio::evidence();
    let observed_at = seed::meta().unwrap().fetched_at_ms;
    let upstream: Upstream = seed::upstream_state()
        .unwrap()
        .models
        .into_iter()
        .map(|(provider, models)| {
            let models = models
                .into_iter()
                .map(|(model, facts)| {
                    (
                        model,
                        facts
                            .into_iter()
                            .map(|(key, raw)| (key, serde_json::from_str(&raw).unwrap()))
                            .collect(),
                    )
                })
                .collect();
            (
                provider,
                ProviderFacts {
                    provider: Default::default(),
                    models,
                },
            )
        })
        .collect();
    let prior = FactIndex::from_snapshot_upstream(&upstream).unwrap();
    let previous = FactIndex::from_bytes(&prior.to_bytes()).unwrap();

    // V2 uses the first API and first cache-write TTL as defaults, so decoding
    // the whole catalog into Value could change facts unrelated to the edit.
    type RawObject = BTreeMap<String, Box<RawValue>>;
    let mut changed: RawObject = serde_json::from_slice(SEED).unwrap();
    let mut provider: RawObject = serde_json::from_str(changed[PROVIDER].get()).unwrap();
    let mut models: RawObject = serde_json::from_str(provider["models"].get()).unwrap();
    let mut model: RawObject = serde_json::from_str(models[MODEL].get()).unwrap();
    let edited = json!("A changed display name");
    assert_ne!(
        serde_json::from_str::<Value>(model["name"].get()).unwrap(),
        edited
    );
    model.insert("name".into(), to_raw_value(&edited).unwrap());
    models.insert(MODEL.into(), to_raw_value(&model).unwrap());
    let mut changed_document = |models: &RawObject| {
        provider.insert("models".into(), to_raw_value(models).unwrap());
        changed.insert(PROVIDER.into(), to_raw_value(&provider).unwrap());
        serde_json::to_vec(&changed).unwrap()
    };
    let next = publish_step(
        &changed_document(&models),
        &previous,
        &evidence,
        observed_at,
    )
    .unwrap();
    assert_eq!(next.model_count, previous.model_count());
    assert_ne!(next.upstream_hash, previous.upstream_hash());
    assert_eq!(
        next.ops,
        [ChangeOp::Set {
            provider: PROVIDER.to_owned(),
            model: MODEL.to_owned(),
            fact: "model.name".to_owned(),
            value: edited,
        }]
    );

    assert!(models.remove(MODEL).is_some());
    let removed = publish_step(
        &changed_document(&models),
        &previous,
        &evidence,
        observed_at,
    )
    .unwrap();
    let expected: Vec<_> = upstream[PROVIDER].models[MODEL]
        .keys()
        .map(|fact| ChangeOp::Remove {
            provider: PROVIDER.to_owned(),
            model: MODEL.to_owned(),
            fact: fact.clone(),
        })
        .collect();
    assert!(expected.iter().any(|op| op.key().2 == "existence"));
    assert!(expected.iter().any(|op| op.key().2 == "provider.name"));
    assert_eq!(removed.ops, expected);
    assert_eq!(removed.model_count, previous.model_count() - 1);
}

const CONFLICTING_MODE: &[u8] = br#"{
  "p": {"name":"Provider", "models": {
    "m": {"name":"Tiered model", "type":"chat", "capabilities":{}, "api":{},
      "experimental":{"modes":{"fast":{"service_tier":"priority",
        "cost":{"text":{"input":{"price":1}}}}}},
      "cost":{"text":{
        "input":{"overrides":[{"when":{"service_tier":{"equals":"priority"}},"price":2}]},
        "output":{"overrides":[{"when":{"context":{"gt":200000}},"price":3}]}
      }}},
    "n": {"name":"Other model", "type":"chat", "capabilities":{}, "api":{}}
  }}
}"#;

#[test]
fn conflicting_named_mode_and_service_tier_prices_are_omitted_by_both_paths() {
    let (_dir, store) = blank_store();
    let signals = Signals::new();
    let poll = |at| {
        apply(
            &store,
            &signals,
            SourceId::ModelsDev,
            FetchOutcome::Body {
                bytes: CONFLICTING_MODE.to_vec(),
                etag: None,
                duration: std::time::Duration::ZERO,
            },
            at,
        )
        .unwrap()
    };
    assert!(poll(1).eras_written > 0, "ordinary facts must still land");

    let held = store.current_values(SourceId::ModelsDev).unwrap();
    let facts = &held[&("p".into(), "m".into())];
    let input = "rate.input.mode.fast";
    let output = "rate.output.above_context.200000";
    assert!(!facts.keys().any(|key| key.as_str() == input));
    let (_, raw) = facts
        .iter()
        .find(|(key, _)| key.as_str() == output)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(raw).unwrap()["units"],
        3_000_000_000u64
    );
    assert_eq!(held.len(), 2);
    let store_hash = hash_held(&held);

    let blank = FactIndex::from_snapshot_upstream(&Upstream::new()).unwrap();
    let evidence = input_audio::evidence();
    let normalized = normalize_models_dev_with(CONFLICTING_MODE, &evidence, Timestamp(1)).unwrap();
    let [finding] = normalized.findings.as_slice() else {
        panic!("expected only the mode conflict: {:?}", normalized.findings)
    };
    assert_eq!(finding.first_model_id.as_deref(), Some("m"));
    assert!(matches!(&finding.key,
        fusiform_core::normalize::v2::FindingKey::RefusedPrice { role, detail }
        if role == "text.input.price" && detail == "conflicting prices for mode fast"));

    let source: Value = serde_json::from_slice(CONFLICTING_MODE).unwrap();
    for (remove_tier, expected) in [(true, 1_000_000_000), (false, 2_000_000_000)] {
        let mut candidate = source.clone();
        let row = &mut candidate["p"]["models"]["m"];
        if remove_tier {
            row["cost"]["text"].as_object_mut().unwrap().remove("input");
        } else {
            row["experimental"]["modes"]["fast"]
                .as_object_mut()
                .unwrap()
                .remove("cost");
        }
        let only = publish_step(
            &serde_json::to_vec(&candidate).unwrap(),
            &blank,
            &evidence,
            1,
        )
        .unwrap();
        assert!(
            only.ops.iter().any(|op| matches!(op,
            ChangeOp::Set { provider, model, fact, value }
                if provider == "p" && model == "m" && fact == input && value["units"] == expected))
        );
    }
    let published = publish_step(CONFLICTING_MODE, &blank, &evidence, 1).unwrap();
    assert_eq!(published.upstream_hash, store_hash);
    assert!(published.ops.iter().all(|op| op.key().2 != input));
    assert_eq!(
        published
            .ops
            .iter()
            .filter(|op| op.key().2 == output)
            .count(),
        1
    );
    let repeated = publish_step(CONFLICTING_MODE, &published.index, &evidence, 2).unwrap();
    assert!(repeated.ops.is_empty());
    assert_eq!(repeated.conflicts, published.conflicts);

    let repeated_poll = poll(2);
    assert!(matches!(repeated_poll.outcome, TickOutcome::Unchanged));
    assert_eq!(repeated_poll.eras_written, 0);
}

#[test]
fn matching_context_overrides_merge_in_order() {
    // The models.dev v2 schema says, “Matching overrides merge field by field
    // (nested objects included) in order”. The later price wins without
    // clearing the earlier cache read price.
    let body = br#"{"p":{"models":{"m":{"type":"chat","api":{},"capabilities":{},
      "cost":{"text":{"input":{"overrides":[
        {"when":{"context":{"gt":200000}},"price":1,"cache":{"read":0.5}},
        {"when":{"context":{"gt":200000}},"price":2}
      ]}}}}}}}"#;
    let blank = FactIndex::from_snapshot_upstream(&Upstream::new()).unwrap();
    let published = publish_step(body, &blank, &input_audio::evidence(), 1).unwrap();
    assert!(published.conflicts.is_empty());
    for (fact, units) in [
        ("rate.input.above_context.200000", 2_000_000_000),
        ("rate.cache_read.above_context.200000", 500_000_000),
    ] {
        let matching: Vec<_> = published
            .ops
            .iter()
            .filter(|op| op.key().2 == fact)
            .collect();
        assert_eq!(matching.len(), 1, "{fact}");
        assert!(matches!(matching[0], ChangeOp::Set { value, .. } if value["units"] == units));
    }
}
