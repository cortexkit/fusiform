use std::{
    io::{BufRead as _, BufReader},
    net::TcpListener,
    num::NonZeroU64,
    path::Path,
};

use fusiform_release::{
    Change, ChangeOp, CurationSet, Head, HeadBody, Release, ReleaseEnvelope, ReleaseFormat,
    SigningKey, Snapshot, State, sign_envelope, sign_head, state_hash, upstream_state_hash,
};
use serde_json::json;

use super::*;

const KID: &str = "test-key";
const PUBLIC_KEY: &str = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";

fn fake_store(tamper_release: bool) -> String {
    // RFC 8032 test vector; this key has no authority outside the fake store.
    let signing = SigningKey::from_bytes(&[
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ]);
    let curation_set: CurationSet = serde_json::from_value(json!({
        "version": 1, "approved_at_ms": null, "approved_drafts": [], "grades": {},
        "effort_grades": {}, "kind_grades": {}, "workload_efforts": {}, "decommissioned": {},
        "vendor_facts": {},
    }))
    .unwrap();
    let options = json!([{"type":"effort","values":["low","high"]}]);
    let mut state = State {
        upstream: serde_json::from_value(json!({"google":{"provider":{},"models":{
            "neon":{"capability.reasoning_options":options}
        }}}))
        .unwrap(),
        curation_set,
    };
    let sign_release = |state: &State, change: Change| {
        let n = state.curation_set.version;
        let upstream_hash = upstream_state_hash(&state.upstream);
        let curation_hash = state.curation_set.curation_hash();
        let envelope = ReleaseEnvelope {
            format: ReleaseFormat::V1,
            release: n,
            parent: (n > 1).then_some(n - 1),
            published_at_ms: 100 + i64::try_from(n).unwrap(),
            upstream_observed_at_ms: 100,
            upstream_hash,
            curation_version: n,
            curation_hash,
            state_hash: state_hash(curation_hash, upstream_hash),
            change_hash: change.change_hash(),
        };
        Release {
            signatures: sign_envelope(&envelope, KID, &signing).unwrap(),
            envelope,
            change,
        }
    };
    let first = sign_release(
        &state,
        Change {
            upstream: vec![ChangeOp::Set {
                provider: "google".to_owned(),
                model: "neon".to_owned(),
                fact: "capability.reasoning_options".to_owned(),
                value: options,
            }],
            curation_set: Some(state.curation_set.clone()),
        },
    );
    let snapshot = Snapshot {
        envelope: first.envelope.clone(),
        signatures: first.signatures.clone(),
        state: state.clone(),
        history_attestation: None,
    };
    state.curation_set.version += 1;
    state
        .curation_set
        .vendor_facts
        .insert("model-aliases".into(), json!({"aliases":[]}));
    state.curation_set.grades.insert(
        "google/neon".into(),
        serde_json::from_value(json!({"elo":2100,"eq":2000,"graded_effort":"high"})).unwrap(),
    );
    state.curation_set.decommissioned.insert(
        "google/neon".into(),
        fusiform_release::Decommissioned {
            reason: "test case".into(),
        },
    );
    let mut latest = sign_release(
        &state,
        Change {
            upstream: Vec::new(),
            curation_set: Some(state.curation_set.clone()),
        },
    );
    let head = HeadBody {
        release: NonZeroU64::new(2).unwrap(),
        latest_snapshot: 1,
        state_hash: latest.envelope.state_hash,
        published_at_ms: latest.envelope.published_at_ms,
    };
    let head = Head {
        signatures: sign_head(&head, KID, &signing).unwrap(),
        head,
    };
    if tamper_release {
        latest
            .change
            .curation_set
            .as_mut()
            .unwrap()
            .vendor_facts
            .clear();
    }
    let routes = BTreeMap::from([
        ("/v1/head", json(head)),
        ("/v1/snapshots/1", json(snapshot)),
        ("/v1/releases/1", json(first)),
        ("/v1/releases/2", json(latest)),
    ]);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().take(routes.len()) {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut parts = request.split_ascii_whitespace();
            assert_eq!(parts.next(), Some("GET"));
            let body = &routes[parts.next().unwrap()];
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    url
}

async fn invoke(url: &str, out: &Path, command: &[&str]) -> Result<()> {
    run([
        "--url",
        url,
        "--key-id",
        KID,
        "--public-key",
        PUBLIC_KEY,
        "--out",
        out.to_str().unwrap(),
    ]
    .into_iter()
    .chain(command.iter().copied())
    .map(str::to_owned))
    .await
}

async fn vendor(url: &str, dir: &Path, file: &str) -> Result<()> {
    let input = dir.join("alias.json");
    fs::write(&input, file).unwrap();
    invoke(
        url,
        dir,
        &[
            "vendor",
            "model-aliases",
            "--file",
            input.to_str().unwrap(),
            "--note",
            "test note",
            "--source",
            "https://example.test/source",
            "--review-by",
            "2027-01-18",
        ],
    )
    .await
}

fn alias_file(model: &str) -> String {
    format!(
        r#"{{"aliases":[{{"provider_id":"google","model_id":"antigravity-neon","target":{{"provider_id":"google","model_id":"{model}"}},"basis":"test row","source_ref":"test source","established_at_ms":1,"review_by_ms":2}}]}}"#
    )
}

#[tokio::test]
async fn rejects_alias_target_absent_from_verified_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let error = vendor(&fake_store(false), dir.path(), &alias_file("not-here"))
        .await
        .unwrap_err();
    assert!(
        error.contains("targets google/not-here, absent from verified upstream"),
        "{error}"
    );
    assert!(!dir.path().join("request.json").exists());
}

