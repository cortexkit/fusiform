use std::collections::BTreeMap;

use fusiform_curate::aliases;
use fusiform_module::{curation::validate_published, store_poll::VerifiedStoreHead};
use fusiform_release::{Upstream, VendorDocument};
use serde_json::{Value, value::RawValue};

use super::{Request, Result, json, metadata, segment};

pub(super) struct Input {
    document: String,
    kind: VendorDocument,
    file: String,
    value: Value,
    note: String,
    source: String,
    review_by: String,
}

impl Input {
    pub(super) fn new(
        document: &str,
        file: &str,
        note: String,
        source: String,
        review_by: String,
    ) -> Result<Self> {
        let kind = VendorDocument::from_name(document)
            .ok_or_else(|| format!("unrecognized published vendor document {document:?}"))?;
        metadata("note", &note)?;
        metadata("source", &source)?;
        if !is_date(&review_by) {
            return Err("review_by must be a valid date in YYYY-MM-DD form".into());
        }
        let raw: &RawValue =
            serde_json::from_str(file).map_err(|error| format!("value: {error}"))?;
        if raw.get().len() > 1_500_000 || !raw.get().starts_with('{') {
            return Err("value must be a JSON object of at most 1,500,000 bytes".into());
        }
        let value = serde_json::from_str(raw.get()).map_err(|error| format!("value: {error}"))?;
        Ok(Self {
            document: document.into(),
            kind,
            file: file.into(),
            value,
            note,
            source,
            review_by,
        })
    }

    pub(super) fn prepare(self, head: &VerifiedStoreHead) -> Result<Request> {
        let state = &head.state;
        let mut documents = state.curation_set.vendor_facts.clone();
        let previous = documents.insert(self.document.clone(), self.value.clone());
        validate_published(&documents, &state.upstream)?;
        if self.kind == VendorDocument::ModelAliases {
            check_alias_targets(&state.upstream, &self.file)?;
        }
        if head
            .vendor_documents
            .get(&self.document)
            .is_some_and(|current| current.get() == self.file.trim_matches([' ', '\t', '\r', '\n']))
        {
            return Err(format!(
                "{} is byte-identical to the signed copy",
                self.document
            ));
        }
        let diff = summarize(self.kind, previous.as_ref(), &self.value);
        // Insert the validated JSON text, including its whitespace, without re-encoding it.
        let body = format!(
            r#"{{"value":{},"note":{},"source":{},"review_by":{}}}"#,
            self.file,
            json(self.note),
            json(self.source),
            json(self.review_by),
        );
        Ok(Request {
            path: format!("/v1/curation/vendor-facts/{}", segment(&self.document)),
            body,
            summary: format!("{}: {diff}", self.document),
            subdirectory: None,
        })
    }
}

fn is_date(text: &str) -> bool {
    if text.len() != 10
        || !text.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 4 | 7) {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
    {
        return false;
    }
    let year: u32 = text[0..4].parse().unwrap();
    let month: u32 = text[5..7].parse().unwrap();
    let day: u32 = text[8..].parse().unwrap();
    let days = match month {
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => 0,
    };
    year > 0 && (1..=days).contains(&day)
}

fn check_alias_targets(upstream: &Upstream, file: &str) -> Result<()> {
    for ((provider, model), alias) in aliases::load_reporting_rejects(file).0 {
        if !upstream
            .get(&alias.target_provider_id)
            .is_some_and(|facts| facts.models.contains_key(&alias.target_model_id))
        {
            return Err(format!(
                "model-aliases: {provider}/{model} targets {}/{}, absent from verified upstream",
                alias.target_provider_id, alias.target_model_id
            ));
        }
    }
    Ok(())
}

fn summarize(kind: VendorDocument, previous: Option<&Value>, next: &Value) -> String {
    let empty = Value::Null;
    let previous = previous.unwrap_or(&empty);
    let old = rows(kind, previous);
    let new = rows(kind, next);
    let added: Vec<_> = new.keys().filter(|key| !old.contains_key(*key)).collect();
    let removed: Vec<_> = old.keys().filter(|key| !new.contains_key(*key)).collect();
    let changed: Vec<_> = new
        .iter()
        .filter_map(|(key, value)| old.get(key).filter(|old| *old != value).map(|_| key))
        .collect();
    let describe = |keys: Vec<&String>| {
        let labels = keys
            .iter()
            .map(|key| key.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if labels.is_empty() {
            "0".to_owned()
        } else {
            format!("{} [{labels}]", keys.len())
        }
    };
    let row_count = added.len() + removed.len() + changed.len();
    let mut summary = format!(
        "added {}, removed {}, changed {}",
        describe(added),
        describe(removed),
        describe(changed)
    );
    if row_count == 0 && previous != next {
        summary.push_str(" (non-row fields or row ordering differ)");
    }
    summary
}

fn rows(kind: VendorDocument, doc: &Value) -> BTreeMap<String, &Value> {
    use VendorDocument::*;

    const MODEL: &[&str] = &["provider_id", "model_id"];
    const PROVIDER: &[&str] = &["provider_id"];
    let collections: &[(&str, &[&str])] = match kind {
        AttachmentLimits => &[("providers", PROVIDER)],
        BillingPlanes => &[("planes", &["provider_id", "auth_method"])],
        CacheLifetimes => &[("rules", MODEL)],
        CanonicalLinks => &[("links", MODEL)],
        CuratedModels => &[("models", MODEL), ("held", MODEL)],
        CuratedProviders => &[("providers", &["id"])],
        FactOverrides => &[("overrides", &["provider_id", "model_id", "fact_key"])],
        InputAudioPerToken | OutputAudioPerToken => {
            &[("providers", PROVIDER), ("declined", PROVIDER)]
        }
        LoginMethods => &[("models", MODEL)],
        ModelAliases => &[("aliases", MODEL), ("retired", MODEL)],
        ModelCreators => &[("creators", &["family"])],
        PlanPrices => &[("cells", &["provider_id", "tier"])],
        SpeedTwins => &[("speed_twins", &["provider_id", "base_model_id", "mode"])],
        TemperatureRanges => &[("ranges", PROVIDER)],
        WireCorrections => &[("corrections", &["provider", "model"])],
        WireFamilies => &[("rules", &["npm", "shapes"])],
        ModelLineage => {
            return ["models", "families", "labs", "providers"]
                .into_iter()
                .flat_map(|name| {
                    doc[name]["entries"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .map(move |(key, value)| (format!("{name}:{key}"), value))
                })
                .collect();
        }
    };
    let mut rows = BTreeMap::new();
    for (collection, keys) in collections {
        for (index, row) in doc[*collection]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let key = keys
                .iter()
                .map(|key| match &row[*key] {
                    Value::String(text) => text.clone(),
                    Value::Array(values) => {
                        let mut parts: Vec<_> = values.iter().map(Value::to_string).collect();
                        parts.sort_unstable();
                        parts.join("|")
                    }
                    value => value.to_string(),
                })
                .collect::<Vec<_>>()
                .join("/");
            let mut key = format!("{collection}:{key}");
            // Some documents are accepted without checking for duplicate keys.
            if rows.contains_key(&key) {
                key = format!("{key} (row {})", index + 1);
            }
            rows.insert(key, row);
        }
    }
    rows
}
