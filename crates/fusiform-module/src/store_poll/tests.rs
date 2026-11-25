use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    num::NonZeroU64,
    sync::{Arc, Mutex},
};

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_release::{canonical_json, sha256, sign_envelope, sign_head, SigningKey};
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

fn poll_store(descriptor: &StorageDescriptor) -> Arc<CatalogStore> {
    Arc::new(CatalogStore::open_for_store(descriptor).unwrap())
}

fn server(files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>) -> Url {
    server_with_requests(files, Arc::default())
}

fn server_with_requests(
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    requests: Arc<Mutex<Vec<String>>>,
) -> Url {
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
            requests.lock().unwrap().push(route.into());
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

fn next_named_release(snapshot: &mut Snapshot, key: &SigningKey) -> Release {
    let envelope = &mut snapshot.envelope;
    envelope.parent = Some(envelope.release);
    envelope.release += 1;
    envelope.published_at_ms += 120_000;
    envelope.upstream_observed_at_ms += 120_000;
    let value = json!(format!("Mini {}", envelope.release % 10));
    snapshot
        .state
        .upstream
        .get_mut("zeta")
        .unwrap()
        .models
        .get_mut("mini")
        .unwrap()
        .insert("model.name".into(), value.clone());
    let change = fusiform_release::Change {
        upstream: vec![fusiform_release::ChangeOp::Set {
            provider: "zeta".into(),
            model: "mini".into(),
            fact: "model.name".into(),
            value,
        }],
        curation_set: None,
    };
    envelope.upstream_hash = fusiform_release::upstream_state_hash(&snapshot.state.upstream);
    envelope.state_hash =
        fusiform_release::state_hash(envelope.curation_hash, envelope.upstream_hash);
    envelope.change_hash = change.change_hash();
    Release {
        envelope: envelope.clone(),
        signatures: sign_envelope(envelope, KID, key).unwrap(),
        change,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn store_poll_replay_leaves_the_async_worker_responsive() {
    let key = SigningKey::from_bytes(&[42; 32]);
    let snapshot: Snapshot = serde_json::from_slice(SNAPSHOT).unwrap();
    let files = Arc::new(Mutex::new(BTreeMap::from([
        ("/v1/head".into(), signed_head(&snapshot, 2, &key)),
        ("/v1/snapshots/2".into(), SNAPSHOT.to_vec()),
        ("/v1/releases/1".into(), FIRST.to_vec()),
        ("/v1/history/base.json".into(), BASE.to_vec()),
        ("/v1/history/days/2026-11-19.json".into(), DAY.to_vec()),
    ])));
    let dir = tempfile::tempdir().unwrap();
    let store = poll_store(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into_owned(),
        },
    });
    let mut poller = StorePoller::new(
        server(files),
        BTreeMap::from([(KID.into(), key.verifying_key())]),
    )
    .unwrap();
    let (started, replaying) = tokio::sync::oneshot::channel();
    let (reply, replied) = std::sync::mpsc::channel();
    let probe = Mutex::new(Some((started, replied)));
    let request = tokio::spawn(async move {
        replaying.await.unwrap();
        reply.send(()).unwrap();
    });

    assert_eq!(
        poller
            .poll_with_progress(&store, move || {
                // Hold the first replay stage until another task can answer. A replay
                // on the sole async worker prevents the reply until the timeout.
                if let Some((started, replied)) = probe.lock().unwrap().take() {
                    started.send(()).unwrap();
                    assert_eq!(
                        replied.recv_timeout(Duration::from_secs(2)),
                        Ok(()),
                        "the async worker could not answer while replay was in progress"
                    );
                }
            })
            .await
            .unwrap(),
        2
    );
    request.await.unwrap();
}

#[tokio::test]
async fn store_poll_restart_uses_snapshot_instead_of_full_history() {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::Instant,
    };

    let key = SigningKey::from_bytes(&[42; 32]);
    let mut snapshot: Snapshot = serde_json::from_slice(SNAPSHOT).unwrap();
    let mut files = BTreeMap::from([
        ("/v1/head".into(), signed_head(&snapshot, 2, &key)),
        ("/v1/snapshots/2".into(), SNAPSHOT.to_vec()),
        ("/v1/releases/1".into(), FIRST.to_vec()),
        ("/v1/history/base.json".into(), BASE.to_vec()),
        ("/v1/history/days/2026-11-19.json".into(), DAY.to_vec()),
    ]);
    for day in ["2026-11-21", "2026-11-23", "2026-11-25"] {
        publish_snapshot_day(&mut files, &mut snapshot, &key, day);
    }
    let dir = tempfile::tempdir().unwrap();
    let store = poll_store(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into_owned(),
        },
    });
    let files = Arc::new(Mutex::new(files));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let url = server_with_requests(files.clone(), requests.clone());
    let poller = || {
        StorePoller::new(
            url.clone(),
            BTreeMap::from([(KID.into(), key.verifying_key())]),
        )
        .unwrap()
    };
    assert_eq!(poller().poll(&store).await.unwrap(), 5);

    let mut restarts = Vec::new();
    for number in 6..=7 {
        let release = next_named_release(&mut snapshot, &key);
        {
            let mut files = files.lock().unwrap();
            files.insert(
                format!("/v1/releases/{number}"),
                serde_json::to_vec(&release).unwrap(),
            );
            files.insert("/v1/head".into(), signed_head(&snapshot, 5, &key));
        }
        requests.lock().unwrap().clear();
        let mut restarted = poller();
        let started = Instant::now();
        let progress = Arc::new(AtomicU64::new(0));
        let stamp = Arc::clone(&progress);
        assert_eq!(
            restarted
                .poll_with_progress(&store, move || {
                    stamp.fetch_add(1, Ordering::Relaxed);
                })
                .await
                .unwrap(),
            number
        );
        let seen = requests.lock().unwrap().clone();
        println!(
            "restart at cursor {} applied {number} in {:?}: {} requests {seen:?}",
            number - 1,
            started.elapsed(),
            seen.len()
        );
        assert_eq!(progress.load(Ordering::Relaxed), 1 + number - 5);
        restarts.push(seen);
    }
    assert_eq!(
        restarts,
        [
            vec!["/v1/head", "/v1/snapshots/5", "/v1/releases/6"],
            vec![
                "/v1/head",
                "/v1/snapshots/5",
                "/v1/releases/6",
                "/v1/releases/7",
            ],
        ],
        "cold replay must start at the signed snapshot, not release 1"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "measures peak RSS; run alone so other tests do not affect it"]
