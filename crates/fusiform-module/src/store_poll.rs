//! Fetch and verify the published chain before offering it to the history store.

mod current;
mod history_day;
mod state;
#[cfg(test)]
mod tests;
mod translate;

use std::{
    fs::File,
    io::{BufReader, BufWriter, Seek, Write},
    sync::Arc,
    time::Duration,
};

use fusiform_release::{
    history_day_name, sha256, validate_history_files, verify_head, verify_history_attestation,
    Hash, Head, HeadBody, HistoryFile, Release, ReleaseEnvelope, Snapshot,
};
use fusiform_store::{CatalogStore, StoreRelease};
use reqwest::{StatusCode, Url};
use serde::de::DeserializeOwned;
use sha2::{Digest as _, Sha256};

use crate::{fetch::Fetcher, settings::ReleaseKeys, signals::Signals};
pub use current::VerifiedStoreHead;
pub use state::validate_judgments;
use state::Verified;

const HEAD_PATH: &str = "v1/head";
const HISTORY_BASE_PATH: &str = "v1/history/base.json";
pub(crate) const POLL_INTERVAL_MS: i64 = 60_000;
const INTERVAL: Duration = Duration::from_millis(POLL_INTERVAL_MS as u64);

struct Replay<'a> {
    cursor: Option<&'a StoreRelease>,
    current: Verified,
    last_observation: i64,
    arrivals: BufWriter<File>,
}

impl<'a> Replay<'a> {
    fn new(
        cursor: Option<&'a StoreRelease>,
        current: Verified,
        last_observation: i64,
    ) -> Result<Self, String> {
        Self::check_cursor(cursor, &current)?;
        let mut replay = Self {
            cursor,
            current,
            last_observation,
            arrivals: BufWriter::new(step("open release replay spool", tempfile::tempfile())?),
        };
        if cursor.is_none() {
            let first = translate::arrival(
                None,
                None,
                &replay.current,
                &[],
                true,
                &mut replay.last_observation,
            )?;
            step(
                "write release replay spool",
                first.spool(&mut replay.arrivals),
            )?;
        }
        Ok(replay)
    }

    fn check_cursor(cursor: Option<&StoreRelease>, verified: &Verified) -> Result<(), String> {
        let envelope = &verified.envelope;
        if cursor.is_some_and(|cursor| {
            cursor.release == envelope.release
                && (cursor.state_hash != envelope.state_hash.to_string()
                    || cursor.curation_hash != envelope.curation_hash.to_string())
        }) {
            return Err("published history does not match the last applied release".into());
        }
        Ok(())
    }

    fn advance(&mut self, release: Release, keys: &ReleaseKeys) -> Result<(), String> {
        let previous_observed_at_ms = self.current.envelope.upstream_observed_at_ms;
        let before = self.current.advance(&release, keys)?;
        Self::check_cursor(self.cursor, &self.current)?;
        if self
            .cursor
            .is_none_or(|cursor| self.current.envelope.release > cursor.release)
        {
            let arrival = translate::arrival(
                Some(previous_observed_at_ms),
                before.as_ref(),
                &self.current,
                &release.change.upstream,
                release.change.curation_set.is_some(),
                &mut self.last_observation,
            )?;
            step(
                "write release replay spool",
                arrival.spool(&mut self.arrivals),
            )?;
        }
        Ok(())
    }
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

    async fn attested_file(
        &self,
        path: &str,
        attested: &HistoryFile,
    ) -> Result<BufReader<File>, String> {
        let mut response = self.response(path).await?;
        let expected_size = response.content_length();
        let mut spool = step(path, tempfile::tempfile())?;
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        while let Some(chunk) = step(path, response.chunk().await)? {
            size += chunk.len() as u64;
            digest.update(&chunk);
            step(path, spool.write_all(&chunk))?;
        }
        if expected_size.is_some_and(|expected| expected != size) {
            return Err(format!(
                "{path}: response size does not match Content-Length"
            ));
        }
        if Hash::from_bytes(digest.finalize().into()) != attested.sha256 {
            return Err(format!(
                "{path}: history file does not match its attested hash"
            ));
        }
        step(path, spool.rewind())?;
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

    async fn suffix(
        &self,
        replay: &mut Replay<'_>,
        head: &HeadBody,
        progress: &impl Fn(),
    ) -> Result<bool, String> {
        for number in replay.current.envelope.release + 1..=head.release.get() {
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
                return Ok(false);
            }
            let response = step(&file, response)?;
            let bytes = step(&file, response.bytes().await)?;
            let release: Release = step(&file, serde_json::from_slice(&bytes))?;
            if release.envelope.release != number {
                return Err(format!("{file}: wrong release number"));
            }
            replay.advance(release, &self.keys)?;
            progress();
        }
        Ok(true)
    }

