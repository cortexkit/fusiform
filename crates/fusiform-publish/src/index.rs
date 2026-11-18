use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use fusiform_release::{FactKeyRef, Hash, UpstreamHasher, upstream_hash_sorted};

const HEADER: &[u8] = b"CKFI\x01";
const HASH_SIZE: usize = 32;
const COUNT_SIZE: usize = 4;

/// Previous upstream keys and hashes, without any previous fact values.
#[derive(Debug, PartialEq, Eq)]
pub struct FactIndex {
    keys: Vec<String>,
    providers: Vec<IndexedProvider>,
    upstream_hash: Hash,
}

#[derive(Debug, PartialEq, Eq)]
struct IndexedProvider {
    pub id: String,
    pub models: Vec<IndexedModel>,
}

#[derive(Debug, PartialEq, Eq)]
struct IndexedModel {
    pub id: Option<String>,
    pub facts: Vec<IndexedFact>,
}

#[derive(Debug, PartialEq, Eq)]
struct IndexedFact {
    pub key: usize,
    pub hash: Hash,
}

impl FactIndex {
    pub fn model_count(&self) -> usize {
        self.providers
            .iter()
            .map(|provider| provider.models.len() - usize::from(provider.models[0].id.is_none()))
            .sum()
    }

    pub fn fact_count(&self) -> usize {
        self.providers
            .iter()
            .flat_map(|provider| &provider.models)
            .map(|model| model.facts.len())
            .sum()
    }

    pub const fn upstream_hash(&self) -> Hash {
        self.upstream_hash
    }

    pub(crate) fn facts(&self) -> impl Iterator<Item = (FactKeyRef<'_>, Hash)> {
        self.providers.iter().flat_map(|provider| {
            provider.models.iter().flat_map(|model| {
                model.facts.iter().map(|fact| {
                    (
                        (
                            provider.id.as_str(),
                            model.id.as_deref(),
                            self.keys[fact.key].as_str(),
                        ),
                        fact.hash,
                    )
                })
            })
        })
    }

    /// Encode an opaque, versioned blob. The host may split it for storage.
    pub fn to_bytes(&self) -> Vec<u8> {
        // Keys are interned across models, and digests stay binary to avoid
        // doubling their size in hex (the digests dominate the blob).
        let mut out = Vec::with_capacity(self.fact_count() * (COUNT_SIZE + HASH_SIZE));
        out.extend_from_slice(HEADER);
        out.extend_from_slice(self.upstream_hash.as_bytes());
        put_count(&mut out, self.keys.len());
        for key in &self.keys {
            put_text(&mut out, key);
        }
        put_count(&mut out, self.providers.len());
        for provider in &self.providers {
            put_text(&mut out, &provider.id);
            put_count(&mut out, provider.models.len());
            for model in &provider.models {
                if let Some(id) = &model.id {
                    out.push(1);
                    put_text(&mut out, id);
                } else {
                    out.push(0);
                }
                put_count(&mut out, model.facts.len());
                for fact in &model.facts {
                    put_count(&mut out, fact.key);
                    out.extend_from_slice(fact.hash.as_bytes());
                }
            }
        }
        out
    }

    /// Refuse malformed indexes and indexes that do not reproduce their hash.
    /// After a refusal, rebuild at the published head before polling. See
    /// [`Self::from_snapshot_upstream`] for the recovery sequence.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, IndexDecodeError> {
        let mut reader = Reader(
            bytes
                .strip_prefix(HEADER)
                .ok_or(IndexDecodeError::UnsupportedHeader)?,
        );
        let upstream_hash = reader.hash()?;
        let key_count = reader.count(COUNT_SIZE)?;
        let mut keys = Vec::with_capacity(key_count);
        for _ in 0..key_count {
            keys.push(reader.text()?);
        }
        if keys.iter().collect::<BTreeSet<_>>().len() != keys.len() {
            return Err(IndexDecodeError::Malformed);
        }

        let mut used_keys = vec![false; keys.len()];
        let provider_count = reader.count(2 * COUNT_SIZE)?;
        let mut providers = Vec::<IndexedProvider>::with_capacity(provider_count);
        for _ in 0..provider_count {
            let id = reader.text()?;
            if providers.last().is_some_and(|prior| prior.id >= id) {
                return Err(IndexDecodeError::Malformed);
            }
            let models = reader.models(&keys, &mut used_keys)?;
            if models.len() == 1 && models[0].id.is_none() {
                return Err(IndexDecodeError::Malformed);
            }
            providers.push(IndexedProvider { id, models });
        }
        if !reader.0.is_empty() || used_keys.contains(&false) {
            return Err(IndexDecodeError::Malformed);
        }

        let index = Self {
            keys,
            providers,
            upstream_hash,
        };
        if upstream_hash_sorted(index.facts()) != upstream_hash {
            return Err(IndexDecodeError::HashMismatch);
        }
        Ok(index)
    }
}

#[derive(Default)]
pub(crate) struct IndexBuilder {
    key_ids: BTreeMap<String, usize>,
    providers: Vec<IndexedProvider>,
    digest: UpstreamHasher,
}

