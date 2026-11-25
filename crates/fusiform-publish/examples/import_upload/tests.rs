use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    num::NonZeroU64,
    thread,
    time::Duration,
};

use flate2::read::GzDecoder;
use fusiform_release::{Head, HeadBody, Release, SigningKey, sha256, sign_head};
use serde_json::{Value, json};

use super::{VarError, redact, run};

const TOKEN: &str = "fake-store-import-token";
const KEY_ID: &str = "release-format-v1-test-key";
const PUBLIC_KEY: &str = "GX9rI-FshTLGq8g4-s1ep4m-DHaykgM0A5v6iz02jWE";
const COMMIT_REPLY: &str = r#"{"release":1}"#;
const GENESIS: &[u8] =
    include_bytes!("../../../fusiform-release/fixtures/release-format-v1/chain/release-1.json");
const ARTIFACTS: [(&str, &str, &[u8]); 3] = [
    ("release-1.json", "release-1.json", GENESIS),
    ("state.json", "state.json", b"{\"upstream\":{}}\r\n"),
    ("base.json", "history/base.json", b"{\"tables\":{}}"),
];
const UPLOADS: [&str; 3] = [
    "/store/v1/import/files/release-1.json",
    "/store/v1/import/files/state.json",
    "/store/v1/import/files/base.json",
];
const COMMIT: &str = "/store/v1/import/commit";
const HEAD: &str = "/store/v1/head";