async fn store_poll_large_curation_backlog_has_bounded_peak_rss() {
    fn peak_kib() -> u64 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        status
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmHWM:")?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
            .unwrap()
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("http");
    for folder in ["v1/history/days", "v1/releases", "v1/snapshots"] {
        std::fs::create_dir_all(root.join(folder)).unwrap();
    }
    let key = SigningKey::from_bytes(&[42; 32]);
    let mut snapshot: Snapshot = serde_json::from_slice(SNAPSHOT).unwrap();
    for (path, bytes) in [
        ("v1/head", signed_head(&snapshot, 2, &key).as_slice()),
        ("v1/snapshots/2", SNAPSHOT),
        ("v1/releases/1", FIRST),
        ("v1/history/base.json", BASE),
        ("v1/history/days/2026-11-19.json", DAY),
    ] {
        std::fs::write(root.join(path), bytes).unwrap();
    }
    // Serve response files from disk so the server does not contribute a
    // copy of the backlog to the consumer's measured peak.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let server_root = root.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut request = String::new();
            let mut reader = BufReader::new(&mut stream);
            if reader.read_line(&mut request).unwrap() == 0 {
                continue;
            }
            let file = request
                .split_ascii_whitespace()
                .nth(1)
                .unwrap()
                .trim_start_matches('/');
            while {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap() > 0 && header != "\r\n"
            } {}
            let mut response = File::open(server_root.join(file)).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.metadata().unwrap().len()
            )
            .unwrap();
            std::io::copy(&mut response, &mut stream).unwrap();
        }
    });
    let store = poll_store(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into_owned(),
        },
    });
    let mut poller =
        StorePoller::new(url, BTreeMap::from([(KID.into(), key.verifying_key())])).unwrap();
    assert_eq!(poller.poll(&store).await.unwrap(), 2);

    const BACKLOG: u64 = 32;
    snapshot.state.curation_set.vendor_facts.insert(
        "curated-providers".into(),
        json!({"schema":"curated-providers/v1", "providers":[], "padding":"x".repeat(1024 * 1024)}),
    );
    for n in 3..3 + BACKLOG {
        let mut release = next_named_release(&mut snapshot, &key);
        let set = &mut snapshot.state.curation_set;
        set.version += 1;
        release.change.curation_set = Some(set.clone());
        let envelope = &mut snapshot.envelope;
        envelope.curation_version = set.version;
        envelope.curation_hash = set.curation_hash();
        envelope.state_hash =
            fusiform_release::state_hash(envelope.curation_hash, envelope.upstream_hash);
        envelope.change_hash = release.change.change_hash();
        release.signatures = sign_envelope(envelope, KID, &key).unwrap();
        release.envelope = envelope.clone();
        let mut output =
            std::io::BufWriter::new(File::create(root.join(format!("v1/releases/{n}"))).unwrap());
        serde_json::to_writer(&mut output, &release).unwrap();
        output.flush().unwrap();
    }
    std::fs::write(root.join("v1/head"), signed_head(&snapshot, 2, &key)).unwrap();
    let before = peak_kib();
    assert_eq!(poller.poll(&store).await.unwrap(), 2 + BACKLOG);
    let after = peak_kib();
    println!("{BACKLOG} releases × 1 MiB: peak RSS before catch-up {before} KiB, after catch-up {after} KiB");
    assert!(
        after - before < 20 * 1024,
        "peak RSS grew by {} KiB",
        after - before
    );
}

