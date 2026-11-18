use std::{
    collections::{BTreeMap, btree_map::Entry},
    fmt,
    io::{BufReader, Read},
    marker::PhantomData,
};

use fusiform_release::{Hash, fact_hash};
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};
use serde_json::Value;

use crate::{FactIndex, SnapshotIndexError, index::IndexBuilder, recovery::check_models};

impl FactIndex {
    /// Rebuild from a JSON reader positioned at the snapshot's `state.upstream`
    /// document. Only a single fact value is decoded at a time. The input may
    /// be in any key order, but duplicate provider, model and fact keys are
    /// refused rather than resolving them according to parser-specific rules.
    /// Follow the same replay and head verification sequence as with
    /// [`Self::from_snapshot_upstream`].
    pub fn from_snapshot_upstream_reader(reader: impl Read) -> Result<Self, SnapshotReadError> {
        let UniqueMap(providers) =
            serde_json::from_reader::<_, UniqueMap<HashedProvider>>(BufReader::new(reader))
                .map_err(SnapshotReadError::Json)?;

        let mut builder = IndexBuilder::default();
        for (
            provider,
            HashedProvider {
                provider: UniqueMap(facts),
                models: UniqueMap(models),
            },
        ) in providers
        {
            check_models(
                &provider,
                models
                    .iter()
                    .map(|(model, UniqueMap(facts))| (model.as_str(), facts.is_empty())),
            )
            .map_err(SnapshotReadError::InvalidUpstream)?;

            for (key, HashedFact(hash)) in facts {
                builder.add_sorted((&provider, None, &key), hash);
            }
            for (model, UniqueMap(facts)) in models {
                for (key, HashedFact(hash)) in facts {
                    builder.add_sorted((&provider, Some(&model), &key), hash);
                }
            }
        }
        Ok(builder.finish())
    }
}

#[derive(Debug)]
pub enum SnapshotReadError {
    Json(serde_json::Error),
    InvalidUpstream(SnapshotIndexError),
}

impl fmt::Display for SnapshotReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(err) => write!(f, "cannot decode snapshot upstream: {err}"),
            Self::InvalidUpstream(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for SnapshotReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(err) => Some(err),
            Self::InvalidUpstream(err) => Some(err),
        }
    }
}

#[derive(Deserialize)]
struct HashedProvider {
    provider: UniqueMap<HashedFact>,
    models: UniqueMap<UniqueMap<HashedFact>>,
}

struct HashedFact(Hash);

impl<'de> Deserialize<'de> for HashedFact {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Ok(Self(fact_hash(&value)))
    }
}

// The map sorts decoded keys while retaining just digests and their identities.
struct UniqueMap<V>(BTreeMap<String, V>);

impl<'de, V: Deserialize<'de>> Deserialize<'de> for UniqueMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueMapVisitor<V>(PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for UniqueMapVisitor<V> {
            type Value = UniqueMap<V>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object with unique keys")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut unique = BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    match unique.entry(key) {
                        Entry::Occupied(entry) => {
                            return Err(serde::de::Error::custom(format_args!(
                                "duplicate key {:?}",
                                entry.key()
                            )));
                        }
                        Entry::Vacant(entry) => {
                            entry.insert(map.next_value()?);
                        }
                    }
                }
                Ok(UniqueMap(unique))
            }
        }

        deserializer.deserialize_map(UniqueMapVisitor(PhantomData))
    }
}
