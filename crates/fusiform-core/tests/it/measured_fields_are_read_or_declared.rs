//! Inventory the embedded api.json bytes, not a hand-maintained description of
//! them. Serde ignores unknown fields, so a new upstream key otherwise looks
//! exactly like a field upstream never published.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

const SEED: &[u8] = include_bytes!("../../../fusiform-module/data/models-dev-seed.json");
const RAW: &str = include_str!("../../src/normalize/raw.rs");

/// Values deliberately not interpreted or served. A quarantine may still record
/// presence, but none of its contents are a model fact. Prefixes cover the whole
/// unread subtree, including arbitrary request headers and body parameters.
const DECLARED_UNREAD: &[(&str, &str)] = &[
    ("model.description", "human-facing prose, not a served capacity, price or capability"),
    ("model.type", "sparse model-kind labels with no consumer contract; not served"),
    ("model.provider", "SDK adapter and endpoint overrides select request rendering and destination; only presence is quarantined"),
    ("model.temperature", "request knob acceptance, not a model capacity; no served consumer contract"),
    ("model.interleaved", "bool or field-selector object whose request-shaping meaning is unestablished"),
    ("model.experimental.modes.*.provider", "request body and header overrides that switch modes on; dropped at the parse boundary"),
];

/// Each path records whether every occurrence is numeric, for the raw cost maps
/// that preserve arbitrary numeric keys. Map identifiers are not schema keys.
fn payload_keys(bytes: &[u8]) -> BTreeMap<String, bool> {
    fn visit(value: &Value, path: &str, keys: &mut BTreeMap<String, bool>) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    let child = format!("{path}.{key}");
                    keys.entry(child.clone())
                        .and_modify(|numeric| *numeric &= value.is_number())
                        .or_insert(value.is_number());
                    match child.as_str() {
                        "provider.models" => {
                            for model in value
                                .as_object()
                                .expect("models must be an object")
                                .values()
                            {
                                visit(model, "model", keys);
                            }
                        }
                        "model.experimental.modes" => {
                            for mode in value.as_object().expect("modes must be an object").values()
                            {
                                visit(mode, "model.experimental.modes.*", keys);
                            }
                        }
                        _ => visit(value, &child, keys),
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    visit(value, path, keys);
                }
            }
            _ => {}
        }
    }

    let doc: Value = serde_json::from_slice(bytes).expect("payload must be JSON");
    let mut keys = BTreeMap::new();
    for provider in doc
        .as_object()
        .expect("payload must be a provider map")
        .values()
    {
        visit(provider, "provider", &mut keys);
    }
    keys
}

/// Read the actual serde fields, including renamed JSON keys. This is a source
/// fence because deserializing a payload cannot tell us which keys serde ignored.
fn struct_fields(raw: &str, name: &str) -> BTreeSet<String> {
    let declaration = format!("pub struct {name} {{");
    let start = raw
        .find(&declaration)
        .unwrap_or_else(|| panic!("missing {name}"));
    let body = &raw[start + declaration.len()..];
    let end = body.find("\n}").expect("raw struct must be closed");
    let body = &body[..end];
    let mut rename = None;
    let mut fields = BTreeSet::new();
    for line in body.lines().map(str::trim) {
        if let Some(attribute) = line.strip_prefix("#[serde(rename = \"") {
            rename = Some(attribute.split('"').next().unwrap().to_owned());
        }
        if let Some(field) = line.strip_prefix("pub ") {
            let name = field.split(':').next().expect("field name").trim();
            fields.insert(rename.take().unwrap_or_else(|| name.to_owned()));
        }
    }
    assert!(!fields.is_empty(), "{name} field extractor read nothing");
    fields
}

