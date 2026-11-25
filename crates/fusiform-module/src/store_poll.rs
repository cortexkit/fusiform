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
    history_day_name, validate_history_files, verify_head, verify_history_attestation, Hash, Head,
    HeadBody, HistoryFile, Release, ReleaseEnvelope, Snapshot,
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

enum ReplayError {
    Verification(String),
    Other(String),
}

impl From<ReplayError> for String {
    fn from(error: ReplayError) -> Self {
        match error {
            ReplayError::Verification(detail) | ReplayError::Other(detail) => detail,
        }
    }
}

impl From<String> for ReplayError {
    fn from(detail: String) -> Self {
        Self::Other(detail)
    }
}

type Progress = Arc<dyn Fn() + Send + Sync>;

struct Replay {
    cursor: Option<StoreRelease>,
    current: Verified,
    last_observation: i64,
    arrivals: BufWriter<File>,
}

impl Replay {
    fn new(
        cursor: Option<StoreRelease>,
        current: Verified,
        last_observation: i64,
    ) -> Result<Self, ReplayError> {
        Self::check_cursor(cursor.as_ref(), &current).map_err(ReplayError::Verification)?;
        let mut replay = Self {
            cursor,
            current,
            last_observation,
            arrivals: BufWriter::new(
                step("open release replay spool", tempfile::tempfile())
                    .map_err(ReplayError::Other)?,
            ),
        };
        if replay.cursor.is_none() {
            let first = translate::arrival(
                None,
                None,
                &replay.current,
                &[],
                true,
                &mut replay.last_observation,
            )
            .map_err(ReplayError::Other)?;
            step(
                "write release replay spool",
                first.spool(&mut replay.arrivals),
            )
            .map_err(ReplayError::Other)?;
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

    fn advance(&mut self, release: Release, keys: &ReleaseKeys) -> Result<(), ReplayError> {
        let previous_observed_at_ms = self.current.envelope.upstream_observed_at_ms;
        let before = self
            .current
            .advance(&release, keys)
            .map_err(ReplayError::Verification)?;
        Self::check_cursor(self.cursor.as_ref(), &self.current)
            .map_err(ReplayError::Verification)?;
        if self
            .cursor
            .as_ref()
            .is_none_or(|cursor| self.current.envelope.release > cursor.release)
        {
            let arrival = translate::arrival(
                Some(previous_observed_at_ms),
                before.as_ref(),
                &self.current,
                &release.change.upstream,
                release.change.curation_set.is_some(),
                &mut self.last_observation,
            )
            .map_err(ReplayError::Other)?;
            step(
                "write release replay spool",
                arrival.spool(&mut self.arrivals),
            )
            .map_err(ReplayError::Other)?;
        }
        Ok(())
    }
}

// Keep only the consumer's verified state between polls, without the heap
// overhead of the maps and individual facts in the decoded tree.
struct Retained {
    envelope: ReleaseEnvelope,
    bytes: Box<[u8]>,
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
        Ok(Self { envelope, bytes })
    }

    fn decode(&self) -> Result<Verified, String> {
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
    keys: Arc<ReleaseKeys>,
    verified: Option<Arc<Retained>>,
}

fn step<T, E: std::fmt::Display>(name: &str, value: Result<T, E>) -> Result<T, String> {
    value.map_err(|err| format!("{name}: {err}"))
}

async fn blocking<T: Send + 'static, E: From<String> + Send + 'static>(
    name: &str,
    work: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, E> {
    step(name, tokio::task::spawn_blocking(work).await).map_err(E::from)?
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

fn observation_watermark(store: &CatalogStore) -> Result<i64, String> {
    step(
        "read upstream observation watermark",
        store.latest_observation_id(),
    )
}

impl StorePoller {
    pub fn new(url: Url, keys: ReleaseKeys) -> Result<Self, reqwest::Error> {
        Ok(Self {
            fetcher: Fetcher::new()?,
            url,
            keys: Arc::new(keys),
            verified: None,
        })
    }

    async fn response(&self, name: &str) -> Result<reqwest::Response, String> {
        let url = step(name, self.url.join(name))?;
        // A signed head can reach this edge before its objects do. The fetcher
        // returns HTTP 404 as an error, so the next tick retries the same cursor.
        step(name, self.fetcher.store_response(url).await)
    }

    async fn json<T: DeserializeOwned + Send + 'static>(&self, name: &str) -> Result<T, String> {
        let bytes = step(name, self.response(name).await?.bytes().await)?;
        let path = name.to_owned();
        blocking(name, move || step(&path, serde_json::from_slice(&bytes))).await
    }

    async fn attested_file(
        &self,
        path: &str,
        attested: &HistoryFile,
    ) -> Result<BufReader<File>, String> {
        let mut response = self.response(path).await?;
        let expected_size = response.content_length();
        let mut spool = blocking(path, || {
            step("open history download spool", tempfile::tempfile())
        })
        .await?;
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        // Batch streamed chunks so hashing and writing do not occupy a Tokio
        // worker or create a blocking job for every network frame.
        let mut pending = Vec::new();
        while let Some(chunk) = step(path, response.chunk().await)? {
            size += chunk.len() as u64;
            pending.extend_from_slice(&chunk);
            if pending.len() >= 1024 * 1024 {
                (spool, digest, pending) = blocking(path, move || {
                    digest.update(&pending);
                    step("write history download spool", spool.write_all(&pending))?;
                    pending.clear();
                    Ok::<_, String>((spool, digest, pending))
                })
                .await?;
            }
        }
        if expected_size.is_some_and(|expected| expected != size) {
            return Err(format!(
                "{path}: response size does not match Content-Length"
            ));
        }
        let expected_hash = attested.sha256;
        let name = path.to_owned();
        blocking(path, move || {
            digest.update(&pending);
            if Hash::from_bytes(digest.finalize().into()) != expected_hash {
                return Err(format!(
                    "{name}: history file does not match its attested hash"
                ));
            }
            step(&name, spool.write_all(&pending))?;
            step(&name, spool.rewind())?;
            Ok(BufReader::new(spool))
        })
        .await
    }

    async fn head(&self) -> Result<HeadBody, String> {
        let Head { head, signatures } = self.json(HEAD_PATH).await?;
        let keys = Arc::clone(&self.keys);
        let head = blocking(HEAD_PATH, move || {
            step(
                &format!("{HEAD_PATH} signature"),
                verify_head(&head, &signatures, &keys),
            )?;
            Ok::<_, String>(head)
        })
        .await?;
        if i64::try_from(head.release.get()).is_err() {
            return Err(format!(
                "{HEAD_PATH}: release is outside the supported SQLite integer range"
            ));
        }
        Ok(head)
    }

    async fn suffix(
        &self,
        mut replay: Replay,
        head: &HeadBody,
        progress: &Progress,
    ) -> Result<Option<Replay>, ReplayError> {
        for number in replay.current.envelope.release + 1..=head.release.get() {
            let file = format!("v1/releases/{number}");
            let url = step(&file, self.url.join(&file)).map_err(ReplayError::Other)?;
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
            let response = step(&file, response).map_err(ReplayError::Other)?;
            let bytes = step(&file, response.bytes().await).map_err(ReplayError::Other)?;
            let keys = Arc::clone(&self.keys);
            let progress = Arc::clone(progress);
            let path = file.clone();
            replay = blocking(&file, move || {
                let release: Release = step(&path, serde_json::from_slice(&bytes))
                    .map_err(ReplayError::Verification)?;
                if release.envelope.release != number {
                    return Err(ReplayError::Verification(format!(
                        "{path}: wrong release number"
                    )));
                }
                replay.advance(release, &keys)?;
                progress();
                Ok(replay)
            })
            .await?;
        }
        check_head(&replay.current.envelope, head).map_err(ReplayError::Verification)?;
        Ok(Some(replay))
    }

    async fn history(
        &self,
        snapshot_number: u64,
        store: &Arc<CatalogStore>,
        cursor: Option<StoreRelease>,
        progress: &Progress,
    ) -> Result<(Replay, Option<BufReader<File>>), String> {
        let path = format!("v1/snapshots/{snapshot_number}");
        let snapshot = self.snapshot(snapshot_number).await?;
        let keys = Arc::clone(&self.keys);
        let snapshot_path = path.clone();
        // Keep only the signed envelope and file list; the replay builds state
        // without retaining a second copy of the decoded snapshot state.
        let (envelope, history) = blocking(&path, move || {
            state::verify_snapshot(&snapshot, &keys)?;
            let attestation = snapshot
                .history_attestation
                .ok_or_else(|| format!("{snapshot_path}: missing history_attestation"))?;
            step(
                "snapshot history attestation",
                verify_history_attestation(&attestation, &snapshot.envelope, &keys),
            )?;
            Ok::<_, String>((snapshot.envelope, attestation.body.history))
        })
        .await?;

        let first: Release = self.json("v1/releases/1").await?;
        if first.envelope.release != 1 {
            return Err("v1/releases/1: wrong release number".into());
        }
        let keys = Arc::clone(&self.keys);
        let stamp = Arc::clone(progress);
        let first = blocking("verify release 1", move || {
            let first = Verified::first(first, &keys)?;
            stamp();
            Ok::<_, String>(first)
        })
        .await?;
        let (base, days) = validate_history_files(&history)?;
        let tables = if cursor.is_none() {
            Some(self.attested_file(HISTORY_BASE_PATH, base).await?)
        } else {
            None
        };
        let store = Arc::clone(store);
        let (mut replay, tables) = blocking("start signed history replay", move || {
            let mut tables = tables;
            let last_observation = match tables.as_mut() {
                Some(spool) => step(
                    HISTORY_BASE_PATH,
                    CatalogStore::history_observation_watermark(spool),
                )?,
                None => observation_watermark(&store)?,
            };
            let replay = Replay::new(cursor, first, last_observation)?;
            Ok::<_, String>((replay, tables))
        })
        .await?;

        for file in days {
            let path = format!("v1/history/{}", file.file);
            // validate_history_files already checked every day path and their order.
            let day = history_day_name(&file.file)
                .ok_or("history day path is invalid")?
                .to_owned();
            let input = self.attested_file(&path, file).await?;
            let keys = Arc::clone(&self.keys);
            let progress = Arc::clone(progress);
            let history_path = path.clone();
            replay = blocking(&path, move || {
                step(
                    &history_path,
                    history_day::read(input, &day, |release| {
                        replay.advance(release, &keys)?;
                        progress();
                        Ok(())
                    }),
                )?;
                Ok::<_, String>(replay)
            })
            .await?;
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
    pub async fn poll(&mut self, store: &Arc<CatalogStore>) -> Result<u64, String> {
        self.poll_with_progress(store, || {}).await
    }

    async fn poll_with_progress(
        &mut self,
        store: &Arc<CatalogStore>,
        progress: impl Fn() + Send + Sync + 'static,
    ) -> Result<u64, String> {
        let head = self.head().await?;
        let cursor_store = Arc::clone(store);
        let cursor = blocking("read store cursor", move || {
            step("read store cursor", cursor_store.latest_store_release())
        })
        .await?;
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
        let progress: Progress = Arc::new(progress);
        let warm = if let Some((retained, cursor)) = self
            .verified
            .as_ref()
            .zip(cursor.as_ref())
            .filter(|(old, cursor)| old.matches_cursor(cursor))
        {
            let store = Arc::clone(store);
            let retained = Arc::clone(retained);
            let cursor = cursor.clone();
            let replay = blocking("start retained store replay", move || {
                let last_observation = observation_watermark(&store)?;
                let current = retained.decode().map_err(ReplayError::Verification)?;
                Replay::new(Some(cursor), current, last_observation)
            })
            .await;
            let warm = match replay {
                Ok(replay) => self.suffix(replay, &head, &progress).await,
                Err(error) => Err(error),
            };
            match warm {
                Ok(replay) => replay,
                Err(error) => {
                    // Keep the state on transport or disk failure. If
                    // verification fails, let the next poll rebuild it from
                    // a signed snapshot or archived history.
                    if matches!(error, ReplayError::Verification(_)) {
                        self.verified = None;
                    }
                    return Err(error.into());
                }
            }
        } else {
            None
        };
        let (replay, base) = match warm {
            Some(replay) => (replay, None),
            None => {
                let (replay, base) = if let Some(cursor) = cursor
                    .as_ref()
                    .filter(|cursor| cursor.release >= head.latest_snapshot)
                {
                    let snapshot = self.snapshot(head.latest_snapshot).await?;
                    let keys = Arc::clone(&self.keys);
                    let store = Arc::clone(store);
                    let cursor = cursor.clone();
                    let stamp = Arc::clone(&progress);
                    // Releases through the cursor rebuild the state and check
                    // its hashes; only newer releases enter the SQLite spool.
                    let replay = blocking("start snapshot replay", move || {
                        let current = Verified::from_snapshot(snapshot, &keys)?;
                        let last_observation = observation_watermark(&store)?;
                        let replay = Replay::new(Some(cursor), current, last_observation)?;
                        stamp();
                        Ok::<_, String>(replay)
                    })
                    .await?;
                    (replay, None)
                } else {
                    // With no usable anchor, reproduce the signed archive so
                    // releases newer than the cursor still add their historical rows.
                    self.history(head.latest_snapshot, store, cursor, &progress)
                        .await?
                };
                let replay = self
                    .suffix(replay, &head, &progress)
                    .await?
                    .ok_or("release beyond the snapshot is missing")?;
                (replay, base)
            }
        };
        let store = Arc::clone(store);
        let release = head.release.get();
        // All network and verification work finishes before the store's
        // single-writer lock is taken, so no download waits under it. SQLite
        // then imports the verified spool in one transaction.
        let retained = blocking("commit verified store history", move || {
            let Replay {
                current, arrivals, ..
            } = replay;
            let retained = Retained::encode(current)?;
            let mut arrivals = step("flush release replay spool", arrivals.into_inner())?;
            step("rewind release replay spool", arrivals.rewind())?;
            step(
                "commit verified store history",
                store.apply_spooled_store_releases(base, BufReader::new(arrivals), release),
            )?;
            Ok::<_, String>(retained)
        })
        .await?;
        self.verified = Some(Arc::new(retained));
        Ok(release)
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
            let progress = Arc::clone(&signals);
            match self
                .poll_with_progress(&store, move || {
                    progress.store_replay_progress(crate::route::now_ms())
                })
                .await
            {
                Ok(release) => {
                    if signals.store_applied_release() != Some(release) {
                        signals.record_store_applied_release(release);
                        signals.poll_recorded();
                        let (store, adopted) = (Arc::clone(&store), Arc::clone(&signals));
                        if let Err(error) = tokio::task::spawn_blocking(move || {
                            adopted.adopt_from_store(&store, fusiform_core::SourceId::ModelsDev);
                        })
                        .await
                        {
                            eprintln!("fusiform: could not adopt store health after poll: {error}");
                            signals.adoption_failed();
                        }
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