    async fn history<'a>(
        &self,
        snapshot_number: u64,
        store: &CatalogStore,
        cursor: Option<&'a StoreRelease>,
        progress: &impl Fn(),
    ) -> Result<(Replay<'a>, Option<BufReader<File>>), String> {
        let path = format!("v1/snapshots/{snapshot_number}");
        let snapshot = self.snapshot(snapshot_number).await?;
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
        progress();
        let (base, days) = validate_history_files(&attestation.body.history)?;
        let mut tables = if cursor.is_none() {
            Some(self.attested_file(HISTORY_BASE_PATH, base).await?)
        } else {
            None
        };
        let last_observation = match tables.as_mut() {
            Some(spool) => step(
                HISTORY_BASE_PATH,
                CatalogStore::history_observation_watermark(spool),
            )?,
            None => step(
                "read upstream observation watermark",
                store.latest_observation_id(),
            )?,
        };
        let mut replay = Replay::new(cursor, first, last_observation)?;

        for file in days {
            let path = format!("v1/history/{}", file.file);
            // validate_history_files already checked every day path and their order.
            let day = history_day_name(&file.file).ok_or("history day path is invalid")?;
            let input = self.attested_file(&path, file).await?;
            step(
                &path,
                history_day::read(input, day, |release| {
                    replay.advance(release, &self.keys)?;
                    progress();
                    Ok(())
                }),
            )?;
        }
        if replay.current.envelope.release != snapshot_number {
            return Err("snapshot history does not end at the attested release".into());
        }
        if replay.current.envelope != envelope {
            return Err("verified history does not reproduce the snapshot envelope".into());
        }
        Ok((replay, tables))
    }

    async fn snapshot(&self, number: u64) -> Result<Snapshot, String> {
        let path = format!("v1/snapshots/{number}");
        let snapshot: Snapshot = self.json(&path).await?;
        if snapshot.envelope.release != number {
            return Err(format!("{path}: envelope names a different release"));
        }
        Ok(snapshot)
    }

    /// Verify the store head and return the release now held locally.
    pub async fn poll(&mut self, store: &CatalogStore) -> Result<u64, String> {
        self.poll_with_progress(store, || {}).await
    }

    async fn poll_with_progress(
        &mut self,
        store: &CatalogStore,
        progress: impl Fn(),
    ) -> Result<u64, String> {
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
        let warm = if let Some(retained) = self.verified.as_ref().filter(|old| {
            cursor
                .as_ref()
                .is_some_and(|cursor| old.matches_cursor(cursor))
        }) {
            let last_observation = step(
                "read upstream observation watermark",
                store.latest_observation_id(),
            )?;
            let mut replay = Replay::new(cursor.as_ref(), retained.decode()?, last_observation)?;
            self.suffix(&mut replay, &head, &progress)
                .await?
                .then_some(replay)
        } else {
            None
        };
        let (replay, base) = match warm {
            Some(replay) => (replay, None),
            None => {
                let (mut replay, base) = if let Some(cursor) = cursor
                    .as_ref()
                    .filter(|cursor| cursor.release >= head.latest_snapshot)
                {
                    let snapshot = self.snapshot(head.latest_snapshot).await?;
                    let current = Verified::from_snapshot(snapshot, &self.keys)?;
                    let last_observation = step(
                        "read upstream observation watermark",
                        store.latest_observation_id(),
                    )?;
                    // Releases through the cursor rebuild the state and check
                    // its hashes; only newer releases enter the SQLite spool.
                    let replay = Replay::new(Some(cursor), current, last_observation)?;
                    progress();
                    (replay, None)
                } else {
                    // With no usable anchor, reproduce the signed archive so
                    // releases newer than the cursor still add their historical rows.
                    self.history(head.latest_snapshot, store, cursor.as_ref(), &progress)
                        .await?
                };
                if !self.suffix(&mut replay, &head, &progress).await? {
                    return Err("release beyond the snapshot is missing".into());
                }
                (replay, base)
            }
        };
        let Replay {
            current, arrivals, ..
        } = replay;
        check_head(&current.envelope, &head)?;
        let retained = Retained::encode(current)?;
        let mut arrivals = step("flush release replay spool", arrivals.into_inner())?;
        step("rewind release replay spool", arrivals.rewind())?;
        // All network and verification work finishes before the store's
        // single-writer lock is taken, so no download waits under it. SQLite
        // then imports the verified spool in one transaction.
        step(
            "commit verified store history",
            store.apply_spooled_store_releases(base, BufReader::new(arrivals), head.release.get()),
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
            signals.attempted(crate::route::now_ms());
            match self
                .poll_with_progress(&store, || signals.store_replay_progress(crate::route::now_ms()))
                .await
            {
                Ok(release) => {
                    if signals.store_applied_release() != Some(release) {
                        signals.record_store_applied_release(release);
                        signals.poll_recorded();
                        signals.adopt_from_store(&store, fusiform_core::SourceId::ModelsDev);
                    }
                    signals.store_head_checked(crate::route::now_ms());
                    signals.store_poll_result(None);
                    if let Some(installed) = install.take() {
                        installed();
                    }
                    uses_seed = false;
                }
                Err(error) if uses_seed => signals.store_poll_result(Some(format!(
                    "serving the built-in snapshot; the store was not reached: {error}"
                ))),
                Err(error) => signals.store_poll_result(Some(error)),
            }
        }
    }
}
