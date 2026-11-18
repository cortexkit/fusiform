//! Read the attested history format without retaining whole tables in memory.

use std::{collections::BTreeSet, fmt, io::Read};

use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use crate::STORE_HISTORY_TABLES;

pub(crate) enum Tables {
    Observations,
    Rest,
}

pub(crate) fn read(
    input: impl Read,
    selection: Tables,
    mut row: impl FnMut(&'static str, Map<String, Value>) -> rusqlite::Result<()>,
) -> serde_json::Result<()> {
    let mut json = serde_json::Deserializer::from_reader(input);
    let seed = Rows {
        selection,
        row: &mut row,
    };
    json.deserialize_map(Base(seed))?;
    json.end()
}

use serde::Deserializer as _;

type RowSink<'a> = &'a mut dyn FnMut(&'static str, Map<String, Value>) -> rusqlite::Result<()>;

struct Rows<'a> {
    selection: Tables,
    row: RowSink<'a>,
}

struct Base<'a>(Rows<'a>);

impl<'de> Visitor<'de> for Base<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a v1 history base")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut format = None;
        let mut release = None;
        let mut tables = false;
        let mut rows = Some(self.0);
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "format" if format.is_none() => format = Some(map.next_value::<String>()?),
                "release" if release.is_none() => release = Some(map.next_value::<u64>()?),
                "tables" if !tables => {
                    map.next_value_seed(rows.take().expect("tables parsed once"))?;
                    tables = true;
                }
                _ => {
                    return Err(de::Error::custom(format!(
                        "unexpected or duplicate history field {key:?}"
                    )))
                }
            }
        }
        if format.as_deref() != Some("ck-models-history-base/v1") || release != Some(1) || !tables {
            return Err(de::Error::custom(
                "wrong history format, release number or missing tables",
            ));
        }
        Ok(())
    }
}

impl<'de> DeserializeSeed<'de> for Rows<'_> {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Rows<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a map of catalog history tables")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut seen = BTreeSet::new();
        while let Some(table) = map.next_key::<String>()? {
            let table = STORE_HISTORY_TABLES
                .iter()
                .copied()
                .find(|&name| name == table)
                .ok_or_else(|| {
                    de::Error::custom(format!("unexpected catalog history table {table:?}"))
                })?;
            if !seen.insert(table) {
                return Err(de::Error::custom(format!(
                    "duplicate catalog history table {table:?}"
                )));
            }
            if (table == "observation") != matches!(self.selection, Tables::Observations) {
                map.next_value::<IgnoredAny>()?;
            } else {
                map.next_value_seed(Table {
                    table,
                    row: self.row,
                })?;
            }
        }
        // The nine original v1 tables are mandatory; tables for newer curation
        // dimensions may be absent from an older signed base.
        for &table in &STORE_HISTORY_TABLES[..9] {
            if !seen.contains(table) {
                return Err(de::Error::custom(format!(
                    "history base missing table {table}"
                )));
            }
        }
        Ok(())
    }
}

struct Table<'a> {
    table: &'static str,
    row: RowSink<'a>,
}

impl<'de> DeserializeSeed<'de> for Table<'_> {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Table<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array of rows in insertion order")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut prior = 0;
        while let Some(row) = seq.next_element::<Map<String, Value>>()? {
            prior = row
                .get("id")
                .and_then(Value::as_i64)
                .filter(|&id| id > prior)
                .ok_or_else(|| {
                    de::Error::custom(format!(
                        "{}: history ids must be positive and in insertion order",
                        self.table
                    ))
                })?;
            (self.row)(self.table, row).map_err(de::Error::custom)?;
        }
        Ok(())
    }
}
