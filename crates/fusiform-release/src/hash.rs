use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::{fmt, io, str::FromStr};

/// A SHA-256 digest. On the wire, it is 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hash([u8; 32]);

impl Hash {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn hex(&self) -> [u8; 64] {
        let digits = b"0123456789abcdef";
        let mut hex = [0; 64];
        for (i, &byte) in self.0.iter().enumerate() {
            hex[2 * i] = digits[(byte >> 4) as usize];
            hex[2 * i + 1] = digits[(byte & 15) as usize];
        }
        hex
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(str::from_utf8(&self.hex()).expect("hex is ASCII"))
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidHash;

impl fmt::Display for InvalidHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected a SHA-256 hash of 64 lowercase hex digits")
    }
}

impl std::error::Error for InvalidHash {}

impl FromStr for Hash {
    type Err = InvalidHash;

    fn from_str(hex: &str) -> Result<Self, Self::Err> {
        fn nibble(byte: u8) -> Result<u8, InvalidHash> {
            match byte {
                b'0'..=b'9' => Ok(byte - b'0'),
                b'a'..=b'f' => Ok(byte - b'a' + 10),
                _ => Err(InvalidHash),
            }
        }

        if hex.len() != 64 {
            return Err(InvalidHash);
        }
        let mut bytes = [0; 32];
        for (&[high, low], byte) in hex.as_bytes().as_chunks::<2>().0.iter().zip(&mut bytes) {
            *byte = nibble(high)? * 16 + nibble(low)?;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for Hash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Canonical JSON for values parsed with `serde_json`'s `arbitrary_precision`.
pub fn canonical_json(value: &Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_canonical_json(&mut bytes, value).expect("writing to a Vec cannot fail");
    bytes
}

/// Write canonical JSON without retaining the encoded document in memory.
pub fn write_canonical_json(mut out: impl io::Write, value: &Value) -> io::Result<()> {
    write_value(&mut out, value)
}

fn write_value(out: &mut impl io::Write, value: &Value) -> io::Result<()> {
    match value {
        Value::Null => out.write_all(b"null"),
        Value::Bool(true) => out.write_all(b"true"),
        Value::Bool(false) => out.write_all(b"false"),
        Value::Number(n) => {
            // Also normalize numbers built in memory, not just deserialized ones.
            let text = n.as_str();
            if let Some((mantissa, exponent)) = text.split_once(['e', 'E']) {
                out.write_all(mantissa.as_bytes())?;
                out.write_all(b"e")?;
                if !exponent.starts_with(['+', '-']) {
                    out.write_all(b"+")?;
                }
                out.write_all(exponent.as_bytes())
            } else if text == "-0" {
                out.write_all(b"0")
            } else {
                out.write_all(text.as_bytes())
            }
        }
        Value::String(text) => write_string(out, text),
        Value::Array(values) => {
            out.write_all(b"[")?;
            for (i, item) in values.iter().enumerate() {
                if i != 0 {
                    out.write_all(b",")?;
                }
                write_value(out, item)?;
            }
            out.write_all(b"]")
        }
        Value::Object(map) => {
            // Cargo feature unification can make serde_json maps insertion-ordered.
            let mut members: Vec<_> = map.iter().collect();
            members.sort_unstable_by_key(|&(key, _)| key);
            out.write_all(b"{")?;
            for (i, (key, value)) in members.into_iter().enumerate() {
                if i != 0 {
                    out.write_all(b",")?;
                }
                write_string(out, key)?;
                out.write_all(b":")?;
                write_value(out, value)?;
            }
            out.write_all(b"}")
        }
    }
}

fn write_string(out: &mut impl io::Write, text: &str) -> io::Result<()> {
    serde_json::to_writer(out, text).map_err(io::Error::other)
}

/// SHA-256 of bytes, used also for fixture file checksums.
pub fn sha256(bytes: &[u8]) -> Hash {
    Hash(Sha256::digest(bytes).into())
}

/// Hash a single upstream fact value.
pub fn fact_hash(value: &Value) -> Hash {
    let mut digest = DigestWriter::default();
    write_canonical_json(&mut digest, value).expect("writing to the hasher cannot fail");
    digest.finish()
}

/// Keys sort by provider, then model (provider-level `None` first), then fact.
pub type FactKeyRef<'a> = (&'a str, Option<&'a str>, &'a str);

/// Hash an unordered collection of unique upstream fact keys and hashes.
pub fn upstream_hash<'a>(facts: impl IntoIterator<Item = (FactKeyRef<'a>, Hash)>) -> Hash {
    let mut facts: Vec<_> = facts.into_iter().collect();
    facts.sort_unstable_by_key(|&(key, _)| key);
    upstream_hash_sorted(facts)
}

/// Hash facts already in key order without buffering or sorting their hashes.
pub fn upstream_hash_sorted<'a>(facts: impl IntoIterator<Item = (FactKeyRef<'a>, Hash)>) -> Hash {
    let mut digest = UpstreamHasher::default();
    for (key, hash) in facts {
        digest.add_sorted(key, hash);
    }
    digest.finish()
}

/// Incremental upstream hashing when facts arrive in key order.
#[derive(Default)]
pub struct UpstreamHasher(DigestWriter);

impl UpstreamHasher {
    /// Add a fact. Callers must supply unique keys in strictly ascending order.
    pub fn add_sorted(&mut self, key: FactKeyRef<'_>, hash: Hash) {
        write_fact_line(&mut self.0, key, hash).expect("writing to the hasher cannot fail");
    }

    pub fn finish(self) -> Hash {
        self.0.finish()
    }
}

fn write_fact_line(
    out: &mut impl io::Write,
    (provider, model, fact): FactKeyRef<'_>,
    hash: Hash,
) -> io::Result<()> {
    out.write_all(b"[")?;
    write_string(out, provider)?;
    out.write_all(b",")?;
    match model {
        Some(model) => write_string(out, model)?,
        None => out.write_all(b"null")?,
    }
    out.write_all(b",")?;
    write_string(out, fact)?;
    out.write_all(b",\"")?;
    out.write_all(&hash.hex())?;
    out.write_all(b"\"]\n")
}

#[derive(Default)]
pub(crate) struct DigestWriter(Sha256);

impl DigestWriter {
    pub(crate) fn finish(self) -> Hash {
        Hash(self.0.finalize().into())
    }
}

impl io::Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
