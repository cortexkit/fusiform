use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{Date, Month};

use crate::{ArtifactRefusal, Hash, Release, Signatures, sha256, write_canonical_json};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryFile {
    pub file: String,
    pub sha256: Hash,
}

impl HistoryFile {
    /// Verify the downloaded file's exact bytes, not its re-encoded JSON.
    pub fn verify_bytes(&self, bytes: &[u8]) -> Result<(), ArtifactRefusal> {
        if sha256(bytes) == self.sha256 {
            Ok(())
        } else {
            Err(ArtifactRefusal::new(
                "history_file_hash_mismatch",
                format_args!(
                    "history file {:?} does not match its attested hash",
                    self.file
                ),
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryAttestationBody {
    pub release: NonZeroU64,
    pub state_hash: Hash,
    pub history: Vec<HistoryFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryAttestation {
    pub body: HistoryAttestationBody,
    pub signatures: Signatures,
}

/// Build one canonical day file from consecutive signed releases stored as
/// JSON Lines. The import is recorded separately in `history/base.json`.
pub fn history_day_bytes(day: &str, releases_jsonl: &[u8]) -> Result<Vec<u8>, ArtifactRefusal> {
    let start = day_start_ms(day).ok_or_else(|| {
        ArtifactRefusal::new("invalid_day", "expected a UTC day in YYYY-MM-DD form")
    })?;
    let releases = str::from_utf8(releases_jsonl)
        .map_err(|err| ArtifactRefusal::new("invalid_history_release", err))?;
    let mut out = format!("{{\"day\":\"{day}\",\"releases\":[").into_bytes();
    let mut previous: Option<u64> = None;
    for line in releases.lines() {
        let Release { envelope, .. } = serde_json::from_str(line)
            .map_err(|err| ArtifactRefusal::new("invalid_history_release", err))?;
        if envelope.release == 1 {
            return Err(ArtifactRefusal::new(
                "history_import_release",
                "release 1 belongs in history/base.json",
            ));
        }
        if previous.is_some() && envelope.parent != previous {
            return Err(ArtifactRefusal::new(
                "nonconsecutive_history",
                "history releases must be consecutive and in order",
            ));
        }
        if !(start..start + 86_400_000).contains(&envelope.published_at_ms) {
            return Err(ArtifactRefusal::new(
                "history_day_mismatch",
                "release publication time is outside the requested UTC day",
            ));
        }
        if previous.is_some() {
            out.push(b',');
        }
        // Canonicalize the stored release object itself, including signatures.
        let stored: Value = serde_json::from_str(line)
            .map_err(|err| ArtifactRefusal::new("invalid_history_release", err))?;
        write_canonical_json(&mut out, &stored).expect("writing to a Vec cannot fail");
        previous = Some(envelope.release);
    }
    if previous.is_none() {
        return Err(ArtifactRefusal::new(
            "empty_history",
            "no releases for the requested day",
        ));
    }
    out.extend_from_slice(b"]}");
    Ok(out)
}

fn day_start_ms(day: &str) -> Option<i64> {
    let [y0, y1, y2, y3, b'-', m0, m1, b'-', d0, d1] = *day.as_bytes() else {
        return None;
    };
    if ![y0, y1, y2, y3, m0, m1, d0, d1]
        .iter()
        .all(u8::is_ascii_digit)
    {
        return None;
    }
    let year = day[..4].parse().ok()?;
    let month = Month::try_from(day[5..7].parse::<u8>().ok()?).ok()?;
    let date = Date::from_calendar_date(year, month, day[8..].parse().ok()?).ok()?;
    Some(date.midnight().assume_utc().unix_timestamp() * 1000)
}
