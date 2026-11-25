//! Fetch and verify the published chain before offering it to the history store.

mod current;
mod state;
#[cfg(test)]
mod tests;
mod translate;

use std::{
    fs::File,
    io::{BufReader, Seek, Write},
    sync::Arc,
    time::Duration,
};

use fusiform_release::{
    sha256, validate_history_files, verify_head, verify_history_attestation, Hash, Head, HeadBody,
    HistoryFile, Release, ReleaseEnvelope, Snapshot,
};
use fusiform_store::{CatalogStore, StoreRelease};
use reqwest::{StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize};
use sha2::{Digest as _, Sha256};

use crate::{fetch::Fetcher, settings::ReleaseKeys, signals::Signals};
pub use current::VerifiedStoreHead;
pub use state::validate_judgments;
use state::Verified;

const HEAD_PATH: &str = "v1/head";
const HISTORY_BASE_PATH: &str = "v1/history/base.json";
const INTERVAL: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryDay {
    day: String,
    releases: Vec<Release>,
}

struct History {
    snapshot: ReleaseEnvelope,
    base: Option<BufReader<File>>,
    first: Verified,
    later: Vec<Release>,
}

// Keep only the consumer's verified state between polls, without the heap
// overhead of the maps and individual facts in the decoded tree.
struct Retained {
    envelope: ReleaseEnvelope,
    bytes: Box<[u8]>,
    hash: Hash,
}

impl Retained {
    fn matches_cursor(&self, cursor: &StoreRelease) -> bool {
        self.envelope.release == cursor.release
            && self.envelope.state_hash.to_string() == cursor.state_hash
            && self.envelope.curation_hash.to_string() == cursor.curation_hash
    }

    fn encode(Verified { envelope, state }: Verified) -> Result<Self, String> {
        let bytes =
            step("encode verified store state", serde_json::to_vec(&state))?.into_boxed_slice();
        let hash = sha256(&bytes);
        Ok(Self {
            envelope,
            bytes,
            hash,
        })
    }

    fn decode(&self) -> Result<Verified, String> {
        if sha256(&self.bytes) != self.hash {
            return Err("retained store state does not match its verified hash".into());
        }
        Ok(Verified {
            envelope: self.envelope.clone(),
            state: step(
                "decode verified store state",
                serde_json::from_slice(&self.bytes),
            )?,
        })
    }
}

pub struct StorePoller {
    fetcher: Fetcher,
    url: Url,
    keys: ReleaseKeys,
    verified: Option<Retained>,
}

fn step<T, E: std::fmt::Display>(name: &str, value: Result<T, E>) -> Result<T, String> {
    value.map_err(|err| format!("{name}: {err}"))
}

fn check_head(envelope: &ReleaseEnvelope, head: &HeadBody) -> Result<(), String> {
    if envelope.release != head.release.get()
        || envelope.state_hash != head.state_hash
        || envelope.published_at_ms != head.published_at_ms
    {
        return Err(format!("verified release does not match {HEAD_PATH}"));
    }
    Ok(())
}

impl StorePoller {
    pub fn new(url: Url, keys: ReleaseKeys) -> Result<Self, reqwest::Error> {
        Ok(Self {
            fetcher: Fetcher::new()?,
            url,
            keys,
            verified: None,
        })
    }

    async fn response(&self, name: &str) -> Result<reqwest::Response, String> {
        let url = step(name, self.url.join(name))?;
        // A signed head can reach this edge before its objects do. The fetcher
        // returns HTTP 404 as an error, so the next tick retries the same cursor.
        step(name, self.fetcher.store_response(url).await)
    }

    async fn json<T: DeserializeOwned>(&self, name: &str) -> Result<T, String> {
        let bytes = step(name, self.response(name).await?.bytes().await)?;
        step(name, serde_json::from_slice(&bytes))
    }

    async fn history_base(&self, attested: &HistoryFile) -> Result<BufReader<File>, String> {
        let mut response = self.response(HISTORY_BASE_PATH).await?;
        let expected_size = response.content_length();
        let mut spool = step(HISTORY_BASE_PATH, tempfile::tempfile())?;
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        while let Some(chunk) = step(HISTORY_BASE_PATH, response.chunk().await)? {
            size += chunk.len() as u64;
            digest.update(&chunk);
            step(HISTORY_BASE_PATH, spool.write_all(&chunk))?;
        }
        if expected_size.is_some_and(|expected| expected != size) {
            return Err(format!(
                "{HISTORY_BASE_PATH}: response size does not match Content-Length"
            ));
        }
        if Hash::from_bytes(digest.finalize().into()) != attested.sha256 {
            return Err(format!(
                "{HISTORY_BASE_PATH}: history file does not match its attested hash"
            ));
        }
        step(HISTORY_BASE_PATH, spool.rewind())?;
        Ok(BufReader::new(spool))
    }

