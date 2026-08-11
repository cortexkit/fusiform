//! Fetching an upstream document, and telling apart the ways it can go wrong.
//!
//! The fetcher's whole job is to return one of four outcomes honestly: the
//! document changed, it did not, the upstream said nothing changed, or nothing
//! was observed. The last is the one that matters most, because a failed poll
//! that gets recorded as a successful one narrows an observation window on the
//! strength of a connection timeout.
//!
//! # Conditional by default
//!
//! Every poll after the first sends `If-None-Match`. Measured on 2026-08-11:
//! models.dev answers a matching ETag with 304 and zero body bytes, against
//! 355 KB gzipped for a full response. At a 30-minute cadence that is the
//! difference between ~6 GB and a few megabytes a year, and it is also the
//! upstream's bandwidth rather than fusiform's.

use std::time::Duration;

use fusiform_core::{FailureClass, SourceId};

/// What one fetch produced.
#[derive(Debug)]
pub enum FetchOutcome {
    /// A 200 with a body. Whether it CHANGED anything is not decided here —
    /// that needs the normalized hash, which needs a parse, and this layer
    /// deliberately does not parse.
    Body {
        bytes: Vec<u8>,
        etag: Option<String>,
        duration: Duration,
    },
    /// The upstream matched the conditional request. No body, and a real
    /// observation: the content is confirmed unchanged.
    NotModified {
        etag: Option<String>,
        duration: Duration,
    },
    /// Nothing was observed.
    Failed {
        class: FailureClass,
        detail: String,
        duration: Duration,
    },
}

/// Where a source lives and how to ask it.
#[derive(Debug, Clone)]
pub struct SourceEndpoint {
    pub source: SourceId,
    pub url: String,
}

impl SourceEndpoint {
    pub fn models_dev() -> Self {
        Self {
            source: SourceId::ModelsDev,
            url: "https://models.dev/api.json".to_string(),
        }
    }
}

/// An HTTP client configured for polling catalogs.
pub struct Fetcher {
    client: reqwest::Client,
}

impl Fetcher {
    /// Build a fetcher.
    ///
    /// The timeout is a whole-request budget rather than a connect timeout: a
    /// stalled response body is the failure mode a polling loop actually hits,
    /// and a connect-only timeout leaves the loop hanging on a socket that
    /// opened and then went quiet. Ninety seconds is generous for a 3.6 MB
    /// document precisely so a timeout means something is wrong rather than
    /// that the upstream was briefly slow.
    pub fn new() -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(90))
            .user_agent(concat!("ck-fusiform/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client })
    }

    /// Poll one source, sending `If-None-Match` when an ETag is known.
    ///
    /// Never returns an error: every way this can fail is one of the outcomes,
    /// because a polling loop that has to distinguish "this returned Err" from
    /// "this returned Failed" will eventually treat one as the other.
    pub async fn poll(&self, endpoint: &SourceEndpoint, etag: Option<&str>) -> FetchOutcome {
        let started = std::time::Instant::now();
        let mut request = self.client.get(&endpoint.url);
        if let Some(etag) = etag {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => {
                // A timeout is a network failure for our purposes: nothing was
                // observed. Classifying it separately would suggest a different
                // response, and there isn't one.
                return FetchOutcome::Failed {
                    class: FailureClass::Network,
                    detail: e.to_string(),
                    duration: started.elapsed(),
                };
            }
        };

        let status = response.status();
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        if status == reqwest::StatusCode::NOT_MODIFIED {
            return FetchOutcome::NotModified {
                etag,
                duration: started.elapsed(),
            };
        }

        if !status.is_success() {
            return FetchOutcome::Failed {
                class: FailureClass::HttpStatus,
                detail: format!("upstream answered {status}"),
                duration: started.elapsed(),
            };
        }

        match response.bytes().await {
            Ok(bytes) => FetchOutcome::Body {
                bytes: bytes.to_vec(),
                etag,
                duration: started.elapsed(),
            },
            // The headers arrived and the body did not. Still a network
            // failure, and still nothing observed: a truncated document is not
            // a smaller catalog.
            Err(e) => FetchOutcome::Failed {
                class: FailureClass::Network,
                detail: format!("body read failed: {e}"),
                duration: started.elapsed(),
            },
        }
    }
}

/// Hash bytes for change detection.
///
/// blake3 rather than a cryptographic-strength choice with a slower
/// constant: nothing here is adversarial, the input is a document fusiform
/// just fetched, and the hash exists to answer "are these the same bytes".
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
