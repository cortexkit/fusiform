use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    num::NonZeroU64,
    sync::{Arc, Mutex},
};

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_release::{canonical_json, sign_envelope, sign_head, SigningKey};
use serde_json::{json, Value};

use super::*;

const FIRST: &[u8] =
    include_bytes!("../../../fusiform-release/fixtures/release-format-v1/chain/release-1.json");
const BASE: &[u8] =
    include_bytes!("../../../fusiform-release/fixtures/release-format-v1/chain/base.json");
const DAY: &[u8] = include_bytes!(
    "../../../fusiform-release/fixtures/release-format-v1/chain/days/2026-11-19.json"
);
const SNAPSHOT: &[u8] =
    include_bytes!("../../../fusiform-release/fixtures/release-format-v1/chain/snapshot.json");
const KID: &str = "release-format-v1-test-key";

fn server(files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut line = String::new();
            let mut reader = BufReader::new(&mut stream);
            if reader.read_line(&mut line).ok() == Some(0) {
                break;
            }
            let route = line.split_ascii_whitespace().nth(1).unwrap_or("");
            let body = files.lock().unwrap().get(route).cloned();
            while {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap_or(0) > 0 && header != "\r\n"
            } {}
            let (status, bytes) =
                body.map_or(("404 Not Found", Vec::new()), |body| ("200 OK", body));
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            )
            .unwrap();
            stream.write_all(&bytes).unwrap();
        }
    });
    url
}

fn signed_head(snapshot: &Snapshot, latest_snapshot: u64, key: &SigningKey) -> Vec<u8> {
    let head = HeadBody {
        release: NonZeroU64::new(snapshot.envelope.release).unwrap(),
        latest_snapshot,
        state_hash: snapshot.envelope.state_hash,
        published_at_ms: snapshot.envelope.published_at_ms,
    };
    let signatures = sign_head(&head, KID, key).unwrap();
    canonical_json(&serde_json::to_value(Head { head, signatures }).unwrap())
}

fn model_name_at(store: &CatalogStore, at_ms: i64) -> Value {
    let request = json!({"provider_id":"zeta","model_id":"mini","fact_prefixes":["model.name"],"at_ms":at_ms});
    crate::route::serve_catalog_get(store, &serde_json::to_vec(&request).unwrap())
        .unwrap()
        .models["zeta/mini"]["model.name"]
        .clone()
}