fn publish_snapshot_day(
    files: &mut BTreeMap<String, Vec<u8>>,
    snapshot: &mut Snapshot,
    key: &SigningKey,
    day: &str,
) -> Release {
    use fusiform_release::{
        history_day_bytes, sign_history_attestation, HistoryAttestation, HistoryFile,
    };

    // A quiet gap longer than one day must not force an already current
    // consumer to replay older releases.
    snapshot.envelope.published_at_ms += 2 * 86_400_000;
    snapshot.envelope.upstream_observed_at_ms += 2 * 86_400_000;
    let release = next_named_release(snapshot, key);
    let mut line = serde_json::to_vec(&release).unwrap();
    line.push(b'\n');
    let bytes = history_day_bytes(day, &line).unwrap();
    let file = format!("days/{day}.json");
    let mut body = snapshot.history_attestation.as_ref().unwrap().body.clone();
    body.release = NonZeroU64::new(release.envelope.release).unwrap();
    body.state_hash = release.envelope.state_hash;
    body.history.push(HistoryFile {
        file: file.clone(),
        sha256: sha256(&bytes),
    });
    snapshot.history_attestation = Some(HistoryAttestation {
        signatures: sign_history_attestation(&body, KID, key),
        body,
    });
    snapshot.signatures = release.signatures.clone();
    let number = release.envelope.release;
    files.insert(format!("/v1/history/{file}"), bytes);
    files.insert(
        format!("/v1/releases/{number}"),
        serde_json::to_vec(&release).unwrap(),
    );
    files.insert(
        format!("/v1/snapshots/{number}"),
        serde_json::to_vec(snapshot).unwrap(),
    );
    files.insert("/v1/head".into(), signed_head(snapshot, number, key));
    release
}

