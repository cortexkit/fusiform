//! The conditional GET is actually conditional.
//!
//! # Why this exists
//!
//! Found by mutation, in the oldest file in the module crate: deleting the
//! `If-None-Match` header from the request reddened NOTHING. Every 304 test in
//! the suite drives a stub that answers 304 whatever it is asked, so the
//! response was pinned and the request was not — and a fetcher that never sends
//! the header would pass all of them while downloading 3.6 MB every thirty
//! minutes forever.
//!
//! The failure is entirely silent by construction. Every value in the store
//! stays correct, the catalog stays fresh, health stays green; the only symptom
//! is bandwidth nobody is watching. It is the shape this repository keeps
//! finding: a mechanism verified by what it returns rather than by what it does.
//!
//! The one test that would have caught it is `FUSIFORM_LIVE`-gated and reaches
//! models.dev, so it is deliberately off in CI. **A guard that only runs when
//! someone remembers to set an environment variable is not covering the path.**

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use fusiform_module::fetch::{FetchOutcome, Fetcher, SourceEndpoint};

use super::http_stub;

async fn poll(fetcher: &Fetcher, endpoint: &SourceEndpoint, etag: Option<&str>) -> FetchOutcome {
    http_stub::within_deadline(fetcher.poll(endpoint, etag)).await
}

async fn tick(
    ctx: &fusiform_module::loop_::PollContext,
    at: i64,
) -> Result<fusiform_module::loop_::TickReport, fusiform_module::loop_::TickError> {
    http_stub::within_deadline(fusiform_module::loop_::tick(ctx, at)).await
}

/// What the stub answers next.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// 200 with a body and an ETag.
    Body,
    /// 304 with an ETag and no body, as models.dev really answers.
    NotModified,
    /// 304 with NO ETag header, which a proxy may produce.
    NotModifiedBare,
}

/// A stub that records what it was ASKED, not only what it answered.
///
/// Recording the request head is the whole point: the existing failure-path
/// stub records a request COUNT, which cannot distinguish a conditional request
/// from an unconditional one.
struct Upstream {
    _server: http_stub::StubServer,
    url: String,
    answer: Arc<Mutex<Answer>>,
    heads: Arc<Mutex<Vec<String>>>,
    requests: Arc<AtomicUsize>,
}

impl Upstream {
    fn start(initial: Answer) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        let port = listener.local_addr().unwrap().port();
        let answer = Arc::new(Mutex::new(initial));
        let heads = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::new(AtomicUsize::new(0));

        let a = Arc::clone(&answer);
        let h = Arc::clone(&heads);
        let r = Arc::clone(&requests);
        let server = http_stub::StubServer::start(listener, move |stream| {
            r.fetch_add(1, Ordering::SeqCst);
            let answer = *a.lock().unwrap_or_else(|p| p.into_inner());
            serve_one(stream, answer, &h);
        });

        Self {
            _server: server,
            url: format!("http://127.0.0.1:{port}/api.json"),
            answer,
            heads,
            requests,
        }
    }

    fn set(&self, answer: Answer) {
        *self.answer.lock().unwrap_or_else(|p| p.into_inner()) = answer;
    }

    fn heads(&self) -> Vec<String> {
        self.heads.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    fn endpoint(&self) -> SourceEndpoint {
        SourceEndpoint {
            source: fusiform_core::SourceId::ModelsDev,
            url: self.url.clone(),
        }
    }
}

/// Serve one request and close.
///
/// `Connection: close` on every response for the reason recorded in
/// `failure_path.rs`: this handles one request per connection, and an HTTP/1.1
/// response with `Content-Length` and no close header advertises a reusable
/// socket to a pooling client.
fn serve_one(mut stream: TcpStream, answer: Answer, heads: &Arc<Mutex<Vec<String>>>) {
    let mut buf = [0u8; 4096];
    let n = stream
        .read(&mut buf)
        .expect("stub request read before its deadline");
    assert!(n > 0, "stub client closed without a request");
    heads
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(String::from_utf8_lossy(&buf[..n]).to_string());

    match answer {
        Answer::Body => {
            let body = br#"{"anthropic":{"id":"anthropic","name":"A","models":{}}}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 ETag: \"v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
        }
        Answer::NotModified => {
            let _ = stream.write_all(
                b"HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\n\
                  Connection: close\r\n\r\n",
            );
        }
        Answer::NotModifiedBare => {
            let _ = stream.write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n");
        }
    }
}

fn header_line(head: &str, name: &str) -> Option<String> {
    head.lines()
        .find(|l| {
            l.to_ascii_lowercase()
                .starts_with(&name.to_ascii_lowercase())
        })
        .map(|l| l.trim().to_string())
}