#[derive(Clone, Copy)]
enum Reply {
    Normal,
    WrongHash,
    WrongSize,
    RefuseUpload,
    RefuseCommit,
    CommitBody(&'static str),
    DifferentRelease,
    DifferentSnapshot,
    DifferentState,
    BadHeadSignature,
    EchoInvalidReceipt,
    EchoRefusal,
}

fn against_stub(
    reply: Reply,
    token: Result<String, VarError>,
) -> (Result<String, String>, Vec<String>) {
    let out_dir = tempfile::tempdir().unwrap();
    fs::create_dir(out_dir.path().join("history")).unwrap();
    for (_, path, bytes) in ARTIFACTS {
        fs::write(out_dir.path().join(path), bytes).unwrap();
    }
    if matches!(reply, Reply::RefuseUpload) {
        // Keep the compressed body large enough to exercise a refusal while
        // the client is still streaming it.
        let mut state = 1u32;
        let bytes: Vec<_> = (0..1024 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        fs::write(out_dir.path().join("release-1.json"), bytes).unwrap();
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::scope(|scope| {
        let server = scope.spawn(move || {
            let mut paths = Vec::new();
            for stream in listener.incoming() {
                let Some(path) = serve(stream.unwrap(), reply) else {
                    break;
                };
                paths.push(path);
            }
            paths
        });
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(
                [
                    out_dir.path().as_os_str().into(),
                    format!("http://{address}/store/").into(),
                    "--key-id".into(),
                    KEY_ID.into(),
                    "--public-key".into(),
                    PUBLIC_KEY.into(),
                ]
                .into_iter(),
                token,
            ));
        // An empty connection wakes the listener once the client has finished.
        let _ = TcpStream::connect(address);
        (result, server.join().unwrap())
    })
}

fn serve(mut stream: TcpStream, reply: Reply) -> Option<String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap() == 0 {
        return None;
    }
    let mut request = line.split_whitespace();
    let method = request.next().unwrap().to_owned();
    let path = request.next().unwrap().to_owned();
    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        let (field, value) = line.split_once(':').unwrap();
        headers.insert(field.to_ascii_lowercase(), value.trim().to_owned());
    }
    if path == HEAD {
        assert_eq!(method, "GET");
        assert!(!headers.contains_key("authorization"));
    } else {
        assert_eq!(headers["authorization"], format!("Bearer {TOKEN}"));
    }
    if matches!(reply, Reply::RefuseUpload) {
        assert_eq!(method, "PUT");
        let mut stream = reader.into_inner();
        stream.write_all(b"HTTP/1.1 409 Conflict\r\nContent-Length: 16\r\nConnection: close\r\n\r\nalready imported").unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        // Drain pending data so a reset from closing the socket does not race
        // delivery of the refusal to the client.
        let _ = io::copy(&mut stream, &mut io::sink());
        return Some(path);
    }
    let (status, text) = if path == HEAD {
        ("200 OK", signed_head(reply))
    } else if path == COMMIT {
        let body = read_body(&mut reader, &headers);
        assert_eq!(method, "POST");
        assert_eq!(headers["content-type"], "application/json");
        let hashes: BTreeMap<_, _> = ARTIFACTS
            .into_iter()
            .map(|(name, _, raw)| (name, sha256(raw).to_string()))
            .collect();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!(hashes)
        );
        match reply {
            Reply::RefuseCommit => ("409 Conflict", r#"{"error":"already_imported"}"#.into()),
            Reply::CommitBody(text) => ("200 OK", text.into()),
            Reply::EchoInvalidReceipt => ("200 OK", format!(r#"{{"debug":"Bearer {TOKEN}"}}"#)),
            _ => ("200 OK", COMMIT_REPLY.into()),
        }
    } else {
        let body = read_body(&mut reader, &headers);
        assert_eq!(method, "PUT");
        assert_eq!(headers["content-encoding"], "gzip");
        let name = path.strip_prefix("/store/v1/import/files/").unwrap();
        let (_, _, expected) = ARTIFACTS.iter().find(|(file, _, _)| *file == name).unwrap();
        let mut raw = Vec::new();
        GzDecoder::new(body.as_slice())
            .read_to_end(&mut raw)
            .unwrap();
        assert_eq!(raw, *expected);
        match reply {
            Reply::EchoRefusal => ("409 Conflict", format!("already imported; Bearer {TOKEN}")),
            _ => {
                let mut receipt =
                    json!({"file":name,"raw_bytes":raw.len(),"sha256":sha256(&raw).to_string()});
                if name == "base.json" {
                    match reply {
                        Reply::WrongHash => receipt["sha256"] = json!("0".repeat(64)),
                        Reply::WrongSize => receipt["raw_bytes"] = json!(raw.len() + 1),
                        _ => {}
                    }
                }
                ("200 OK", receipt.to_string())
            }
        }
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )
    .unwrap();
    Some(path)
}

fn signed_head(reply: Reply) -> String {
    let genesis: Release = serde_json::from_slice(GENESIS).unwrap();
    let head = HeadBody {
        release: if matches!(reply, Reply::DifferentRelease) {
            NonZeroU64::new(2).unwrap()
        } else {
            NonZeroU64::MIN
        },
        latest_snapshot: 1,
        state_hash: if matches!(reply, Reply::DifferentState) {
            sha256(b"different state")
        } else {
            genesis.envelope.state_hash
        },
        published_at_ms: genesis.envelope.published_at_ms,
    };
    let signing = SigningKey::from_bytes(
        &[if matches!(reply, Reply::BadHeadSignature) {
            43
        } else {
            42
        }; 32],
    );
    let signatures = sign_head(&head, KEY_ID, &signing).unwrap();
    let mut wire = json!(Head { head, signatures });
    if matches!(reply, Reply::DifferentSnapshot) {
        wire["head"]["latest_snapshot"] = json!(0);
    }
    wire.to_string()
}

fn read_body(reader: &mut impl BufRead, headers: &BTreeMap<String, String>) -> Vec<u8> {
    if let Some(length) = headers.get("content-length") {
        let mut body = vec![0; length.parse().unwrap()];
        reader.read_exact(&mut body).unwrap();
        return body;
    }
    assert_eq!(headers["transfer-encoding"], "chunked");
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let length = usize::from_str_radix(line.trim_end(), 16).unwrap();
        let start = body.len();
        body.resize(start + length, 0);
        reader.read_exact(&mut body[start..]).unwrap();
        let mut end = [0; 2];
        reader.read_exact(&mut end).unwrap();
        assert_eq!(&end, b"\r\n");
        if length == 0 {
            return body;
        }
    }
}

#[test]
fn uploads_three_gzip_files_and_commits_the_raw_hashes() {
    let (result, paths) = against_stub(Reply::Normal, Ok(TOKEN.into()));
    assert_eq!(result.unwrap(), COMMIT_REPLY);
    assert_eq!(paths, [UPLOADS.as_slice(), &[COMMIT, HEAD]].concat());
}

#[test]
fn http_200_without_the_exact_commit_receipt_is_not_success() {
    for body in [
        "<html>Maintenance</html>",
        r#"{"release":2}"#,
        r#"{"release":1,"extra":true}"#,
    ] {
        let (result, paths) = against_stub(Reply::CommitBody(body), Ok(TOKEN.into()));
        let error = result.expect_err("HTTP 200 alone is not a successful commit receipt");
        assert!(error.contains(body), "{error}");
        assert_eq!(paths, [UPLOADS.as_slice(), &[COMMIT]].concat());
    }
}

#[test]
fn receipt_requires_a_matching_pinned_signed_head() {
    for (reply, expected) in [
        (
            Reply::DifferentRelease,
            "store head does not confirm release 1",
        ),
        (
            Reply::DifferentState,
            "store head does not confirm release 1",
        ),
        (Reply::DifferentSnapshot, "invalid store head"),
        (Reply::BadHeadSignature, "verify store head after commit"),
    ] {
        let (result, paths) = against_stub(reply, Ok(TOKEN.into()));
        let error = result.expect_err("commit receipt must be confirmed by the signed head");
        assert!(error.contains(expected), "{error}");
        assert_eq!(paths, [UPLOADS.as_slice(), &[COMMIT, HEAD]].concat());
    }
}

#[test]
fn incorrect_sha256_prevents_commit() {
    let (result, paths) = against_stub(Reply::WrongHash, Ok(TOKEN.into()));
    assert!(
        result
            .unwrap_err()
            .starts_with("base.json: upload receipt does not match")
    );
    assert_eq!(paths, UPLOADS);
}

#[test]
fn incorrect_raw_size_prevents_commit() {
    let (result, paths) = against_stub(Reply::WrongSize, Ok(TOKEN.into()));
    assert!(
        result
            .unwrap_err()
            .starts_with("base.json: upload receipt does not match")
    );
    assert_eq!(paths, UPLOADS);
}

#[test]
fn reports_the_stores_already_imported_refusal_on_either_route() {
    let (result, paths) = against_stub(Reply::RefuseUpload, Ok(TOKEN.into()));
    assert_eq!(
        result.unwrap_err(),
        "release-1.json: 409 Conflict: already imported"
    );
    assert_eq!(paths, &UPLOADS[..1]);

    let (result, paths) = against_stub(Reply::RefuseCommit, Ok(TOKEN.into()));
    assert_eq!(
        result.unwrap_err(),
        "commit: 409 Conflict: already_imported"
    );
    assert_eq!(paths, [UPLOADS.as_slice(), &[COMMIT]].concat());
}

#[test]
fn missing_token_is_refused_without_a_request() {
    let (result, paths) = against_stub(Reply::Normal, Err(VarError::NotPresent));
    assert_eq!(result.unwrap_err(), "MODELS_IMPORT_TOKEN is required");
    assert!(paths.is_empty());
}

#[test]
fn redacts_the_token_in_malformed_receipts_and_refusal_output() {
    let (result, _) = against_stub(Reply::EchoInvalidReceipt, Ok(TOKEN.into()));
    assert_eq!(
        result.unwrap_err(),
        r#"commit: expected {"release":1}, got {"debug":"Bearer [redacted]"}"#
    );
    let (result, _) = against_stub(Reply::EchoRefusal, Ok(TOKEN.into()));
    assert_eq!(
        result.unwrap_err(),
        "release-1.json: 409 Conflict: already imported; Bearer [redacted]"
    );
}

#[test]
fn redaction_marker_cannot_echo_a_short_token() {
    assert!(redact("Bearer redacted".into(), "redacted").is_empty());
}

#[tokio::test]
async fn refuses_plaintext_bases_outside_the_loopback_allowlist() {
    for base in ["http://example.com", "http://127.0.0.2", "http://[::1]"] {
        let result = run(
            ["out", base, "--key-id", KEY_ID, "--public-key", PUBLIC_KEY]
                .into_iter()
                .map(Into::into),
            Ok(TOKEN.into()),
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .starts_with("import base URL must use HTTPS")
        );
    }
}