#[tokio::test]
async fn valid_alias_produces_exact_request() {
    let dir = tempfile::tempdir().unwrap();
    let file = alias_file("neon");
    vendor(&fake_store(false), dir.path(), &file).await.unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("request.json")).unwrap(),
        format!(
            r#"{{"value":{file},"note":"test note","source":"https://example.test/source","review_by":"2027-01-18"}}"#,
        )
    );
}

#[tokio::test]
async fn vendor_numbers_keep_their_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let file = " \n{ \"aliases\": [], \"metadata\": [1E-09,1.2300,18446744073709551617] }\n";
    vendor(&fake_store(false), dir.path(), file).await.unwrap();
    let body = fs::read_to_string(dir.path().join("request.json")).unwrap();
    assert!(
        body.starts_with(&format!(r#"{{"value":{file},"note":"#)),
        "{body}"
    );
}

#[tokio::test]
async fn refuses_byte_identical_vendor_document_from_latest_curation() {
    let dir = tempfile::tempdir().unwrap();
    let error = vendor(&fake_store(false), dir.path(), r#"{"aliases":[]}"#)
        .await
        .unwrap_err();
    assert!(error.contains("byte-identical"), "{error}");
    assert!(!dir.path().join("request.json").exists());
}

#[tokio::test]
async fn rejects_tampered_later_release_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let error = vendor(&fake_store(true), dir.path(), &alias_file("neon"))
        .await
        .unwrap_err();
    assert!(error.contains("change_hash does not match"), "{error}");
    assert!(!dir.path().join("request.json").exists());
}

#[tokio::test]
async fn rejects_draft_effort_outside_verified_reasoning_options() {
    let dir = tempfile::tempdir().unwrap();
    let error = invoke(
        &fake_store(false),
        dir.path(),
        &[
            "draft",
            "workload_efforts",
            "google/neon",
            "--value",
            r#"{"code":{"default":"high","minimum":"xhigh"}}"#,
            "--note",
            "test note",
        ],
    )
    .await
    .unwrap_err();
    assert!(
        error.contains(r#"effort "xhigh" is not listed in capability.reasoning_options"#),
        "{error}"
    );
    assert!(!dir.path().join("request.json").exists());
}

#[tokio::test]
async fn rejects_unknown_family_without_a_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let error = invoke(
        "http://127.0.0.1:1",
        dir.path(),
        &[
            "draft",
            "unknown",
            "google/neon",
            "--remove",
            "--note",
            "test note",
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(error, r#"unknown draft family "unknown""#);
    assert!(!dir.path().join("request.json").exists());
}

#[tokio::test]
async fn from_local_emits_only_changed_drafts() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("local.json");
    fs::write(&input, r#"{"note":"test note","grades":{"graded_effort":"high","elo":2100,"eq":2000},"workload_efforts":{"code":{"default":"high"}},"decommissioned":null}"#).unwrap();
    let out = dir.path().join("requests");
    invoke(
        &fake_store(false),
        &out,
        &[
            "from-local",
            "--model",
            "google/neon",
            "--local-json",
            input.to_str().unwrap(),
        ],
    )
    .await
    .unwrap();
    let produced: std::collections::BTreeSet<_> = fs::read_dir(&out)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        produced,
        ["decommissioned".into(), "workload_efforts".into()].into()
    );
    assert_eq!(
        fs::read_to_string(out.join("decommissioned/request.json")).unwrap(),
        r#"{"note":"test note","value":null}"#
    );
    assert_eq!(
        fs::read_to_string(out.join("workload_efforts/request.json")).unwrap(),
        r#"{"note":"test note","value":{"code":{"default":"high"}}}"#
    );
    assert_eq!(segment("google/neon"), "google%2Fneon");
    assert_eq!(
        draft_path("grades", "google/neon"),
        "/v1/curation/drafts/grades?key=google%2Fneon"
    );
}
