//! Native release measurement for the embedded seed. Run the built executable
//! under `/usr/bin/time -v` to exclude compilation. With `hash-diff`, measure
//! the upstream parse, canonical hash and comparison rather than the merge.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::io::{self, Write};

use fusiform_curate::{curate, Quality, RawFacts};
use fusiform_module::{curation, seed};
use serde::ser::{Serialize, SerializeMap, Serializer};
use sha2::{Digest, Sha256};

type Providers = BTreeMap<String, BTreeMap<String, RawFacts>>;

fn main() {
    match std::env::args().nth(1).as_deref() {
        None => {
            let curation = curation::embedded();
            let upstream = seed::upstream_state().expect("the embedded seed normalizes");
            let served = curate(upstream, &curation, &Quality::default());
            println!("curated {} models", black_box(served).models.len());
        }
        Some("hash-diff") => hash_diff_seed(),
        Some(arg) => panic!("unknown measurement {arg:?}"),
    }
}

fn hash_diff_seed() {
    let upstream = seed::upstream_state().expect("the embedded seed normalizes");
    let mut digest = HashWriter(Sha256::new());
    serde_json::to_writer(&mut digest, &Canonical(&upstream.models)).unwrap();
    let hash = digest.0.finalize();

    let mut previous = upstream.models.clone();
    let row = previous
        .values_mut()
        .next()
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    let existence = row.get_mut("existence").unwrap();
    assert_eq!(existence, "\"present\"");
    *existence = "\"absent\"".into();
    let changes = changed_facts(black_box(&upstream.models), black_box(&previous));
    assert_eq!(changes, 1);
    println!("upstream SHA-256 {hash:x}; {changes} changed fact");
}

// The two inputs have the same key set by construction; compare each stored
// JSON value without materializing another map or normalizing via floating point.
fn changed_facts(current: &Providers, previous: &Providers) -> usize {
    current
        .iter()
        .map(|(provider, models)| {
            let prior_models = &previous[provider];
            models
                .iter()
                .map(|(model, row)| {
                    let prior_row = &prior_models[model];
                    row.iter()
                        .filter(|(key, value)| prior_row[*key] != **value)
                        .count()
                })
                .sum::<usize>()
        })
        .sum()
}

struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Canonical<'a, T>(&'a T);

impl<T> Serialize for Canonical<'_, BTreeMap<String, T>>
where
    for<'a> Canonical<'a, T>: Serialize,
{
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in self.0 {
            map.serialize_entry(key, &Canonical(value))?;
        }
        map.end()
    }
}

impl Serialize for Canonical<'_, String> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // A Value sorts keys even inside an object-shaped fact. With
        // arbitrary_precision, decimal numbers never round-trip through f64.
        let value: serde_json::Value =
            serde_json::from_str(self.0).map_err(serde::ser::Error::custom)?;
        value.serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_hash_input_sorts_nested_keys_without_rounding() {
        let row = RawFacts::from([
            ("z".into(), "0".into()),
            ("a".into(), r#"{"y":1.2300,"b":9007199254740993}"#.into()),
        ]);
        assert_eq!(
            serde_json::to_string(&Canonical(&row)).unwrap(),
            r#"{"a":{"b":9007199254740993,"y":1.2300},"z":0}"#
        );
    }
}