impl IndexBuilder {
    pub(crate) fn add_sorted(&mut self, key: FactKeyRef<'_>, hash: Hash) {
        let (provider, model, fact) = key;
        if self.providers.last().is_none_or(|last| last.id != provider) {
            self.providers.push(IndexedProvider {
                id: provider.to_owned(),
                models: Vec::new(),
            });
        }
        let models = &mut self.providers.last_mut().unwrap().models;
        if models.last().is_none_or(|last| last.id.as_deref() != model) {
            models.push(IndexedModel {
                id: model.map(str::to_owned),
                facts: Vec::new(),
            });
        }
        let key_id = self.key_ids.get(fact).copied().unwrap_or_else(|| {
            let next_id = self.key_ids.len();
            self.key_ids.insert(fact.to_owned(), next_id);
            next_id
        });
        models
            .last_mut()
            .unwrap()
            .facts
            .push(IndexedFact { key: key_id, hash });
        self.digest.add_sorted(key, hash);
    }

    pub(crate) fn has_orphan_provider_facts(&self, provider: &str) -> bool {
        self.providers.last().is_some_and(|last| {
            last.id == provider && last.models.len() == 1 && last.models[0].id.is_none()
        })
    }

    pub(crate) fn finish(self) -> FactIndex {
        let mut keys = vec![String::new(); self.key_ids.len()];
        for (name, id) in self.key_ids {
            keys[id] = name;
        }
        FactIndex {
            keys,
            providers: self.providers,
            upstream_hash: self.digest.finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexDecodeError {
    UnsupportedHeader,
    Malformed,
    HashMismatch,
}

impl fmt::Display for IndexDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedHeader => "unrecognized fact index header or version",
            Self::Malformed => "malformed or truncated fact index",
            Self::HashMismatch => "fact index does not reproduce its stored upstream hash",
        })
    }
}

impl std::error::Error for IndexDecodeError {}

fn put_count(out: &mut Vec<u8>, count: usize) {
    let count = u32::try_from(count).expect("fact index fields fit in the wasm32 address space");
    out.extend_from_slice(&count.to_le_bytes());
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    put_count(out, text.len());
    out.extend_from_slice(text.as_bytes());
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], IndexDecodeError> {
        let (head, tail) = self
            .0
            .split_at_checked(n)
            .ok_or(IndexDecodeError::Malformed)?;
        self.0 = tail;
        Ok(head)
    }

    fn hash(&mut self) -> Result<Hash, IndexDecodeError> {
        Ok(Hash::from_bytes(self.take(HASH_SIZE)?.try_into().unwrap()))
    }

    fn word(&mut self) -> Result<usize, IndexDecodeError> {
        Ok(u32::from_le_bytes(self.take(COUNT_SIZE)?.try_into().unwrap()) as usize)
    }

    fn count(&mut self, min_item_size: usize) -> Result<usize, IndexDecodeError> {
        let n = self.word()?;
        // Bound allocations by what can actually fit in the remaining blob.
        if n > self.0.len() / min_item_size {
            return Err(IndexDecodeError::Malformed);
        }
        Ok(n)
    }

    fn text(&mut self) -> Result<String, IndexDecodeError> {
        let len = self.word()?;
        let bytes = self.take(len)?;
        str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| IndexDecodeError::Malformed)
    }

    fn models(
        &mut self,
        keys: &[String],
        used_keys: &mut [bool],
    ) -> Result<Vec<IndexedModel>, IndexDecodeError> {
        let count = self.count(1 + COUNT_SIZE)?;
        if count == 0 {
            return Err(IndexDecodeError::Malformed);
        }
        let mut models = Vec::<IndexedModel>::with_capacity(count);
        for _ in 0..count {
            let id = match self.take(1)?[0] {
                0 => None,
                1 => Some(self.text()?),
                _ => return Err(IndexDecodeError::Malformed),
            };
            if models.last().is_some_and(|prior| prior.id >= id) {
                return Err(IndexDecodeError::Malformed);
            }
            let facts = self.facts(keys, used_keys)?;
            models.push(IndexedModel { id, facts });
        }
        Ok(models)
    }

    fn facts(
        &mut self,
        keys: &[String],
        used_keys: &mut [bool],
    ) -> Result<Vec<IndexedFact>, IndexDecodeError> {
        let count = self.count(COUNT_SIZE + HASH_SIZE)?;
        if count == 0 {
            return Err(IndexDecodeError::Malformed);
        }
        let mut facts = Vec::<IndexedFact>::with_capacity(count);
        for _ in 0..count {
            let key = self.word()?;
            let name = keys.get(key).ok_or(IndexDecodeError::Malformed)?;
            if facts.last().is_some_and(|prior| &keys[prior.key] >= name) {
                return Err(IndexDecodeError::Malformed);
            }
            used_keys[key] = true;
            facts.push(IndexedFact {
                key,
                hash: self.hash()?,
            });
        }
        Ok(facts)
    }
}
