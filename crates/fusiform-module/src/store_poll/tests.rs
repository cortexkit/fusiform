use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    num::NonZeroU64,
    sync::{Arc, Mutex},
};

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_release::{canonical_json, sign_envelope, sign_head, SigningKey};
use serde_json::json;

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

#[tokio::test]
async fn store_poll_verifies_attested_history_and_keeps_last_good_release() {
    let key = SigningKey::from_bytes(&[42; 32]);
    let mut snapshot: Snapshot = serde_json::from_slice(SNAPSHOT).unwrap();
    let wrong_base = [b" ".as_slice(), BASE].concat();
    let files = Arc::new(Mutex::new(BTreeMap::from([
        ("/head.json".into(), signed_head(&snapshot, 2, &key)),
        ("/snapshots/2.json".into(), SNAPSHOT.to_vec()),
        ("/releases/1.json".into(), FIRST.to_vec()),
        ("/history/base.json".into(), wrong_base),
        ("/history/days/2026-11-19.json".into(), DAY.to_vec()),
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
        "/snapshots/2.json".into(),
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
        .insert("/snapshots/2.json".into(), SNAPSHOT.to_vec());
    let mut invalid_anchor: Release = serde_json::from_slice(FIRST).unwrap();
    invalid_anchor.envelope.published_at_ms -= 1;
    files.lock().unwrap().insert(
        "/releases/1.json".into(),
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
        .insert("/releases/1.json".into(), FIRST.to_vec());
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
        .insert("/history/base.json".into(), BASE.to_vec());
    poller.poll(&store).await.unwrap();
    assert_eq!(store.latest_store_release().unwrap().unwrap().release, 2);
    let at_release_two = snapshot.envelope.upstream_observed_at_ms;
    assert_eq!(model_name_at(&store, at_release_two), "Mini 2");

    let change = fusiform_release::Change {
        upstream: vec![fusiform_release::ChangeOp::Set {
            provider: "zeta".into(),
            model: "mini".into(),
            fact: "model.name".into(),
            value: json!("Mini 3"),
        }],
        curation_set: None,
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
    envelope.state_hash =
        fusiform_release::state_hash(envelope.curation_hash, envelope.upstream_hash);
    envelope.change_hash = change.change_hash();
    let third = Release {
        envelope: envelope.clone(),
        change,
        signatures: sign_envelope(envelope, KID, &key).unwrap(),
    };
    {
        let mut files = files.lock().unwrap();
        files.insert("/head.json".into(), signed_head(&snapshot, 2, &key));
        files.insert(
            "/releases/3.json".into(),
            serde_json::to_vec(&third).unwrap(),
        );
    }
    poller.poll(&store).await.unwrap();
    let version = store.catalog_version().unwrap();
    assert_eq!(
        model_name_at(&store, third.envelope.upstream_observed_at_ms),
        "Mini 3"
    );
    assert_eq!(model_name_at(&store, at_release_two), "Mini 2");

    let mut head: Value = serde_json::from_slice(&signed_head(&snapshot, 2, &key)).unwrap();
    head["head"]["published_at_ms"] = json!(0);
    files
        .lock()
        .unwrap()
        .insert("/head.json".into(), serde_json::to_vec(&head).unwrap());
    assert!(poller.poll(&store).await.unwrap_err().contains("signature"));
    assert_eq!(store.catalog_version().unwrap(), version);
    assert_eq!(store.latest_store_release().unwrap().unwrap().release, 3);
}
