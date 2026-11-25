use std::{
    collections::BTreeMap,
    env::VarError,
    error::Error,
    ffi::OsString,
    fs::File,
    io::{self, Seek},
    path::Path,
    time::Duration,
};

use async_compression::tokio::bufread::GzipEncoder;
use fusiform_release::{Hash, Head, decode_release_public_key, verify_head};
use reqwest::{
    Body, Client, Response, StatusCode, Url,
    header::{AUTHORIZATION, CONTENT_ENCODING, HeaderValue},
    redirect::Policy,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::BufReader;
use tokio_util::io::ReaderStream;

const FILES: [(&str, &str); 3] = [
    ("release-1.json", "release-1.json"),
    ("state.json", "state.json"),
    ("base.json", "history/base.json"),
];

#[derive(Deserialize)]
struct Receipt {
    file: String,
    raw_bytes: u64,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitReceipt {
    release: u64,
}

pub async fn run(
    args: impl Iterator<Item = OsString>,
    token: Result<String, VarError>,
) -> Result<String, String> {
    let token = match token {
        Ok(token) if !token.trim().is_empty() => token,
        Err(VarError::NotUnicode(_)) => return Err("MODELS_IMPORT_TOKEN is not valid text".into()),
        _ => return Err("MODELS_IMPORT_TOKEN is required".into()),
    };
    run_authenticated(args, &token)
        .await
        .map(|reply| redact(reply, &token))
        .map_err(|error| redact(error.to_string(), &token))
}

fn redact(text: String, token: &str) -> String {
    if !text.contains(token) {
        return text;
    }
    let mut redacted = text.replace(token, "[redacted]");
    // A short token can also occur in the redaction marker. Suppress output
    // rather than echo an Authorization value in success or failure output.
    if redacted.contains(token) {
        redacted.clear();
    }
    redacted
}

async fn run_authenticated(
    mut args: impl Iterator<Item = OsString>,
    token: &str,
) -> Result<String, Box<dyn Error>> {
    let (Some(out_dir), Some(base), Some(kid_flag), Some(kid), Some(key_flag), Some(key), None) = (
        args.next(),
        args.next(),
        args.next(),
        args.next(),
        args.next(),
        args.next(),
        args.next(),
    ) else {
        return Err(USAGE.into());
    };
    if kid_flag != "--key-id" || key_flag != "--public-key" {
        return Err(USAGE.into());
    }
    let kid = kid.into_string().map_err(|_| "invalid key id")?;
    if kid.trim().is_empty() {
        return Err("--key-id is required".into());
    }
    let key = decode_release_public_key(key.to_str().ok_or("invalid public key")?)?;
    let mut base = Url::parse(base.to_str().ok_or("invalid base URL")?)?;
    let loopback_http =
        base.scheme() == "http" && matches!(base.host_str(), Some("127.0.0.1" | "localhost"));
    if base.scheme() != "https" && !loopback_http {
        return Err(
            "import base URL must use HTTPS (HTTP is allowed only on 127.0.0.1 or localhost)"
                .into(),
        );
    }
    if !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err("import base URL must not contain credentials, a query, or a fragment".into());
    }
    base.path_segments_mut()
        .map_err(|()| "invalid import base URL")?
        .pop_if_empty()
        .extend(["v1", "import", ""]);

    let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "invalid MODELS_IMPORT_TOKEN")?;
    auth.set_sensitive(true);
    let mut builder = Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(10 * 60));
    if loopback_http {
        builder = builder.no_proxy();
    }
    let client = builder.build()?;

    let mut hashes = BTreeMap::new();
    for (name, local_path) in FILES {
        let hash = upload(
            &client,
            &base,
            &auth,
            &Path::new(&out_dir).join(local_path),
            name,
        )
        .await
        .map_err(|error| format!("{name}: {error}"))?;
        hashes.insert(name, hash);
    }
    let expected = release_state_hash(&Path::new(&out_dir).join("release-1.json"))
        .map_err(|error| format!("release-1.json: {error}"))?;
    let response = client
        .post(base.join("commit")?)
        .header(AUTHORIZATION, auth)
        .json(&hashes)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    let receipt = checked_reply(response)
        .await
        .map_err(|error| format!("commit: {error}"))?;
    match serde_json::from_str(&receipt) {
        Ok(CommitReceipt { release: 1 }) => {}
        _ => return Err(format!("commit: expected {{\"release\":1}}, got {receipt}").into()),
    }

    let response = client
        .get(base.join("../head")?)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    let head: Head = serde_json::from_str(
        &checked_reply(response)
            .await
            .map_err(|error| format!("head: {error}"))?,
    )
    .map_err(|error| format!("invalid store head: {error}"))?;
    verify_head(&head.head, &head.signatures, &BTreeMap::from([(kid, key)]))
        .map_err(|error| format!("verify store head after commit: {error}"))?;
    if head.head.release.get() != 1
        || head.head.latest_snapshot != 1
        || head.head.state_hash != expected
    {
        return Err("store head does not confirm release 1, snapshot 1, and the state hash in release-1.json".into());
    }
    Ok(receipt)
}

const USAGE: &str =
    "usage: import-upload <out-dir> <base-url> --key-id <kid> --public-key <base64url>";

fn release_state_hash(path: &Path) -> Result<Hash, Box<dyn Error>> {
    #[derive(Deserialize)]
    struct Genesis {
        envelope: GenesisEnvelope,
    }

    #[derive(Deserialize)]
    struct GenesisEnvelope {
        release: u64,
        state_hash: Hash,
    }

    let release: Genesis = serde_json::from_reader(io::BufReader::new(File::open(path)?))?;
    if release.envelope.release != 1 {
        return Err("expected the envelope for release 1".into());
    }
    Ok(release.envelope.state_hash)
}

async fn upload(
    client: &Client,
    base: &Url,
    auth: &HeaderValue,
    path: &Path,
    name: &str,
) -> Result<String, Box<dyn Error>> {
    // Hash the raw file before streaming the gzip body. The receipt also catches
    // a file that changed between the two passes through the open file.
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let raw_bytes = io::copy(&mut file, &mut hash)?;
    let sha256 = format!("{:x}", hash.finalize());
    file.rewind()?;
    let gzip = GzipEncoder::new(BufReader::new(tokio::fs::File::from_std(file)));
    // Await the response even if the server refuses before reading the whole
    // body. A blocking reqwest upload waits for the body writer first and can
    // report a broken upload instead of the store's refusal.
    let response = client
        .put(base.join(&format!("files/{name}"))?)
        .header(AUTHORIZATION, auth.clone())
        .header(CONTENT_ENCODING, "gzip")
        .body(Body::wrap_stream(ReaderStream::new(gzip)))
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    let receipt: Receipt = serde_json::from_str(&checked_reply(response).await?)?;
    if receipt.file != name || receipt.raw_bytes != raw_bytes || receipt.sha256 != sha256 {
        return Err(
            "upload receipt does not match the local file name, raw size, or SHA-256".into(),
        );
    }
    Ok(sha256)
}

async fn checked_reply(response: Response) -> Result<String, Box<dyn Error>> {
    #[derive(Deserialize)]
    struct Refusal {
        error: String,
    }
    let status = response.status();
    let text = response.text().await.map_err(reqwest::Error::without_url)?;
    if status != StatusCode::OK {
        if let Ok(Refusal { error }) = serde_json::from_str(&text) {
            return Err(format!("{status}: {error}").into());
        }
        return Err(format!("{status}: {text}").into());
    }
    Ok(text)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
