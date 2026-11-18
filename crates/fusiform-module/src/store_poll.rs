//! Fetch and verify the published chain before offering it to the history store.

mod state;
#[cfg(test)]
mod tests;
mod translate;

use std::{sync::Arc, time::Duration};

use fusiform_release::{
    verify_head, verify_history_attestation, Head, HeadBody, Release, Snapshot,
};
use fusiform_store::{CatalogStore, ImportedTables};
use reqwest::Url;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;

use crate::{fetch::Fetcher, settings::ReleaseKeys, signals::Signals};
use state::Verified;

const DAY_MS: i64 = 86_400_000;
const INTERVAL: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryBase {
    format: String,
    release: u64,
    tables: ImportedTables,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryDay {
    day: String,
    releases: Vec<Release>,
}

pub struct StorePoller {
    fetcher: Fetcher,
    url: Url,
    keys: ReleaseKeys,
    verified: Option<Verified>,
}

fn step<T, E: std::fmt::Display>(name: &str, value: Result<T, E>) -> Result<T, String> {
    value.map_err(|err| format!("{name}: {err}"))
}

fn day_name(path: &str) -> Option<&str> {
    let day = path.strip_prefix("days/")?.strip_suffix(".json")?;
    (day.len() == 10
        && day.bytes().enumerate().all(|(i, byte)| {
            if matches!(i, 4 | 7) {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        }))
    .then_some(day)
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

    async fn bytes(&self, name: &str) -> Result<Vec<u8>, String> {
        let url = step(name, self.url.join(name))?;
        step(name, self.fetcher.fetch_store_file(url).await)
    }

    async fn json<T: DeserializeOwned>(&self, name: &str) -> Result<T, String> {
        step(name, serde_json::from_slice(&self.bytes(name).await?))
    }

    async fn head(&self) -> Result<HeadBody, String> {
        let Head { head, signatures } = self.json("head.json").await?;
        step(
            "head.json signature",
            verify_head(&head, &signatures, &self.keys),
        )?;
        if i64::try_from(head.release.get()).is_err() {
            return Err("head.json: release is outside the supported SQLite integer range".into());
        }
        Ok(head)
    }

    async fn history(
        &self,
        snapshot_number: u64,
        bootstrap: bool,
    ) -> Result<(Snapshot, Option<ImportedTables>, Vec<Release>), String> {
        let path = format!("snapshots/{snapshot_number}.json");
        let snapshot: Snapshot = self.json(&path).await?;
        if snapshot.envelope.release != snapshot_number {
            return Err(format!("{path}: envelope names a different release"));
        }
        state::verify_snapshot(&snapshot, &self.keys)?;
        let attestation = snapshot
            .history_attestation
            .as_ref()
            .ok_or_else(|| format!("{path}: missing history_attestation"))?;
        step(
            "snapshot history attestation",
            verify_history_attestation(attestation, &snapshot.envelope, &self.keys),
        )?;

        let first: Release = self.json("releases/1.json").await?;
        Verified::advance(None, &first, &self.keys)?;
        let (base, days) = attestation
            .body
            .history
            .split_first()
            .ok_or("history attestation is empty")?;
        if base.file != "base.json" {
            return Err("history attestation must start with base.json".into());
        }
        let tables = if bootstrap {
            let bytes = self.bytes("history/base.json").await?;
            step("history/base.json", base.verify_bytes(&bytes))?;
            let base: HistoryBase = step("history/base.json", serde_json::from_slice(&bytes))?;
            if base.release != 1 || base.format != "ck-models-history-base/v1" {
                return Err("history/base.json: wrong history format or release number".into());
            }
            for table in &fusiform_store::STORE_HISTORY_TABLES[..9] {
                if !base.tables.contains_key(*table) {
                    return Err(format!("history/base.json: missing table {table}"));
                }
            }
            Some(base.tables)
        } else {
            None
        };

        let mut releases = vec![first];
        let mut previous_day = "";
        for file in days {
            let day = day_name(&file.file)
                .filter(|day| *day > previous_day)
                .ok_or_else(|| {
                    format!(
                        "history attestation has an invalid or out-of-order path {:?}",
                        file.file
                    )
                })?;
            let path = format!("history/{}", file.file);
            let bytes = self.bytes(&path).await?;
            step(&path, file.verify_bytes(&bytes))?;
            let document: HistoryDay = step(&path, serde_json::from_slice(&bytes))?;
            if document.day != day || document.releases.is_empty() {
                return Err(format!(
                    "{path}: day is empty or does not match its filename"
                ));
            }
            let mut lines = Vec::new();
            for release in &document.releases {
                step(&path, serde_json::to_writer(&mut lines, release))?;
                lines.push(b'\n');
            }
            step(&path, fusiform_release::history_day_bytes(day, &lines))?;
            previous_day = day;
            releases.extend(document.releases);
        }
        Ok((snapshot, tables, releases))
    }

    pub async fn poll(&mut self, store: &CatalogStore) -> Result<(), String> {
        let head = self.head().await?;
        let cursor = step("read store cursor", store.latest_store_release())?;
        let applied = cursor.as_ref().map_or(0, |cursor| cursor.release);
        if applied > head.release.get() {
            return Err("head.json: store head moved backwards".into());
        }
        if applied == head.release.get() {
            if cursor
                .as_ref()
                .is_some_and(|cursor| cursor.state_hash == head.state_hash.to_string())
            {
                return Ok(());
            }
            return Err("head.json: same release has a different state hash".into());
        }
        let catch_up = self.verified.as_ref().is_some_and(|old| {
            head.published_at_ms
                .saturating_sub(old.envelope.published_at_ms)
                > DAY_MS
                && head.latest_snapshot > applied
        });
        let (snapshot, base, mut releases, mut current) = if self.verified.is_none() || catch_up {
            let (snapshot, base, releases) =
                self.history(head.latest_snapshot, applied == 0).await?;
            (Some(snapshot), base, releases, None)
        } else {
            (None, None, Vec::new(), self.verified.clone())
        };
        let mut last_observation = match &base {
            Some(tables) => tables
                .get("observation")
                .and_then(|rows| rows.last())
                .and_then(|row| row.get("id"))
                .and_then(Value::as_i64)
                .unwrap_or(0),
            None => step(
                "read upstream observation watermark",
                store.latest_observation_id(),
            )?,
        };
        let first_individual = snapshot
            .as_ref()
            .map_or(applied, |snapshot| snapshot.envelope.release);
        if releases
            .last()
            .is_some_and(|last| last.envelope.release != first_individual)
        {
            return Err("snapshot history does not end at the attested release".into());
        }
        for number in first_individual + 1..=head.release.get() {
            let file = format!("releases/{number}.json");
            let release: Release = self.json(&file).await?;
            if release.envelope.release != number {
                return Err(format!("{file}: wrong release number"));
            }
            releases.push(release);
        }

        let mut arrivals = Vec::new();
        for release in releases {
            let next = Verified::advance(current.as_ref(), &release, &self.keys)?;
            if let Some(snapshot) = &snapshot {
                if next.envelope.release == snapshot.envelope.release
                    && next.envelope != snapshot.envelope
                {
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
                    &release.change,
                    &mut last_observation,
                )?);
            }
            current = Some(next);
        }
        let current = current.ok_or("published chain did not yield a release")?;
        if current.envelope.release != head.release.get()
            || current.envelope.state_hash != head.state_hash
            || current.envelope.published_at_ms != head.published_at_ms
        {
            return Err("verified release does not match head.json".into());
        }
        step(
            "commit verified store history",
            store.apply_store_releases(base.as_ref(), &arrivals),
        )?;
        self.verified = Some(current);
        Ok(())
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
                Ok(()) => {
                    if let Some(installed) = install.take() {
                        installed();
                    }
                    uses_seed = false;
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
