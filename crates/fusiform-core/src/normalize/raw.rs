//! The upstream's shape, as measured — not as it ought to be.
//!
//! These types exist to be permissive at the boundary and strict afterwards.
//! Every field is optional because the measured document omits nearly all of
//! them somewhere, and a required field here would turn one provider's sparse
//! entry into a total ingest failure.
//!
//! Rate literals are held as [`serde_json::Number`] and read as their SOURCE
//! TEXT. The upstream publishes the same key as both int and float, and 151
//! numbers carry six or more decimals including IEEE-754 artifacts; parsing
//! them to f64 here would corrupt them before the money boundary ever sees
//! them. `serde_json`'s `arbitrary_precision` feature is what makes
//! `Number::as_str` return the literal, and it is enabled for exactly this.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct RawProvider {
    pub id: Option<String>,
    /// Display label. Declared so the upstream's shape is enumerated here, and
    /// deliberately NOT carried into the normalized type — see
    /// [`super::NormalizedProvider`] for why an unread field is a decision
    /// nobody made.
    pub name: Option<String>,
    /// Documentation URL. Declared, not carried, per `name`.
    pub doc: Option<String>,
    /// The provider's API base URL. Renderer-selection: it decides which host
    /// receives the request. Quarantined as a flag, never carried as a value.
    pub api: Option<String>,
    /// The credential environment variable a provider conventionally uses.
    /// Declared, not carried — credential-adjacent, so it is the last field
    /// that should sit unread in a serializable type.
    #[serde(default)]
    pub env: Vec<String>,
    /// Which SDK adapter speaks to this provider. Quarantined: this is the
    /// single most renderer-selecting fact in the document.
    pub npm: Option<String>,
    #[serde(default)]
    pub models: BTreeMap<String, RawModel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawModel {
    pub id: Option<String>,
    pub name: Option<String>,
    pub family: Option<String>,
    pub release_date: Option<String>,
    pub last_updated: Option<String>,
    pub knowledge: Option<String>,
    pub open_weights: Option<bool>,
    pub reasoning: Option<bool>,
    /// The reasoning settings the model accepts, held as the upstream's JSON
    /// untouched. A pass-through on purpose: entry types fusiform has not seen
    /// and `null` elements inside `values` are real upstream data, and a typed
    /// struct would drop or reject them. `None` when the key is absent, which is
    /// a different claim from a published `[]`.
    pub reasoning_options: Option<serde_json::Value>,
    pub tool_call: Option<bool>,
    pub attachment: Option<bool>,
    pub limit: Option<RawLimit>,
    pub modalities: Option<RawModalities>,
    pub cost: Option<RawCost>,
    /// A per-model override carrying literal headers and body parameters.
    /// Quarantined: this decides how a request is spoken.
    pub provider: Option<serde_json::Value>,
    /// Named modes with their own rates and their own wire shaping.
    pub experimental: Option<RawExperimental>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawLimit {
    pub context: Option<u64>,
    pub output: Option<u64>,
    /// The maximum input the model accepts, when the upstream states it
    /// separately from the total.
    ///
    /// Read but NOT YET SERVED, and the distinction is the point. This field
    /// was measured on day one (`docs/upstream-models-dev-measured.md` records
    /// the shape as `{context, output, input?}` and counts its zeros), and the
    /// struct shipped without it — so serde dropped it in silence on 1,199
    /// rows for three days.
    ///
    /// It is worth more than a missing number. On 470 rows
    /// `input + output == context` EXACTLY, including 18 first-party openai
    /// rows (`gpt-5`: 272,000 + 128,000 = 400,000). That is a model's window
    /// GEOMETRY stated in the payload — the same fact this project has been
    /// sourcing from provider documentation one page at a time.
    ///
    /// Not served yet because the name is not the meaning. `limit.output` was
    /// classified byte-affecting for two days on exactly that kind of reading,
    /// until BROCA walked their render path and found it is never rendered.
    /// Serving this needs a source for what the field MEANS, and whether the
    /// 470 sums are observed or computed by the upstream from the other two —
    /// which decides whether it is evidence or restatement.
    pub input: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawModalities {
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub output: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawExperimental {
    #[serde(default)]
    pub modes: BTreeMap<String, RawMode>,
}

/// One named mode under `experimental.modes`, reduced to its rate schedule.
///
/// A mode fuses two things: a `cost` block (a rate schedule that applies when
/// the request runs in this mode) and a `provider` block (literal request body
/// parameters and headers that switch the mode on). Only `cost` is kept.
/// `provider` is not a field here on purpose, so its bytes are dropped at the
/// parse boundary and nothing downstream can store or serve them: a catalog
/// that handed out header values and body parameters would be deciding how a
/// consumer's request is spoken.
///
/// Built from the mode's raw JSON rather than derived, so a mode whose shape is
/// wrong (not an object) is recorded as malformed instead of failing the whole
/// document: one bad mode must refuse that mode, not every model upstream.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(from = "serde_json::Value")]
pub struct RawMode {
    /// The mode's `cost` block, untyped, so the normalizer can refuse a wrong
    /// shape with a reported reason rather than serde dropping it. `None` when
    /// the mode publishes no `cost` at all, which is a mode with no price and
    /// produces no rate.
    pub cost: Option<serde_json::Value>,
    /// True when the mode itself was not a JSON object.
    pub malformed: bool,
}

impl From<serde_json::Value> for RawMode {
    fn from(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Object(mut obj) => RawMode {
                cost: obj.remove("cost"),
                malformed: false,
            },
            _ => RawMode {
                cost: None,
                malformed: true,
            },
        }
    }
}

/// A cost block, read as a flat map so unknown keys survive.
///
/// Deliberately NOT a struct of known fields. The audio keys arrive in the same
/// flat namespace as token rates with nothing to distinguish their unit, and a
/// typed struct would silently drop them — turning "fusiform cannot attribute
/// this charge" into "this charge does not exist", which is the more dangerous
/// of the two errors.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(from = "BTreeMap<String, serde_json::Value>")]
pub struct RawCost {
    /// Scalar rate keys, as literal source text.
    pub scalars: BTreeMap<String, String>,
    pub tiers: Vec<RawTier>,
    /// The older duplicate encoding of a context tier's rates.
    ///
    /// The key's NAME is not its threshold. Measured 2026-08-11 across the 288
    /// models carrying both encodings: the tier size is exactly 200,000 on only
    /// 126 of them, and is 272,000, 256,000, 262,144 or 512,000 on the rest.
    /// The key is a legacy label that stopped tracking the number it names.
    pub context_over_200k: Option<BTreeMap<String, String>>,
}

impl From<BTreeMap<String, serde_json::Value>> for RawCost {
    fn from(map: BTreeMap<String, serde_json::Value>) -> Self {
        let mut scalars = BTreeMap::new();
        let mut tiers = Vec::new();
        let mut context_over_200k = None;

        for (key, value) in map {
            match key.as_str() {
                "tiers" => {
                    if let Ok(parsed) = serde_json::from_value::<Vec<RawTier>>(value) {
                        tiers = parsed;
                    }
                }
                "context_over_200k" => {
                    context_over_200k = value.as_object().map(|obj| {
                        obj.iter()
                            .filter_map(|(k, v)| v.as_number().map(|n| (k.clone(), n.to_string())))
                            .collect()
                    });
                }
                _ => {
                    if let Some(number) = value.as_number() {
                        scalars.insert(key, number.to_string());
                    }
                }
            }
        }

        RawCost {
            scalars,
            tiers,
            context_over_200k,
        }
    }
}

impl RawCost {
    /// The literal text of a known token-rate key.
    pub fn token_rate(&self, key: &str) -> Option<&str> {
        self.scalars.get(key).map(|s| s.as_str())
    }

    /// Scalar cost keys this parser has no charge unit for.
    ///
    /// Anything that is not a known token class. Returned rather than ignored
    /// so the rate can be recorded as explicitly unpriced: a consumer must
    /// refuse to price it, which it can only do if it knows the key was there.
    pub fn unattributable_keys(&self) -> Vec<&String> {
        self.scalars
            .keys()
            .filter(|k| !KNOWN_TOKEN_KEYS.contains(&k.as_str()))
            .collect()
    }
}

const KNOWN_TOKEN_KEYS: &[&str] = &["input", "output", "cache_read", "cache_write", "reasoning"];

/// One pricing tier.
///
/// `tier.type` and `tier.size` are read together and never independently: the
/// size is a threshold only once the type says what it thresholds.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(from = "BTreeMap<String, serde_json::Value>")]
pub struct RawTier {
    pub scalars: BTreeMap<String, String>,
    pub tier: Option<RawTierSpec>,
}

impl From<BTreeMap<String, serde_json::Value>> for RawTier {
    fn from(map: BTreeMap<String, serde_json::Value>) -> Self {
        let mut scalars = BTreeMap::new();
        let mut tier = None;

        for (key, value) in map {
            if key == "tier" {
                tier = serde_json::from_value::<RawTierSpec>(value).ok();
            } else if let Some(number) = value.as_number() {
                scalars.insert(key, number.to_string());
            }
        }

        RawTier { scalars, tier }
    }
}

impl RawTier {
    pub fn token_rate(&self, key: &str) -> Option<&str> {
        self.scalars.get(key).map(|s| s.as_str())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawTierSpec {
    #[serde(rename = "type")]
    pub tier_type: Option<String>,
    pub size: Option<u64>,
}

#[cfg(test)]
mod tests {
    fn serializable_raw_derives(source: &str) -> Vec<&str> {
        source
            .lines()
            .filter(|line| line.trim_start().starts_with("#[derive("))
            .filter(|line| line.contains("Serialize"))
            .collect()
    }

    #[test]
    fn planted_serializable_raw_type_is_reported() {
        assert_eq!(
            serializable_raw_derives("#[derive(Deserialize, Serialize)]\nstruct RawLeaf;"),
            ["#[derive(Deserialize, Serialize)]"]
        );
    }

    /// The raw upstream layer can be read and cannot be written.
    ///
    /// # Why a negative property needs a test
    ///
    /// These types hold the upstream document verbatim, including the two
    /// fields fusiform must never emit: a model's `provider` override (literal
    /// headers and body parameters) and `experimental.modes` (per-mode request
    /// overrides). The override is a `serde_json::Value` passthrough, kept whole
    /// so the normalizer can flag its presence without interpreting it; a mode
    /// keeps only its `cost` block, but that block is still untyped upstream
    /// JSON until the normalizer has checked it.
    ///
    /// They derive `Deserialize` and not `Serialize`, so a quarantined value
    /// cannot be written back out. That is the strongest form of the
    /// quarantine: not "nothing serializes them today" but "nothing can".
    ///
    /// **It was true by accident until this test existed.** Nothing stopped
    /// someone adding `Serialize` to a derive line, and the reason to add one
    /// is entirely plausible — dumping a raw model to a file while debugging a
    /// normalizer finding. That single word would put a renderer-selecting
    /// override one `to_string` from any wire, and no reviewer would see a
    /// quarantine in a diff reading `#[derive(Debug, Clone, Deserialize,
    /// Serialize)]`.
    ///
    /// The module being private is a second layer, not a substitute: it stops a
    /// consumer serializing these types and does nothing about a serialization
    /// added inside this crate, which is where the plausible mistake lives.
    ///
    /// BROCA hit the mirror image on 2026-08-13: their `ProviderSpec` derives
    /// `Serialize` AND carries a raw passthrough, with the fence a narrowing
    /// one layer further in held by a comment. Same hazard, opposite structure
    /// — theirs fails only when someone widens the narrowing, which is a
    /// quieter moment than a diff.
    ///
    /// # Why this reads the source text
    ///
    /// The first version used autoref specialization to ask the type system
    /// whether each type implements `Serialize`. Its control — assert `String`
    /// reads as serializable — FAILED, so the technique was answering `false`
    /// unconditionally and every assertion built on it would have passed
    /// vacuously while proving nothing.
    ///
    /// The derive attribute is the artifact that would actually change, so this
    /// reads it. A text check on source is usually the weaker instrument; here
    /// it is the direct one, because the hazard is a word being added to a line.
    ///
    /// # A second, stronger property found while mutating this
    ///
    /// Adding `Serialize` to `RawModel` DOES NOT COMPILE: its fields are
    /// `RawLimit`, `RawModalities`, `RawExperimental` and `RawCost`, none of
    /// which are serializable either, so the compiler demands the whole tree.
    /// The types are mutually protective, and the plausible one-word mistake is
    /// only reachable on a leaf.
    ///
    /// That does not make this test redundant — it makes it the guard for the
    /// leaves, where the mistake is both possible and quiet. Verified by
    /// mutating `RawTierSpec`, a leaf of `String` fields: it compiles, and this
    /// test reddens naming the derive line.
    #[test]
    fn the_raw_layer_is_read_only() {
        let source = include_str!("raw.rs");

        let offenders = serializable_raw_derives(source);

        assert!(
            offenders.is_empty(),
            "a raw upstream type now derives Serialize, which makes the \
             quarantine a convention rather than a property — a `provider` \
             override or an `experimental` mode becomes one `to_string` from \
             any wire: {offenders:?}"
        );

        // The control. Without it this passes on an empty file, a moved file,
        // or a rename of the derive syntax — three ways to prove nothing while
        // looking green.
        let derives = source
            .lines()
            .filter(|line| line.trim_start().starts_with("#[derive("))
            .count();
        assert!(
            derives >= 8,
            "expected the raw layer's derive lines to be visible; found \
             {derives}, so this test is not reading what it thinks it is"
        );
        assert!(
            source.contains("pub provider: Option<serde_json::Value>"),
            "the quarantined provider override must still be in this file, or \
             this test is guarding something that moved"
        );
    }
}
