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

use serde::{de::DeserializeOwned, Deserialize};

/// An unreadable member carries a default only so its readable siblings can
/// still be decoded. The normalizer must not serve that default as a claim.
#[derive(Debug, Clone)]
pub struct RawMember<T, const OBJECT: bool = false> {
    pub value: T,
    pub unreadable: bool,
}

impl<T: Default, const OBJECT: bool> Default for RawMember<T, OBJECT> {
    fn default() -> Self {
        Self {
            value: T::default(),
            unreadable: false,
        }
    }
}

impl<'de, T: Default + DeserializeOwned, const OBJECT: bool> Deserialize<'de>
    for RawMember<T, OBJECT>
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(deserializer)?;
        // Serde structs can also read tuple-shaped arrays. Upstream model
        // blocks have named members, so such arrays cannot be observations.
        if !OBJECT || raw.is_object() || raw.is_null() {
            if let Ok(value) = serde_json::from_value(raw) {
                return Ok(Self {
                    value,
                    unreadable: false,
                });
            }
        }
        Ok(Self {
            value: T::default(),
            unreadable: true,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawProvider {
    pub id: Option<String>,
    /// Published display label, carried into each model's `provider.name` fact.
    pub name: Option<String>,
    /// Documentation URL. Declared, not carried into the domain.
    pub doc: Option<String>,
    /// The provider's published API base URL, preserved verbatim.
    pub api: Option<String>,
    /// The credential environment variable a provider conventionally uses.
    /// Declared, not carried — credential-adjacent, so it is the last field
    /// that should sit unread in a serializable type.
    #[serde(default)]
    pub env: Vec<String>,
    /// The provider's published client package, preserved verbatim.
    pub npm: Option<String>,
    #[serde(default)]
    pub models: BTreeMap<String, RawMember<RawModel, true>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RawModel {
    pub id: RawMember<Option<String>>,
    pub name: RawMember<Option<String>>,
    pub family: RawMember<Option<String>>,
    pub canonical_model_id: RawMember<Option<String>>,
    pub status: RawMember<Option<String>>,
    #[serde(rename = "type")]
    pub model_type: RawMember<Option<String>>,
    pub release_date: RawMember<Option<String>>,
    pub last_updated: RawMember<Option<String>>,
    pub knowledge: RawMember<Option<String>>,
    pub open_weights: RawMember<Option<bool>>,
    pub reasoning: RawMember<Option<bool>>,
    /// Whether models.dev claims the model supports structured output. `None`
    /// means the upstream did not state a value; it is never defaulted to false.
    pub structured_output: RawMember<Option<bool>>,
    /// Whether the serving API accepts temperature; absence remains unknown.
    pub temperature: RawMember<Option<bool>>,
    /// The reasoning settings the model accepts, held as the upstream's JSON
    /// untouched. A pass-through on purpose: entry types fusiform has not seen
    /// and `null` elements inside `values` are real upstream data, and a typed
    /// struct would drop or reject them. `None` when the key is absent, which is
    /// a different claim from a published `[]`.
    pub reasoning_options: Option<serde_json::Value>,
    pub tool_call: RawMember<Option<bool>>,
    pub attachment: RawMember<Option<bool>>,
    pub limit: RawMember<Option<RawLimit>, true>,
    pub modalities: RawMember<Option<RawModalities>, true>,
    pub cost: RawMember<Option<RawCost>, true>,
    /// Per-model connection claims; other request overrides remain unread.
    pub provider: RawMember<Option<RawModelProvider>, true>,
    /// Named modes with their own rates and their own wire shaping.
    pub experimental: RawMember<Option<RawExperimental>, true>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RawModelProvider {
    pub npm: RawMember<Option<String>>,
    pub api: RawMember<Option<String>>,
    pub shape: RawMember<Option<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RawLimit {
    pub context: RawMember<Option<u64>>,
    pub output: RawMember<Option<u64>>,
    /// The maximum input the model accepts, when the upstream states it
    /// separately from the total.
    ///
    /// Read but NOT YET SERVED, and the distinction is the point. This field
    /// was measured on day one (the upstream's limit shape is
    /// `{context, output, input?}`, with zeros counted), and the
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
    pub input: RawMember<Option<u64>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawModalities {
    #[serde(default)]
    pub input: RawMember<Vec<String>>,
    #[serde(default)]
    pub output: RawMember<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawExperimental {
    #[serde(default)]
    pub modes: RawMember<BTreeMap<String, RawMode>, true>,
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
    /// Paths to unreadable cost members. Serving only the remaining rates
    /// could understate the price.
    pub unreadable_members: Vec<String>,
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
        let mut unreadable_members = Vec::new();

        for (key, value) in map {
            match key.as_str() {
                "tiers" => {
                    let serde_json::Value::Array(rows) = value else {
                        unreadable_members.push("cost.tiers".into());
                        continue;
                    };
                    for (index, row) in rows.into_iter().enumerate() {
                        let Ok(tier) = serde_json::from_value::<RawTier>(row) else {
                            unreadable_members.push(format!("cost.tiers[{index}]"));
                            continue;
                        };
                        unreadable_members.extend(
                            tier.unreadable_rates
                                .iter()
                                .map(|key| format!("cost.tiers[{index}].{key}")),
                        );
                        if tier.tier.as_ref().is_some_and(|spec| {
                            spec.tier_type.as_deref() == Some("context") && spec.size.is_none()
                        }) {
                            unreadable_members.push(format!("cost.tiers[{index}].tier.size"));
                        }
                        if tier.tier_unreadable {
                            unreadable_members.push(format!("cost.tiers[{index}].tier"));
                        }
                        tiers.push(tier);
                    }
                }
                "context_over_200k" => {
                    let serde_json::Value::Object(obj) = value else {
                        unreadable_members.push("cost.context_over_200k".into());
                        continue;
                    };
                    let mut legacy = BTreeMap::new();
                    for (key, value) in obj {
                        if let Some(number) = value.as_number() {
                            legacy.insert(key, number.to_string());
                        } else if is_known_rate(&key) {
                            unreadable_members.push(format!("cost.context_over_200k.{key}"));
                        }
                    }
                    context_over_200k = Some(legacy);
                }
                _ => {
                    if let Some(number) = value.as_number() {
                        scalars.insert(key, number.to_string());
                    } else if is_known_rate(&key) {
                        unreadable_members.push(format!("cost.{key}"));
                    }
                }
            }
        }

        RawCost {
            scalars,
            tiers,
            unreadable_members,
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

    /// Whether a context tier, or the legacy `context_over_200k` block,
    /// carries its own `input_audio` price.
    ///
    /// Tier rows parse only the known token keys, so this method checks those
    /// structures for an audio price that would otherwise be invisible to the
    /// normalizer.
    pub fn prices_input_audio_conditionally(&self) -> bool {
        self.prices_audio_conditionally("input_audio")
    }

    /// Output audio has no served tier selector either: keep the whole key
    /// unattributed when serving only its base would understate a tiered bill.
    pub fn prices_output_audio_conditionally(&self) -> bool {
        self.prices_audio_conditionally("output_audio")
    }

    fn prices_audio_conditionally(&self, key: &str) -> bool {
        self.tiers.iter().any(|tier| tier.scalars.contains_key(key))
            || self
                .context_over_200k
                .as_ref()
                .is_some_and(|legacy| legacy.contains_key(key))
    }
}

const KNOWN_TOKEN_KEYS: &[&str] = &["input", "output", "cache_read", "cache_write", "reasoning"];

fn is_known_rate(key: &str) -> bool {
    KNOWN_TOKEN_KEYS.contains(&key) || matches!(key, "input_audio" | "output_audio")
}

/// One pricing tier.
///
/// `tier.type` and `tier.size` are read together and never independently: the
/// size is a threshold only once the type says what it thresholds.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(from = "BTreeMap<String, serde_json::Value>")]
pub struct RawTier {
    pub scalars: BTreeMap<String, String>,
    pub tier: Option<RawTierSpec>,
    unreadable_rates: Vec<String>,
    tier_unreadable: bool,
}

impl From<BTreeMap<String, serde_json::Value>> for RawTier {
    fn from(map: BTreeMap<String, serde_json::Value>) -> Self {
        let mut scalars = BTreeMap::new();
        let mut tier = None;
        let mut unreadable_rates = Vec::new();
        let mut tier_unreadable = false;

        for (key, value) in map {
            if key == "tier" {
                if value.is_object() {
                    tier = serde_json::from_value::<RawTierSpec>(value).ok();
                }
                tier_unreadable = tier.is_none();
            } else if let Some(number) = value.as_number() {
                scalars.insert(key, number.to_string());
            } else if is_known_rate(&key) {
                unreadable_rates.push(key);
            }
        }

        RawTier {
            scalars,
            tier,
            unreadable_rates,
            tier_unreadable,
        }
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