    async fn head(&self) -> Result<HeadBody, String> {
        let Head { head, signatures } = self.json(HEAD_PATH).await?;
        step(
            &format!("{HEAD_PATH} signature"),
            verify_head(&head, &signatures, &self.keys),
        )?;
        if i64::try_from(head.release.get()).is_err() {
            return Err(format!(
                "{HEAD_PATH}: release is outside the supported SQLite integer range"
            ));
        }
        Ok(head)
    }

    async fn suffix(&self, from: u64, head: &HeadBody) -> Result<Option<Vec<Release>>, String> {
        let mut releases = Vec::new();
        for number in from + 1..=head.release.get() {
            let file = format!("v1/releases/{number}");
            let url = step(&file, self.url.join(&file))?;
            let response = self.fetcher.store_response(url).await;
            // Only an absent object covered by the snapshot can use the
            // archive. Malformed releases and other fetch failures stay errors.
            if number <= head.latest_snapshot
                && response.as_ref().is_err_and(|error| {
                    matches!(
                        error.status(),
                        Some(StatusCode::NOT_FOUND | StatusCode::GONE)
                    )
                })
            {
                return Ok(None);
            }
            let response = step(&file, response)?;
            let bytes = step(&file, response.bytes().await)?;
            let release: Release = step(&file, serde_json::from_slice(&bytes))?;
            if release.envelope.release != number {
                return Err(format!("{file}: wrong release number"));
            }
            releases.push(release);
        }
        Ok(Some(releases))
    }

    async fn history(&self, snapshot_number: u64, bootstrap: bool) -> Result<History, String> {
        let path = format!("v1/snapshots/{snapshot_number}");
        let snapshot: Snapshot = self.json(&path).await?;
        if snapshot.envelope.release != snapshot_number {
            return Err(format!("{path}: envelope names a different release"));
        }
        state::verify_snapshot(&snapshot, &self.keys)?;
        let Snapshot {
            envelope,
            history_attestation,
            state,
            ..
        } = snapshot;
        // The replay must reproduce the signed envelope, not a second live
        // copy of the snapshot's model facts.
        drop(state);
        let attestation = history_attestation
            .as_ref()
            .ok_or_else(|| format!("{path}: missing history_attestation"))?;
        step(
            "snapshot history attestation",
            verify_history_attestation(attestation, &envelope, &self.keys),
        )?;

        let first: Release = self.json("v1/releases/1").await?;
        if first.envelope.release != 1 {
            return Err("v1/releases/1: wrong release number".into());
        }
        let first = Verified::first(first, &self.keys)?;
        let (base, days) = validate_history_files(&attestation.body.history)?;
        let tables = if bootstrap {
            Some(self.history_base(base).await?)
        } else {
            None
        };

        let mut later = Vec::new();
        for file in days {
            let path = format!("v1/history/{}", file.file);
            let bytes = step(&path, self.response(&path).await?.bytes().await)?;
            step(&path, file.verify_bytes(&bytes))?;
            let document: HistoryDay = step(&path, serde_json::from_slice(&bytes))?;
            if file.file != format!("days/{}.json", document.day) || document.releases.is_empty() {
                return Err(format!(
                    "{path}: day is empty or does not match its filename"
                ));
            }
            let mut lines = Vec::new();
            for release in &document.releases {
                step(&path, serde_json::to_writer(&mut lines, release))?;
                lines.push(b'\n');
            }
            step(
                &path,
                fusiform_release::history_day_bytes(&document.day, &lines),
            )?;
            later.extend(document.releases);
        }
        if later
            .last()
            .map_or(first.envelope.release, |last| last.envelope.release)
            != snapshot_number
        {
            return Err("snapshot history does not end at the attested release".into());
        }
        Ok(History {
            snapshot: envelope,
            base: tables,
            first,
            later,
        })
    }