#[tokio::test]
async fn store_poll_warm_catch_up_reads_only_the_missing_suffix() {
    for prior_days in [0, 2] {
        let key = SigningKey::from_bytes(&[42; 32]);
        let mut snapshot: Snapshot = serde_json::from_slice(SNAPSHOT).unwrap();
        let mut files = BTreeMap::from([
            ("/v1/head".into(), signed_head(&snapshot, 2, &key)),
            ("/v1/snapshots/2".into(), SNAPSHOT.to_vec()),
            ("/v1/releases/1".into(), FIRST.to_vec()),
            ("/v1/history/base.json".into(), BASE.to_vec()),
            ("/v1/history/days/2026-11-19.json".into(), DAY.to_vec()),
        ]);
        let mut days = ["2026-11-21", "2026-11-23", "2026-11-25", "2026-11-27"].into_iter();
        for day in days.by_ref().take(prior_days) {
            publish_snapshot_day(&mut files, &mut snapshot, &key, day);
        }
        let dir = tempfile::tempdir().unwrap();
        let store = poll_store(&StorageDescriptor {
            module_id: "fusiform".into(),
            storage_namespace: "default".into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.path().join("store.db").to_string_lossy().into_owned(),
            },
        });
        let files = Arc::new(Mutex::new(files));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut poller = StorePoller::new(
            server_with_requests(files.clone(), requests.clone()),
            BTreeMap::from([(KID.into(), key.verifying_key())]),
        )
        .unwrap();
        let previous = poller.poll(&store).await.unwrap();
        requests.lock().unwrap().clear();

        let release = publish_snapshot_day(
            &mut files.lock().unwrap(),
            &mut snapshot,
            &key,
            days.next().unwrap(),
        );
        let next = previous + 1;
        let next_path = format!("/v1/releases/{next}");
        if prior_days == 0 {
            let mut bad = release.clone();
            bad.signatures.sig.clear();
            files
                .lock()
                .unwrap()
                .insert(next_path.clone(), serde_json::to_vec(&bad).unwrap());
            assert!(poller.poll(&store).await.unwrap_err().contains("signature"));
            assert_eq!(
                store.latest_store_release().unwrap().unwrap().release,
                previous
            );
            assert!(poller.verified.is_none());
            files
                .lock()
                .unwrap()
                .insert(next_path.clone(), serde_json::to_vec(&release).unwrap());
            requests.lock().unwrap().clear();
        }
        assert_eq!(poller.poll(&store).await.unwrap(), next);
        if prior_days == 0 {
            // A rejected individual object invalidates the warm path. The
            // next poll verifies the archived copy instead.
            assert_eq!(
                *requests.lock().unwrap(),
                [
                    "/v1/head".to_owned(),
                    format!("/v1/snapshots/{next}"),
                    "/v1/releases/1".into(),
                    "/v1/history/days/2026-11-19.json".into(),
                    "/v1/history/days/2026-11-21.json".into(),
                ]
            );
        } else {
            assert_eq!(*requests.lock().unwrap(), ["/v1/head".into(), next_path]);
        }

        let archived = publish_snapshot_day(
            &mut files.lock().unwrap(),
            &mut snapshot,
            &key,
            days.next().unwrap(),
        );
        let number = archived.envelope.release;
        if prior_days == 0 {
            // The individual object is missing but the signed snapshot covers it.
            files
                .lock()
                .unwrap()
                .remove(&format!("/v1/releases/{number}"));
        } else {
            // Replay must reconstruct and verify the state at the local
            // cursor if no matching state was retained.
            Arc::get_mut(poller.verified.as_mut().unwrap())
                .unwrap()
                .envelope
                .curation_hash = Hash::from_bytes([0; 32]);
        }
        requests.lock().unwrap().clear();
        assert_eq!(poller.poll(&store).await.unwrap(), number);
        assert!(requests
            .lock()
            .unwrap()
            .contains(&format!("/v1/snapshots/{number}")));
    }
}

