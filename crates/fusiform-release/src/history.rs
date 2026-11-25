use std::{collections::BTreeMap, num::NonZeroU64};

use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};
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

/// Split a history list into its base and strictly ordered day files.
/// This validates paths only; consumers must also verify the file contents.
pub fn validate_history_files(
    files: &[HistoryFile],
) -> Result<(&HistoryFile, &[HistoryFile]), String> {
    let (base, days) = files.split_first().ok_or("history attestation is empty")?;
    if base.file != "base.json" {
        return Err("history attestation must start with base.json".into());
    }
    let mut previous_day = "";
    for file in days {
        previous_day = history_day_name(&file.file)
            .filter(|day| *day > previous_day)
            .ok_or_else(|| {
                format!(
                    "history attestation has an invalid or out-of-order path {:?}",
                    file.file
                )
            })?;
    }
    Ok((base, days))
}

/// The `YYYY-MM-DD` day named by a `days/<day>.json` history path, if well formed.
pub fn history_day_name(path: &str) -> Option<&str> {
    let day = path.strip_prefix("days/")?.strip_suffix(".json")?;
    history_day_start_ms(day).map(|_| day)
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
    let start = history_day_start_ms(day).ok_or_else(|| {
        ArtifactRefusal::new("invalid_day", "expected a UTC day in YYYY-MM-DD form")
    })?;
    let releases = str::from_utf8(releases_jsonl)
        .map_err(|err| ArtifactRefusal::new("invalid_history_release", err))?;
    let mut out = format!("{{\"day\":\"{day}\",\"releases\":[").into_bytes();
    let mut previous: Option<u64> = None;
    for line in releases.lines() {
        let Release {
            envelope, change, ..
        } = serde_json::from_str(line)
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
        if change.change_hash() != envelope.change_hash {
            return Err(ArtifactRefusal::new(
                "history_change_mismatch",
                "change_hash does not match the release's change",
            ));
        }
        drop(change);
        if previous.is_some() {
            out.push(b',');
        }
        write_stored_release(&mut out, line)
            .map_err(|err| ArtifactRefusal::new("invalid_history_release", err))?;
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

// Preserve extensions in the stored object without building a second tree of
// its change. Borrow the large upstream array and canonicalize one op at a time.
fn write_stored_release(out: &mut Vec<u8>, line: &str) -> serde_json::Result<()> {
    write_raw_object(out, line, |out, key, value| {
        if key != "change" {
            return write_raw_value(out, value);
        }
        write_raw_object(out, value.get(), |out, key, value| {
            if key != "upstream" {
                return write_raw_value(out, value);
            }
            let ops: Vec<&RawValue> = serde_json::from_str(value.get())?;
            out.push(b'[');
            for (i, op) in ops.into_iter().enumerate() {
                if i != 0 {
                    out.push(b',');
                }
                write_raw_value(out, op)?;
            }
            out.push(b']');
            Ok(())
        })
    })
}

fn write_raw_object(
    out: &mut Vec<u8>,
    raw: &str,
    mut write_member: impl FnMut(&mut Vec<u8>, &str, &RawValue) -> serde_json::Result<()>,
) -> serde_json::Result<()> {
    // Serde also accepts a struct as an array. Keep the archive spelling
    // instead of rewriting one of those releases into an object.
    if !raw.trim_start().starts_with('{') {
        let value: Value = serde_json::from_str(raw)?;
        return write_canonical_json(out, &value).map_err(serde_json::Error::io);
    }
    let members: BTreeMap<String, &RawValue> = serde_json::from_str(raw)?;
    out.push(b'{');
    for (i, (key, value)) in members.into_iter().enumerate() {
        if i != 0 {
            out.push(b',');
        }
        serde_json::to_writer(&mut *out, &key)?;
        out.push(b':');
        write_member(out, &key, value)?;
    }
    out.push(b'}');
    Ok(())
}

fn write_raw_value(out: &mut Vec<u8>, raw: &RawValue) -> serde_json::Result<()> {
    let value: Value = serde_json::from_str(raw.get())?;
    write_canonical_json(out, &value).expect("writing to a Vec cannot fail");
    Ok(())
}

/// Start of a history day in Unix milliseconds. Invalid UTC dates have no start.
pub fn history_day_start_ms(day: &str) -> Option<i64> {
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