#[tokio::test]
async fn a_stored_etag_is_sent_as_if_none_match() {
    let upstream = Upstream::start(Answer::NotModified);
    let fetcher = Fetcher::new().expect("a fetcher");

    let outcome = poll(&fetcher, &upstream.endpoint(), Some("\"v1\"")).await;
    assert!(
        matches!(outcome, FetchOutcome::NotModified { .. }),
        "the stub answers 304, got {outcome:?}"
    );

    let heads = upstream.heads();
    assert_eq!(heads.len(), 1, "exactly one request should have been made");

    // The assertion the whole file exists for. Without it, deleting the header
    // from the fetcher changes nothing any test can see.
    let sent = header_line(&heads[0], "if-none-match").unwrap_or_else(|| {
        panic!(
            "the request carried no If-None-Match header, so the GET was not \
             conditional and the upstream will send 3.6 MB on every poll. \
             Request head was:\n{}",
            heads[0]
        )
    });
    assert!(
        sent.contains("\"v1\""),
        "If-None-Match must carry the stored validator verbatim, got {sent:?}"
    );
}

#[tokio::test]
async fn a_first_poll_sends_no_validator() {
    // The other direction, and it is not symmetric decoration: sending a
    // fabricated or empty validator on a cold start would make the upstream
    // answer 304 against a store that holds nothing, and fusiform would sit
    // empty while reporting healthy polls.
    let upstream = Upstream::start(Answer::Body);
    let fetcher = Fetcher::new().expect("a fetcher");

    let outcome = poll(&fetcher, &upstream.endpoint(), None).await;
    assert!(
        matches!(outcome, FetchOutcome::Body { .. }),
        "a first poll must fetch a body, got {outcome:?}"
    );

    let heads = upstream.heads();
    assert!(
        header_line(&heads[0], "if-none-match").is_none(),
        "a poll with no stored validator must send no If-None-Match at all: an \
         empty or invented one invites a 304 against an empty store. Head was:\n{}",
        heads[0]
    );
}

#[tokio::test]
async fn a_304_carries_the_validator_forward() {
    // A 304 must not erase the ETag. If it did, every poll after the first
    // unchanged one would be unconditional — the same 3.6 MB waste, reached by
    // a different route, and equally invisible.
    let upstream = Upstream::start(Answer::NotModified);
    let fetcher = Fetcher::new().expect("a fetcher");

    let outcome = poll(&fetcher, &upstream.endpoint(), Some("\"v1\"")).await;
    match outcome {
        FetchOutcome::NotModified { etag, .. } => assert_eq!(
            etag.as_deref(),
            Some("\"v1\""),
            "a 304 must carry its validator forward"
        ),
        other => panic!("expected NotModified, got {other:?}"),
    }
}

#[tokio::test]
async fn a_304_without_an_etag_header_does_not_lose_the_validator() {
    // RFC 9110 says a 304 SHOULD carry the validator, which means a proxy or a
    // CDN edge may omit it. Fusiform reads the header off the response, so a
    // bare 304 yields None — and the store's `last_etag` selects the most recent
    // observation whose etag IS NOT NULL, so the validator survives in history
    // even when one response omits it.
    //
    // This test pins the fetch-layer half: a bare 304 is still NotModified
    // rather than an error, and it reports the absence honestly rather than
    // inventing a value.
    let upstream = Upstream::start(Answer::NotModifiedBare);
    let fetcher = Fetcher::new().expect("a fetcher");

    let outcome = poll(&fetcher, &upstream.endpoint(), Some("\"v1\"")).await;
    match outcome {
        FetchOutcome::NotModified { etag, .. } => assert!(
            etag.is_none(),
            "a 304 with no ETag header must report None rather than echoing the \
             request's validator: fusiform did not observe it, and inventing it \
             would claim the upstream confirmed a value it never sent"
        ),
        other => panic!("a bare 304 must still be NotModified, got {other:?}"),
    }

    assert_eq!(upstream.request_count(), 1);
}

#[tokio::test]
async fn the_validator_is_sent_verbatim_including_a_weak_prefix() {
    // A weak validator (`W/"..."`) is legal and models.dev could start emitting
    // one. Mangling it — stripping the prefix, re-quoting — produces a
    // validator the upstream does not recognise, which answers 200 forever:
    // correct data, silent waste, exactly the failure this file is about.
    let upstream = Upstream::start(Answer::NotModified);
    let fetcher = Fetcher::new().expect("a fetcher");

    let _ = poll(&fetcher, &upstream.endpoint(), Some("W/\"abc123\"")).await;

    let head = &upstream.heads()[0];
    let sent = header_line(head, "if-none-match").expect("the header must be present");
    assert!(
        sent.contains("W/\"abc123\""),
        "the validator must be sent byte-for-byte, got {sent:?}"
    );
}