#[test]
fn store_poll_writes_the_same_model_eras_as_direct_ingest() {
    use fusiform_core::{
        normalize_models_dev, BoundaryKind, ChargeUnitEvidence, ObservationOutcome, SourceId,
        Timestamp,
    };
    use fusiform_publish::{publish_step, FactIndex};
    use fusiform_release::{state_hash, Change};
    use fusiform_store::{ingest::plan_ingest, ImportedTables, NewObservation};

    let dir = tempfile::tempdir().unwrap();
    let poll_path = dir.path().join("poll.db");
    let store_path = dir.path().join("store.db");
    let descriptor = |path: &std::path::Path, namespace: &str| StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: namespace.into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_string_lossy().into_owned(),
        },
    };
    let direct = CatalogStore::open(&descriptor(&poll_path, "poll")).unwrap();
    let published = CatalogStore::open_for_store(&descriptor(&store_path, "store")).unwrap();
    let curation: fusiform_release::CurationSet = serde_json::from_value(json!({
        "version": 1, "approved_at_ms": null, "approved_drafts": [], "grades": {},
        "effort_grades": {}, "kind_grades": {}, "workload_efforts": {},
        "decommissioned": {}, "vendor_facts": {},
    }))
    .unwrap();
    let key = SigningKey::from_bytes(&[42; 32]);
    let keys = BTreeMap::from([(KID.into(), key.verifying_key())]);
    let mut index = FactIndex::from_snapshot_upstream(&Default::default()).unwrap();
    let mut previous: Option<Verified> = None;
    let mut last_observation = 1;

    let mut document = json!({
        "p": {"name": "P", "models": {
            "m": {"name": "Model", "cost": {"input": 1, "output": 2}, "limit": {"context": 100}},
            "other": {"name": "Other"},
        }},
        "q": {"name": "Q", "models": {"untouched": {"name": "Untouched"}}},
    });
    let rows_at = |path: &std::path::Path, at| {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT provider_id, model_id, fact_key, value_json, boundary_kind
            FROM era WHERE source = 'models.dev' AND boundary_at_ms = ?1
            ORDER BY provider_id, model_id, fact_key",
            )
            .unwrap();
        stmt.query_map([at], |row| {
            let value: String = row.get(3)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                serde_json::from_str::<Value>(&value).unwrap(),
                row.get::<_, String>(4)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    };
    let mut held = Value::Null;
    let mut arrival_checks = Vec::new();
    for (i, (label, expected)) in [
        ("seed", vec![]),
        ("fact change", vec![("m", "rate.input")]),
        ("fact removal", vec![("m", "limit.context")]),
        ("whole-model withdrawal", vec![("m", "existence")]),
        ("unchanged reappearance", vec![("m", "existence")]),
        ("second withdrawal", vec![("m", "existence")]),
        (
            "changed reappearance",
            vec![
                ("m", "existence"),
                ("m", "rate.input"),
                ("m", "rate.output"),
            ],
        ),
        (
            "provider removal",
            vec![("m", "existence"), ("other", "existence")],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        match i {
            0 => {}
            1 => document["p"]["models"]["m"]["cost"]["input"] = json!(3),
            2 => {
                document["p"]["models"]["m"]
                    .as_object_mut()
                    .unwrap()
                    .remove("limit");
            }
            3 | 5 => {
                held = document["p"]["models"]
                    .as_object_mut()
                    .unwrap()
                    .remove("m")
                    .unwrap()
            }
            4 => document["p"]["models"]["m"] = held.clone(),
            6 => {
                document["p"]["models"]["m"] = held.clone();
                document["p"]["models"]["m"]["cost"] = json!({"input": 4});
            }
            7 => {
                document.as_object_mut().unwrap().remove("p");
            }
            _ => unreachable!(),
        }
        let at = 1000 * (i as i64 + 1);
        let body = serde_json::to_vec(&document).unwrap();
        let step = publish_step(&body, &index, &ChargeUnitEvidence::default()).unwrap();
        index = step.index;
        let change = Change {
            upstream: step.ops,
            curation_set: (i == 0).then(|| curation.clone()),
        };
        let curation_hash = curation.curation_hash();
        let envelope = ReleaseEnvelope {
            format: fusiform_release::ReleaseFormat::V1,
            release: i as u64 + 1,
            parent: previous.as_ref().map(|previous| previous.envelope.release),
            published_at_ms: at,
            upstream_observed_at_ms: at,
            upstream_hash: step.upstream_hash,
            curation_version: curation.version,
            curation_hash,
            state_hash: state_hash(curation_hash, step.upstream_hash),
            change_hash: change.change_hash(),
        };
        let release = Release {
            signatures: sign_envelope(&envelope, KID, &key).unwrap(),
            envelope,
            change,
        };
        let next = if let Some(previous) = &previous {
            Verified::advance(previous, &release, &keys).unwrap()
        } else {
            Verified::first(release.clone(), &keys).unwrap()
        };
        let arrival = translate::arrival(
            previous.as_ref(),
            &next,
            &release.change.upstream,
            i == 0,
            &mut last_observation,
        )
        .unwrap();
        previous = Some(next);

        let observation = direct
            .record_observation(&NewObservation {
                source: SourceId::ModelsDev,
                observed_at: Timestamp(at),
                outcome: if i == 0 {
                    ObservationOutcome::Seeded
                } else {
                    ObservationOutcome::Changed { snapshot_seq: 1 }
                },
                normalized_hash: None,
                raw_hash: None,
                etag: None,
                duration_ms: None,
                detail: None,
            })
            .unwrap();
        let catalog = normalize_models_dev(&body).unwrap().catalog;
        let plan = plan_ingest(
            &direct,
            &catalog,
            Timestamp(at),
            if i == 0 {
                BoundaryKind::Seed
            } else {
                BoundaryKind::Observed
            },
            (i != 0).then_some(observation),
        )
        .unwrap();
        if i == 0 {
            direct.append_eras(&plan.eras).unwrap();
            let seed_rows = plan.eras.iter().enumerate().map(|(n, era)| {
                json!({"id": n+1, "source": "models.dev", "provider_id": era.provider_id,
                    "model_id": era.model_id, "fact_key": era.fact_key.as_str(), "value_json": era.value_json,
                    "boundary_at_ms": at, "boundary_kind": "seed"}).as_object().unwrap().clone()
            }).collect();
            let seed_observation = json!({"id": observation, "source": "models.dev", "observed_at_ms": at, "outcome": "seeded"});
            let base = ImportedTables::from([
                (
                    "observation".into(),
                    vec![seed_observation.as_object().unwrap().clone()],
                ),
                ("era".into(), seed_rows),
            ]);
            published
                .apply_store_releases(Some(&base), &[arrival])
                .unwrap();
        } else {
            direct
                .append_poll_eras(&plan.eras, observation, Timestamp(at))
                .unwrap();
            arrival_checks.push((label, at, expected, arrival));
        }
    }

    // Apply several verified observations in one transaction, as on catch-up.
    let (checks, arrivals): (Vec<_>, Vec<_>) = arrival_checks
        .into_iter()
        .map(|(label, at, expected, arrival)| ((label, at, expected), arrival))
        .unzip();
    published.apply_store_releases(None, &arrivals).unwrap();
    for (label, at, expected) in checks {
        let poll_rows = rows_at(&poll_path, at);
        assert_eq!(
            poll_rows
                .iter()
                .map(|r| (r.1.as_str(), r.2.as_str()))
                .collect::<Vec<_>>(),
            expected,
            "poll fixture: {label}"
        );
        assert_eq!(rows_at(&store_path, at), poll_rows, "{label}");
    }
}

#[tokio::test]
async fn store_poll_verifies_attested_history_and_keeps_last_good_release() {
    let key = SigningKey::from_bytes(&[42; 32]);
    let mut snapshot: Snapshot = serde_json::from_slice(SNAPSHOT).unwrap();
    let wrong_base = [b" ".as_slice(), BASE].concat();
    let files = Arc::new(Mutex::new(BTreeMap::from([
        ("/v1/head".into(), signed_head(&snapshot, 2, &key)),
        ("/v1/snapshots/2".into(), SNAPSHOT.to_vec()),
        ("/v1/releases/1".into(), FIRST.to_vec()),
        ("/v1/history/base.json".into(), wrong_base),
        ("/v1/history/days/2026-11-19.json".into(), DAY.to_vec()),
    ])));
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open_for_store(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into_owned(),
        },
    })
    .unwrap();
    let mut poller = StorePoller::new(
        server(Arc::clone(&files)),
        BTreeMap::from([(KID.into(), key.verifying_key())]),
    )
    .unwrap();
    let mut unattested = snapshot.clone();
    unattested.history_attestation = None;
    files.lock().unwrap().insert(
        "/v1/snapshots/2".into(),
        serde_json::to_vec(&unattested).unwrap(),
    );
    assert!(poller
        .poll(&store)
        .await
        .unwrap_err()
        .contains("missing history_attestation"));
    files
        .lock()
        .unwrap()
        .insert("/v1/snapshots/2".into(), SNAPSHOT.to_vec());
    let mut invalid_anchor: Release = serde_json::from_slice(FIRST).unwrap();
    invalid_anchor.envelope.published_at_ms -= 1;
    files.lock().unwrap().insert(
        "/v1/releases/1".into(),
        serde_json::to_vec(&invalid_anchor).unwrap(),
    );
    assert!(poller
        .poll(&store)
        .await
        .unwrap_err()
        .contains("verify release 1"));
    assert!(store.latest_store_release().unwrap().is_none());
    files
        .lock()
        .unwrap()
        .insert("/v1/releases/1".into(), FIRST.to_vec());
    assert!(poller
        .poll(&store)
        .await
        .unwrap_err()
        .contains("history/base.json"));
    assert_eq!(store.catalog_version().unwrap(), 0);
    assert!(store.latest_store_release().unwrap().is_none());
    files
        .lock()
        .unwrap()
        .insert("/v1/history/base.json".into(), BASE.to_vec());
    assert_eq!(poller.poll(&store).await.unwrap(), 2);
    assert_eq!(store.latest_store_release().unwrap().unwrap().release, 2);
    let at_release_two = snapshot.envelope.upstream_observed_at_ms;
    assert_eq!(model_name_at(&store, at_release_two), "Mini 2");

    let mut curation_set = snapshot.state.curation_set.clone();
    curation_set.version += 1;
    curation_set.workload_efforts.insert(
        "zeta/mini".into(),
        BTreeMap::from([(
            "code".into(),
            fusiform_release::WorkloadEffort {
                default: None,
                minimum: None,
                excluded: true,
            },
        )]),
    );
    snapshot.state.curation_set = curation_set.clone();
    let change = fusiform_release::Change {
        upstream: vec![fusiform_release::ChangeOp::Set {
            provider: "zeta".into(),
            model: "mini".into(),
            fact: "model.name".into(),
            value: json!("Mini 3"),
        }],
        curation_set: Some(curation_set),
    };
    snapshot
        .state
        .upstream
        .get_mut("zeta")
        .unwrap()
        .models
        .get_mut("mini")
        .unwrap()
        .insert("model.name".into(), json!("Mini 3"));
    let envelope = &mut snapshot.envelope;
    envelope.parent = Some(2);
    envelope.release = 3;
    envelope.published_at_ms += 20_000;
    envelope.upstream_observed_at_ms += 20_000;
    envelope.upstream_hash = fusiform_release::upstream_state_hash(&snapshot.state.upstream);
    envelope.curation_version = snapshot.state.curation_set.version;
    envelope.curation_hash = snapshot.state.curation_set.curation_hash();
    envelope.state_hash =
        fusiform_release::state_hash(envelope.curation_hash, envelope.upstream_hash);
    envelope.change_hash = change.change_hash();
    let third = Release {
        envelope: envelope.clone(),
        change,
        signatures: sign_envelope(envelope, KID, &key).unwrap(),
    };
    files
        .lock()
        .unwrap()
        .insert("/v1/head".into(), signed_head(&snapshot, 2, &key));
    let version = store.catalog_version().unwrap();
    let error = poller.poll(&store).await.unwrap_err();
    assert!(
        error.contains("v1/releases/3") && error.contains("404"),
        "{error}"
    );
    assert_eq!(store.catalog_version().unwrap(), version);
    assert_eq!(store.latest_store_release().unwrap().unwrap().release, 2);
    files
        .lock()
        .unwrap()
        .insert("/v1/releases/3".into(), serde_json::to_vec(&third).unwrap());
    assert_eq!(poller.poll(&store).await.unwrap(), 3);
    let version = store.catalog_version().unwrap();
    assert_eq!(
        model_name_at(&store, third.envelope.upstream_observed_at_ms),
        "Mini 3"
    );
    assert_eq!(model_name_at(&store, at_release_two), "Mini 2");
    let get = |at_ms| {
        let args = json!({"provider_id":"zeta","model_id":"mini","fact_prefixes":["workload.","existence"],"at_ms":at_ms});
        crate::route::serve_catalog_get(&store, &serde_json::to_vec(&args).unwrap()).unwrap()
    };
    let published_at = third.envelope.published_at_ms;
    let before = get(published_at - 1);
    assert!(!before.models["zeta/mini"].contains_key("workload.code.excluded"));
    assert!(before.quality_scores.is_none());
    let published = get(published_at);
    assert_eq!(
        published.models["zeta/mini"]["workload.code.excluded"],
        true
    );
    assert_eq!(published.quality_scores.unwrap().graded_at_ms, published_at);

    let mut head: Value = serde_json::from_slice(&signed_head(&snapshot, 2, &key)).unwrap();
    head["head"]["published_at_ms"] = json!(0);
    files
        .lock()
        .unwrap()
        .insert("/v1/head".into(), serde_json::to_vec(&head).unwrap());
    assert!(poller.poll(&store).await.unwrap_err().contains("signature"));
    assert_eq!(store.catalog_version().unwrap(), version);
    assert_eq!(store.latest_store_release().unwrap().unwrap().release, 3);

    let mut row = fusiform_store::WorkloadEffortEra {
        model: "zeta/mini".into(),
        workload: "quick".into(),
        effort_default: None,
        effort_minimum: None,
        excluded: true,
        reason: "local evaluation".into(),
        recorded_at_ms: published_at + 1,
    };
    store.append_workload_effort(&row, "user").unwrap();
    let local = get(row.recorded_at_ms);
    assert_eq!(local.models["zeta/mini"]["workload.quick.excluded"], true);
    assert!(local
        .overrides
        .iter()
        .any(|row| row.fact == "workload.quick.excluded"
            && row.published.is_none()
            && row.local == Some(json!(true))));
    row.excluded = false;
    row.recorded_at_ms += 1;
    store.append_workload_effort(&row, "user").unwrap();
    let after = get(row.recorded_at_ms);
    assert!(!after.models["zeta/mini"].contains_key("workload.quick.excluded"));
    assert_eq!(after.models["zeta/mini"]["workload.code.excluded"], true);
}
