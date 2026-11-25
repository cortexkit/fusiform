//! Read the signed history day without retaining its array of releases.

use std::{fmt, io::Read};

use fusiform_release::{history_day_start_ms, Release};
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

pub(super) fn read(
    input: impl Read,
    day: &str,
    mut apply: impl FnMut(Release) -> Result<(), String>,
) -> Result<(), String> {
    let start = history_day_start_ms(day).ok_or("invalid history day date")?;
    let mut json = serde_json::Deserializer::from_reader(input);
    json.deserialize_map(Day {
        day,
        releases: Releases {
            start,
            apply: &mut apply,
        },
    })
    .and_then(|()| json.end())
    .map_err(|err| err.to_string())
}

struct Day<'a> {
    day: &'a str,
    releases: Releases<'a>,
}

impl<'de> Visitor<'de> for Day<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an attested history day")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut releases = Some(self.releases);
        let mut day = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "day" if day.is_none() => day = Some(map.next_value::<String>()?),
                "releases" if releases.is_some() => {
                    map.next_value_seed(releases.take().expect("releases parsed once"))?;
                }
                _ => {
                    return Err(de::Error::custom(format!(
                        "unexpected or duplicate day field {key:?}"
                    )))
                }
            }
        }
        if releases.is_some() || day.as_deref() != Some(self.day) {
            return Err(de::Error::custom(
                "day is empty or does not match its filename",
            ));
        }
        Ok(())
    }
}

struct Releases<'a> {
    start: i64,
    apply: &'a mut dyn FnMut(Release) -> Result<(), String>,
}

impl<'de> DeserializeSeed<'de> for Releases<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Releases<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array of signed releases")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut empty = true;
        while let Some(release) = seq.next_element::<Release>()? {
            empty = false;
            if release.envelope.release == 1 {
                return Err(de::Error::custom("release 1 belongs in history/base.json"));
            }
            if !(self.start..self.start + 86_400_000).contains(&release.envelope.published_at_ms) {
                return Err(de::Error::custom(
                    "release publication time is outside the requested UTC day",
                ));
            }
            (self.apply)(release).map_err(de::Error::custom)?;
        }
        if empty {
            return Err(de::Error::custom(
                "day is empty or does not match its filename",
            ));
        }
        Ok(())
    }
}