    /// Verify the store head and return the release now held locally.
    pub async fn poll(&mut self, store: &CatalogStore) -> Result<u64, String> {
        let head = self.head().await?;
        let cursor = step("read store cursor", store.latest_store_release())?;
        let applied = cursor.as_ref().map_or(0, |cursor| cursor.release);
        if applied > head.release.get() {
            return Err(format!("{HEAD_PATH}: store head moved backwards"));
        }
        if applied == head.release.get() {
            if cursor
                .as_ref()
                .is_some_and(|cursor| cursor.state_hash == head.state_hash.to_string())
            {
                return Ok(applied);
            }
            return Err(format!(
                "{HEAD_PATH}: same release has a different state hash"
            ));
        }
        let (mut current, suffix) = if let Some(retained) = self.verified.as_ref().filter(|old| {
            cursor
                .as_ref()
                .is_some_and(|cursor| old.matches_cursor(cursor))
        }) {
            let suffix = self.suffix(applied, &head).await?;
            let current = suffix.as_ref().map(|_| retained.decode()).transpose()?;
            (current, suffix)
        } else {
            (None, None)
        };
        let (snapshot, mut base, first, releases) = match suffix {
            Some(releases) => (None, None, None, releases),
            None => {
                let History {
                    snapshot,
                    base,
                    first,
                    mut later,
                } = self.history(head.latest_snapshot, applied == 0).await?;
                later.extend(
                    self.suffix(snapshot.release, &head)
                        .await?
                        .ok_or("release beyond the snapshot is missing")?,
                );
                (Some(snapshot), base, Some(first), later)
            }
        };
        let mut last_observation = match &mut base {
            Some(spool) => step(
                HISTORY_BASE_PATH,
                CatalogStore::history_observation_watermark(spool),
            )?,
            None => step(
                "read upstream observation watermark",
                store.latest_observation_id(),
            )?,
        };
        let mut arrivals = Vec::new();
        for entry in first
            .into_iter()
            .map(Ok)
            .chain(releases.into_iter().map(Err))
        {
            let (next, incremental) = match entry {
                Ok(first) => (first, None),
                Err(release) => {
                    let previous = current.as_ref().ok_or("published chain lacks an anchor")?;
                    (
                        Verified::advance(previous, &release, &self.keys)?,
                        Some(release.change),
                    )
                }
            };
            if let Some(snapshot) = &snapshot {
                if next.envelope.release == snapshot.release && next.envelope != *snapshot {
                    return Err("verified history does not reproduce the snapshot envelope".into());
                }
            }
            if let Some(cursor) = cursor
                .as_ref()
                .filter(|cursor| cursor.release == next.envelope.release)
            {
                if cursor.state_hash != next.envelope.state_hash.to_string()
                    || cursor.curation_hash != next.envelope.curation_hash.to_string()
                {
                    return Err("published history does not match the last applied release".into());
                }
            }
            if next.envelope.release > applied {
                arrivals.push(translate::arrival(
                    current.as_ref(),
                    &next,
                    incremental
                        .as_ref()
                        .map_or(&[], |change| change.upstream.as_slice()),
                    incremental
                        .as_ref()
                        .is_none_or(|change| change.curation_set.is_some()),
                    &mut last_observation,
                )?);
            }
            current = Some(next);
        }
        let current = current.ok_or("published chain did not yield a release")?;
        check_head(&current.envelope, &head)?;
        let retained = Retained::encode(current)?;
        step(
            "commit verified store history",
            match base {
                Some(spool) => store.apply_streamed_store_releases(spool, &arrivals),
                None => store.apply_store_releases(None, &arrivals),
            },
        )?;
        self.verified = Some(retained);
        Ok(head.release.get())
    }

    pub async fn run(
        mut self,
        store: Arc<CatalogStore>,
        signals: Arc<Signals>,
        installed: impl FnOnce(),
        mut uses_seed: bool,
    ) {
        let mut ticks = tokio::time::interval(INTERVAL);
        let mut install = Some(installed);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            match self.poll(&store).await {
                Ok(release) => {
                    if let Some(installed) = install.take() {
                        installed();
                    }
                    uses_seed = false;
                    signals.record_store_applied_release(release);
                    signals.store_poll_result(None);
                }
                Err(error) if uses_seed => signals.store_poll_result(Some(format!(
                    "serving the built-in snapshot; the store was not reached: {error}"
                ))),
                Err(error) => signals.store_poll_result(Some(error)),
            }
        }
    }
}