/// The stored validator reaches the next request, through the real poll loop.
///
/// # This is the half the unit tests above cannot reach
///
/// Everything above drives `Fetcher::poll` directly with a validator the test
/// hands it. That proves the fetcher sends what it is given and proves nothing
/// about where production gets it from — the loop reads `last_etag` from the
/// store, and until now the only ungated coverage of that wiring was a test
/// asserting it is `None`.
///
/// So the mechanism was verified in two halves that never met: the fetcher
/// tested with a validator no store produced, and the store tested for a
/// validator no fetcher consumed. A change breaking the join reddens neither.
///
/// The live test covers it end to end and is `FUSIFORM_LIVE`-gated, which means
/// the production path is covered only when someone sets an environment
/// variable — the same "guard that runs when remembered" gap that let the
/// missing header survive.
#[tokio::test]
async fn the_stored_validator_reaches_the_next_request() {
    use fusiform_core::SourceId;
    use fusiform_module::loop_::PollContext;
    use fusiform_module::signals::Signals;
    use fusiform_store::CatalogStore;

    let upstream = Upstream::start(Answer::Body);
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&cortexkit_store_types::StorageDescriptor {
        module_id: "fusiform".to_string(),
        storage_namespace: "default".to_string(),
        isolation: cortexkit_store_types::Isolation::Module,
        backend: cortexkit_store_types::StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().to_string(),
        },
    })
    .unwrap();

    let signals = Arc::new(Signals::new());
    signals.store_opened();
    let ctx = PollContext::new(
        Arc::new(store),
        Fetcher::new().unwrap(),
        upstream.endpoint(),
        signals,
    );

    // First poll: nothing stored, so nothing conditional.
    tick(&ctx, 1_000).await.expect("the first poll must record");
    assert!(
        header_line(&upstream.heads()[0], "if-none-match").is_none(),
        "the first poll must be unconditional"
    );

    // The upstream now answers 304, which it may only legitimately do in
    // response to a validator it recognises.
    upstream.set(Answer::NotModified);
    tick(&ctx, 2_000)
        .await
        .expect("the second poll must record");

    let second = &upstream.heads()[1];
    let sent = header_line(second, "if-none-match").unwrap_or_else(|| {
        panic!(
            "the second poll sent no validator, so the ETag stored by the first \
             never reached the fetcher. Head was:\n{second}"
        )
    });
    assert!(
        sent.contains("\"v1\""),
        "the validator must be the one the first response carried, got {sent:?}"
    );

    // And the store kept it, so a third poll would be conditional too.
    assert_eq!(
        ctx.store.last_etag(SourceId::ModelsDev).unwrap().as_deref(),
        Some("\"v1\""),
        "a 304 must not erase the stored validator"
    );
}

/// A new process reads the whole document once before it trusts a validator
/// stored by the process before it.
///
/// A 304 only says the bytes are unchanged. After a deploy the running binary
/// has never read those bytes, and a build that extracts a new fact would get
/// 304 until the upstream happened to move, serving nothing new meanwhile. That
/// is what happened on 2026-09-25 to a build adding tier prices.
#[tokio::test]
async fn a_new_process_reads_the_document_before_trusting_a_stored_validator() {
    use fusiform_core::SourceId;
    use fusiform_module::loop_::PollContext;
    use fusiform_module::signals::Signals;
    use fusiform_store::CatalogStore;

    let upstream = Upstream::start(Answer::Body);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        CatalogStore::open(&cortexkit_store_types::StorageDescriptor {
            module_id: "fusiform".to_string(),
            storage_namespace: "default".to_string(),
            isolation: cortexkit_store_types::Isolation::Module,
            backend: cortexkit_store_types::StorageBackend::Sqlite {
                path: dir.path().join("store.db").to_string_lossy().to_string(),
            },
        })
        .unwrap(),
    );
    let process = || {
        let signals = Arc::new(Signals::new());
        signals.store_opened();
        PollContext::new(
            Arc::clone(&store),
            Fetcher::new().unwrap(),
            upstream.endpoint(),
            signals,
        )
    };

    // The previous process polls once and leaves a validator in the store.
    let before = process();
    tick(&before, 1_000)
        .await
        .expect("the first process must record");
    drop(before);

    // CONTROL: the validator really is stored, so the next assertion cannot
    // pass merely because there was nothing to send.
    assert_eq!(
        store.last_etag(SourceId::ModelsDev).unwrap().as_deref(),
        Some("\"v1\""),
        "the previous process must have stored a validator"
    );

    // A restart: same store, new process.
    let after = process();
    tick(&after, 2_000)
        .await
        .expect("the new process must record");
    assert!(
        header_line(&upstream.heads()[1], "if-none-match").is_none(),
        "a new process must read the whole document once before trusting a \
         stored validator. Head was:\n{}",
        upstream.heads()[1]
    );

    // Once it has read a body, it goes back to conditional polls.
    upstream.set(Answer::NotModified);
    tick(&after, 3_000)
        .await
        .expect("the third poll must record");
    let third = &upstream.heads()[2];
    assert!(
        header_line(third, "if-none-match").is_some_and(|v| v.contains("\"v1\"")),
        "after reading a body the process must poll conditionally again. Head \
         was:\n{third}"
    );
}
