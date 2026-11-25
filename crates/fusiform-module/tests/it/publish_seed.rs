use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::SourceId;
use fusiform_module::fetch::FetchOutcome;
use fusiform_module::loop_::{apply, TickOutcome};
use fusiform_module::signals::Signals;
use fusiform_module::{input_audio, seed};
use fusiform_publish::{publish_step, FactIndex};
use fusiform_release::{fact_hash, upstream_hash_sorted, ChangeOp, ProviderFacts, Upstream};
use fusiform_store::CatalogStore;
use serde_json::{json, Value};

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
    let published = publish_step(SEED, &previous, &input_audio::evidence()).unwrap();
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

    let mut changed: Value = serde_json::from_slice(SEED).unwrap();
    let models = changed[PROVIDER]["models"].as_object_mut().unwrap();
    let edited = json!("A changed display name");
    assert_ne!(models[MODEL]["name"], edited);
    models.get_mut(MODEL).unwrap()["name"] = edited.clone();
    let next = publish_step(&serde_json::to_vec(&changed).unwrap(), &previous, &evidence).unwrap();
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

    let models = changed[PROVIDER]["models"].as_object_mut().unwrap();
    assert!(models.remove(MODEL).is_some());
    let removed =
        publish_step(&serde_json::to_vec(&changed).unwrap(), &previous, &evidence).unwrap();
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

const REPEATED_TIERS: &[u8] = br#"{
  "p": {"name":"Provider", "models": {
    "m": {"name":"Tiered model", "cost":{"tiers":[
      {"tier":{"type":"context","size":200000},"input":1,"output":3},
      {"tier":{"type":"context","size":200000},"input":2,"output":3}
    ]}},
    "n": {"name":"Other model"}
  }}
}"#;

#[test]
fn conflicting_repeated_tier_prices_are_omitted_by_both_paths() {
    let (_dir, store) = blank_store();
    let signals = Signals::new();
    let poll = |at| {
        apply(
            &store,
            &signals,
            SourceId::ModelsDev,
            FetchOutcome::Body {
                bytes: REPEATED_TIERS.to_vec(),
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
    let input = "rate.input.above_context.200000";
    let output = "rate.output.above_context.200000";
    assert!(!facts.keys().any(|key| key.as_str() == input));
    assert!(facts.keys().any(|key| key.as_str() == output));
    assert_eq!(held.len(), 2);
    let store_hash = hash_held(&held);

    let blank = FactIndex::from_snapshot_upstream(&Upstream::new()).unwrap();
    let published = publish_step(REPEATED_TIERS, &blank, &input_audio::evidence()).unwrap();
    let [conflict] = published.conflicts.as_slice() else {
        panic!("expected only the input conflict")
    };
    assert_eq!(
        (
            conflict.provider.as_str(),
            conflict.model.as_str(),
            conflict.fact.as_str()
        ),
        ("p", "m", input)
    );
    let amounts: Vec<_> = conflict
        .values
        .iter()
        .map(|raw| serde_json::from_str::<Value>(raw).unwrap()["units"].clone())
        .collect();
    assert_eq!(amounts, [json!(1_000_000_000), json!(2_000_000_000)]);
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
    let repeated =
        publish_step(REPEATED_TIERS, &published.index, &input_audio::evidence()).unwrap();
    assert!(repeated.ops.is_empty());
    assert_eq!(repeated.conflicts, published.conflicts);

    let repeated_poll = poll(2);
    assert!(matches!(repeated_poll.outcome, TickOutcome::Unchanged));
    assert_eq!(repeated_poll.eras_written, 0);
}