fn fields_the_parser_reads(raw: &str) -> BTreeSet<String> {
    let mut read = BTreeSet::new();
    for (path, name) in [
        ("provider", "RawProvider"),
        ("model", "RawModel"),
        ("model.limit", "RawLimit"),
        ("model.modalities", "RawModalities"),
        ("model.experimental", "RawExperimental"),
        ("model.experimental.modes.*", "RawMode"),
        ("model.cost", "RawCost"),
        ("model.cost.tiers", "RawTier"),
        ("model.cost.tiers.tier", "RawTierSpec"),
    ] {
        for field in struct_fields(raw, name) {
            if field != "scalars" && field != "malformed" && field != "provider" {
                read.insert(format!("{path}.{field}"));
            }
        }
    }
    // Custom deserializers do not derive their dispatch from struct field names.
    // Keep those paths tied to the code that actually takes their JSON values.
    for (path, anchor) in [
        ("model.cost.tiers", "\"tiers\" =>"),
        ("model.cost.context_over_200k", "\"context_over_200k\" =>"),
        ("model.cost.tiers.tier", "key == \"tier\""),
        ("model.experimental.modes.*.cost", "obj.remove(\"cost\")"),
    ] {
        if !raw.contains(anchor) {
            read.remove(path);
        }
    }
    read
}

fn within(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|tail| tail.starts_with('.'))
}

fn unaccounted_fields(bytes: &[u8], raw: &str) -> Vec<String> {
    let read = fields_the_parser_reads(raw);
    payload_keys(bytes)
        .into_iter()
        .filter_map(|(path, numeric)| {
            let declared = DECLARED_UNREAD.iter().any(|(prefix, reason)| {
                assert!(!reason.is_empty(), "an unread declaration needs a reason");
                within(&path, prefix)
            });
            // These values are intentionally opaque JSON, preserved rather than
            // narrowed to a vocabulary that could silently drop future entries.
            let opaque = ["model.reasoning_options", "model.experimental.modes.*.cost"]
                .iter()
                .any(|prefix| read.contains(*prefix) && within(&path, prefix));
            let scalar = numeric
                && [
                    "model.cost",
                    "model.cost.tiers",
                    "model.cost.context_over_200k",
                ]
                .iter()
                .any(|parent| {
                    path.rsplit_once('.').is_some_and(|(p, _)| p == *parent)
                        && read.contains(*parent)
                });
            (!read.contains(&path) && !declared && !opaque && !scalar).then_some(path)
        })
        .collect()
}

#[test]
fn every_seed_key_is_read_or_declared_unread() {
    let keys = payload_keys(SEED);
    assert!(
        keys.len() >= 40,
        "seed inventory read only {} keys",
        keys.len()
    );
    for path in [
        "provider.models",
        "model.canonical_model_id",
        "model.status",
        "model.limit.input",
        "model.cost.tiers.tier.type",
    ] {
        assert!(keys.contains_key(path), "seed must exercise {path}");
    }
    let violations = unaccounted_fields(SEED, RAW);
    assert!(
        violations.is_empty(),
        "seed keys neither parsed nor declared unread: {violations:?}"
    );
}

#[test]
fn planted_unknown_payload_key_is_reported() {
    let payload = br#"{"p":{"models":{"m":{"lost":true}}}}"#;
    assert_eq!(unaccounted_fields(payload, RAW), ["model.lost"]);
}

#[test]
fn planted_unknown_nested_payload_keys_are_reported() {
    let payload = br#"{"p":{"lost":true,"models":{"m":{"limit":{"lost":1},"modalities":{"lost":[]},"cost":{"lost":{}},"experimental":{"modes":{"fast":{"lost":true}}}}}}}"#;
    assert_eq!(
        unaccounted_fields(payload, RAW),
        [
            "model.cost.lost",
            "model.experimental.modes.*.lost",
            "model.limit.lost",
            "model.modalities.lost",
            "provider.lost",
        ]
    );
}

#[test]
fn opaque_and_quarantined_values_are_not_closed_vocabularies() {
    let payload = br#"{"p":{"models":{"m":{"provider":{"new_endpoint_hint":true},"reasoning_options":[{"future_setting":true}],"experimental":{"modes":{"fast":{"provider":{"future_header":true},"cost":{"future_rate":1}}}}}}}}"#;
    assert!(unaccounted_fields(payload, RAW).is_empty());
}

#[test]
fn unread_values_do_not_acquire_typed_parser_fields() {
    let read = fields_the_parser_reads(RAW);
    for (path, _) in DECLARED_UNREAD {
        assert!(
            !read.contains(*path),
            "declared unread value is now typed: {path}"
        );
    }
}