#[test]
fn store_poll_writes_the_same_model_eras_as_direct_ingest() {
    use fusiform_core::{
        normalize_legacy_models_dev_for_tests, BoundaryKind, ChargeUnitEvidence,
        ObservationOutcome, SourceId, Timestamp,
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
        let step = publish_step(&body, &index, &ChargeUnitEvidence::default(), at).unwrap();
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
        let previous_at = previous
            .as_ref()
            .map(|previous| previous.envelope.upstream_observed_at_ms);
        let (next, before) = if let Some(mut previous) = previous.take() {
            let before = previous.advance(&release, &keys).unwrap();
            (previous, before)
        } else {
            (Verified::first(release.clone(), &keys).unwrap(), None)
        };
        let arrival = translate::arrival(
            previous_at,
            before.as_ref(),
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
        let catalog = normalize_legacy_models_dev_for_tests(&body)
            .unwrap()
            .catalog;
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
async fn store_poll_reports_silence_after_a_successful_head_check() {
    use subc_protocol::session::HealthStatus;

    let key = SigningKey::from_bytes(&[42; 32]);
    let snapshot: Snapshot = serde_json::from_slice(SNAPSHOT).unwrap();
    let url = server(Arc::new(Mutex::new(BTreeMap::from([
        ("/v1/head".into(), signed_head(&snapshot, 2, &key)),
        ("/v1/snapshots/2".into(), SNAPSHOT.to_vec()),
        ("/v1/releases/1".into(), FIRST.to_vec()),
        ("/v1/history/base.json".into(), BASE.to_vec()),
        ("/v1/history/days/2026-11-19.json".into(), DAY.to_vec()),
    ]))));
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        CatalogStore::open_for_store(&StorageDescriptor {
            module_id: "fusiform".into(),
            storage_namespace: "default".into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir.path().join("store.db").to_string_lossy().into_owned(),
            },
        })
        .unwrap(),
    );
    let signals = Arc::new(Signals::for_store(url.to_string()));
    signals.store_opened();

    for expected_attempts in [1, 2] {
        let poller = StorePoller::new(
            url.clone(),
            BTreeMap::from([(KID.into(), key.verifying_key())]),
        )
        .unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(poller.run(
            Arc::clone(&store),
            Arc::clone(&signals),
            move || tx.send(()).unwrap(),
            false,
        ));
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("poll did not verify the head")
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        assert_eq!(store.latest_store_release().unwrap().unwrap().release, 2);
        assert_eq!(signals.store_applied_release(), Some(2));
        let now = crate::route::now_ms();
        assert_eq!(
            crate::health::report(&signals, now).status,
            HealthStatus::Ok
        );
        let silent = crate::health::report(&signals, now + 13 * 60_000);
        assert_eq!(silent.status, HealthStatus::Degraded, "{:?}", silent.detail);
        assert!(silent.detail.unwrap().contains("poll loop"));

        let metrics = silent.metrics.unwrap();
        assert_eq!(metrics["process_poll_attempts"], expected_attempts);
        assert_eq!(metrics["process_polls_recorded"], 1);
        let observed_at = store
            .last_confirming_observation(fusiform_core::SourceId::ModelsDev)
            .unwrap()
            .unwrap();
        assert_eq!(
            metrics["observation_age_ms"],
            now + 13 * 60_000 - observed_at.0
        );
        assert!(metrics["model_count"].is_number());
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
    let store = poll_store(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into_owned(),
        },
    });
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
    curation_set.kind_grades.insert(
        "zeta/mini".into(),
        BTreeMap::from([
            ("decision".into(), 83.into()),
            ("transcription".into(), 98.into()),
        ]),
    );
    snapshot.state.curation_set = curation_set.clone();
    let mut set_fact = |provider: &str, model: &str, fact: &str, value: Value| {
        snapshot
            .state
            .upstream
            .get_mut(provider)
            .unwrap()
            .models
            .get_mut(model)
            .unwrap()
            .insert(fact.into(), value.clone());
        fusiform_release::ChangeOp::Set {
            provider: provider.into(),
            model: model.into(),
            fact: fact.into(),
            value,
        }
    };
    let change = fusiform_release::Change {
        upstream: vec![
            set_fact("alpha", "-", "capability.input_modalities", json!(["text"])),
            set_fact("alpha", "-", "model.canonical_id", json!("zeta/mini")),
            set_fact(
                "zeta",
                "mini",
                "capability.input_modalities",
                json!(["audio", "text"]),
            ),
            set_fact("zeta", "mini", "model.canonical_id", json!("zeta/mini")),
            set_fact("zeta", "mini", "model.kind", json!("decision")),
            set_fact("zeta", "mini", "model.name", json!("Mini 3")),
        ],
        curation_set: Some(curation_set),
    };
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
    let decision = crate::route::serve_catalog_get(
        &store,
        br#"{"provider_id":"zeta","model_id":"mini","fact_prefixes":["quality.grade.decision"]}"#,
    )
    .unwrap();
    assert_eq!(decision.models["zeta/mini"]["quality.grade.decision"], 83);
    assert_eq!(
        decision.quality_scores.unwrap().graded_at_ms,
        third.envelope.published_at_ms
    );
    let transcription = crate::route::serve_catalog_get(
        &store,
        br#"{"fact_prefixes":["quality.grade.transcription","existence"]}"#,
    )
    .unwrap();
    assert_eq!(
        transcription.models["zeta/mini"]["quality.grade.transcription"],
        98
    );
    assert_eq!(
        transcription.models["alpha/-"],
        BTreeMap::from([("existence".into(), json!("present"))]),
    );
    assert_eq!(
        transcription.quality_scores.unwrap().graded_at_ms,
        third.envelope.published_at_ms
    );
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

#[tokio::test]
async fn store_poll_keeps_one_state_and_refuses_tampered_bytes() {
    let key = SigningKey::from_bytes(&[42; 32]);
    let mut snapshot: Snapshot = serde_json::from_slice(SNAPSHOT).unwrap();
    let files = Arc::new(Mutex::new(BTreeMap::from([
        ("/v1/head".into(), signed_head(&snapshot, 2, &key)),
        ("/v1/snapshots/2".into(), SNAPSHOT.to_vec()),
        ("/v1/releases/1".into(), FIRST.to_vec()),
        ("/v1/history/base.json".into(), BASE.to_vec()),
        ("/v1/history/days/2026-11-19.json".into(), DAY.to_vec()),
    ])));
    let dir = tempfile::tempdir().unwrap();
    let store = poll_store(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into_owned(),
        },
    });
    let mut poller = StorePoller::new(
        server(Arc::clone(&files)),
        BTreeMap::from([(KID.into(), key.verifying_key())]),
    )
    .unwrap();
    assert_eq!(poller.poll(&store).await.unwrap(), 2);
    let bytes_at_bootstrap = poller.verified.as_ref().unwrap().bytes.len();
    let publish = |snapshot: &mut Snapshot, number| {
        let release = next_named_release(snapshot, &key);
        assert_eq!(release.envelope.release, number);
        let mut files = files.lock().unwrap();
        files.insert("/v1/head".into(), signed_head(snapshot, 2, &key));
        files.insert(
            format!("/v1/releases/{number}"),
            serde_json::to_vec(&release).unwrap(),
        );
    };
    for number in 3..=32 {
        publish(&mut snapshot, number);
        assert_eq!(poller.poll(&store).await.unwrap(), number);
        assert_eq!(
            poller.verified.as_ref().unwrap().bytes.len(),
            bytes_at_bootstrap
        );
    }

    assert_eq!(poller.poll(&store).await.unwrap(), 32);
    let version = store.catalog_version().unwrap();
    publish(&mut snapshot, 33);
    let path = "/v1/releases/33";
    let release = files.lock().unwrap().remove(path).unwrap();
    assert!(poller.poll(&store).await.unwrap_err().contains("404"));
    assert!(
        poller.verified.is_some(),
        "a transport miss keeps the anchor"
    );
    files.lock().unwrap().insert(path.into(), release);

    // Broken JSON cannot recreate verified state from the retained copy.
    Arc::get_mut(poller.verified.as_mut().unwrap())
        .unwrap()
        .bytes = Box::from(&b"{"[..]);
    assert!(poller
        .poll(&store)
        .await
        .unwrap_err()
        .contains("decode verified store state"));
    assert_eq!(store.catalog_version().unwrap(), version);
    assert_eq!(store.latest_store_release().unwrap().unwrap().release, 32);

    assert_eq!(
        poller.poll(&store).await,
        Ok(33),
        "the next poll must rebuild the rejected retained state from signed releases"
    );
    assert_eq!(
        model_name_at(&store, snapshot.envelope.upstream_observed_at_ms),
        "Mini 3"
    );
}
