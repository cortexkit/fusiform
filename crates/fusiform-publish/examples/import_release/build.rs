use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs::{self, File},
    io::{BufWriter, Write},
    path::Path,
};

use fusiform_core::{normalize_models_dev, upstream_facts::facts_of};
use fusiform_publish::FactIndex;
use fusiform_release as release;
use fusiform_release::{
    ChangeOp, CurationSet, Decommissioned, Grade, ProviderFacts, State, Upstream,
};
use fusiform_store::{ingest::upstream_claim_of, serve::POINT_IN_TIME_SQL};
use rusqlite::params;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use serde_json::{Map, Value};

const SOURCE: &str = "models.dev";
const DOCUMENT_LIMIT: usize = 1_500_000;

type ImportResult<T> = Result<T, Box<dyn Error>>;

pub fn import(
    backup: &Path,
    data: &Path,
    out: &Path,
    published_at_ms: i64,
) -> ImportResult<BTreeMap<&'static str, usize>> {
    let mut conn = Connection::open_with_flags(backup, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let tx = conn.transaction()?;
    let (observed_at_ms, state) = read_state(&tx, data)?;
    let State {
        upstream,
        curation_set,
    } = &state;
    if published_at_ms < observed_at_ms
        || curation_set
            .approved_at_ms
            .is_some_and(|at| published_at_ms < at)
    {
        return Err(
            "publication predates the latest upstream observation or curation approval".into(),
        );
    }
    let upstream_hash = release::upstream_state_hash(upstream);
    // Observations store raw digests, not models.dev response bodies. Rebuild
    // the publisher's recovery index from the actual upstream document bytes.
    let upstream_wire = release::canonical_json(&serde_json::to_value(upstream)?);
    let index = FactIndex::from_snapshot_upstream_reader(upstream_wire.as_slice())?;
    if index.upstream_hash() != upstream_hash {
        return Err("recovery index disagrees with the upstream state hash".into());
    }

    let ops: Vec<_> = release::upstream_facts(upstream)
        .map(|((provider, model, fact), value)| match model {
            Some(model) => ChangeOp::Set {
                provider: provider.into(),
                model: model.into(),
                fact: fact.into(),
                value: value.clone(),
            },
            None => ChangeOp::SetProvider {
                provider: provider.into(),
                fact: fact.into(),
                value: value.clone(),
            },
        })
        .collect();
    let unsigned = release::build_release(
        &release::canonical_json(&json!({"ops": ops, "upstream_hash": upstream_hash})),
        &release::canonical_json(&serde_json::to_value(curation_set)?),
        b"",
        1,
        published_at_ms,
        observed_at_ms,
    )?;
    // Refuse to overwrite an existing release folder.
    fs::create_dir(out)?;
    let result = (|| {
        fs::create_dir(out.join("history"))?;
        let counts = write_history(&tx, &out.join("history/base.json"))?;
        fs::write(out.join("release-1.json"), unsigned)?;
        let mut file = BufWriter::new(File::create(out.join("state.json"))?);
        release::write_canonical_json(&mut file, &serde_json::to_value(state)?)?;
        file.flush()?;
        Ok(counts)
    })();
    if result.is_err() {
        fs::remove_dir_all(out)?;
    }
    result
}

pub fn read_state(conn: &Connection, data: &Path) -> ImportResult<(i64, State)> {
    let at = conn.query_row(
        "SELECT observed_at_ms FROM observation WHERE source = ?1 \
         AND outcome IN ('changed', 'unchanged', 'not_modified', 'seeded') \
         ORDER BY observed_at_ms DESC, id DESC LIMIT 1",
        [SOURCE],
        |row| row.get(0),
    )?;
    Ok((
        at,
        State {
            upstream: upstream_at(conn, at)?,
            curation_set: curation(conn, data)?,
        },
    ))
}

fn upstream_at(conn: &Connection, at: i64) -> ImportResult<Upstream> {
    // Use the store's point-in-time query, without served corrections and
    // curation layers. A later id wins a tie at the same recorded boundary.
    let mut stmt = conn.prepare(&format!("{POINT_IN_TIME_SQL} ORDER BY e.id"))?;
    let mut rows = stmt.query(params![SOURCE, at])?;
    let nullable = nullable_projection_keys()?;
    let mut upstream: Upstream = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let provider: String = row.get(0)?;
        let model: String = row.get(1)?;
        let key: String = row.get(2)?;
        let raw: String = row.get(3)?;
        let value = upstream_claim_of(serde_json::from_str(&raw)?);
        upstream
            .entry(provider)
            .or_insert_with(|| ProviderFacts {
                provider: BTreeMap::new(),
                models: BTreeMap::new(),
            })
            .models
            .entry(model)
            .or_default()
            .insert(key, value);
    }
    upstream.retain(|_, provider| {
        provider.models.retain(|_, facts| {
            if facts.get("existence").and_then(Value::as_str) != Some("present") {
                return false;
            }
            facts.retain(|key, value| {
                if value.is_null() {
                    nullable.contains(key)
                } else {
                    // `MissingRate` is an ingest tombstone, not a rate that
                    // the models.dev parser puts in the publisher projection.
                    !(key.starts_with("rate.")
                        && value.get("reason").and_then(Value::as_str) == Some("missing_rate"))
                }
            });
            true
        });
        !provider.models.is_empty()
    });
    if upstream.is_empty() {
        return Err("latest observation has no models in force".into());
    }
    Ok(upstream)
}

fn nullable_projection_keys() -> ImportResult<BTreeSet<String>> {
    // A null era for a sparse fact is a withdrawal. The presence of a null
    // in the projection of an otherwise empty model identifies the base
    // facts that publish null instead. Use the projection, not a copied list.
    let catalog = normalize_models_dev(br#"{"p":{"models":{"m":{}}}}"#)?.catalog;
    let model = catalog.models().next().ok_or("empty projection probe")?;
    Ok(facts_of(model)
        .values
        .into_iter()
        .filter(|(_, value)| value == "null")
        .map(|(key, _)| key.as_str().to_owned())
        .collect())
}

fn curation(conn: &Connection, data: &Path) -> ImportResult<CurationSet> {
    let approved_at_ms = conn.query_row(
        "SELECT MAX(recorded_at_ms) FROM (SELECT recorded_at_ms FROM quality_grade_era \
         UNION ALL SELECT recorded_at_ms FROM model_mark_era)",
        [],
        |row| row.get(0),
    )?;
    let mut set = CurationSet {
        version: 1,
        approved_at_ms,
        approved_drafts: vec!["import-2026-11".to_owned()],
        grades: BTreeMap::new(),
        effort_grades: BTreeMap::new(),
        kind_grades: BTreeMap::new(),
        workload_efforts: BTreeMap::new(),
        decommissioned: BTreeMap::new(),
        vendor_facts: vendor_facts(data)?,
    };

    let mut stmt = conn.prepare(
        "SELECT model, kind, elo, eq, grade FROM quality_grade_era q WHERE withdrawn = 0 \
         AND id = (SELECT MAX(id) FROM quality_grade_era WHERE model = q.model AND kind IS q.kind)",
    )?;
    let mut grades = stmt.query([])?;
    while let Some(row) = grades.next()? {
        let model: String = row.get(0)?;
        if let Some(kind) = row.get::<_, Option<String>>(1)? {
            let grade: String = row.get(4)?;
            set.kind_grades
                .entry(model)
                .or_default()
                .insert(kind, serde_json::from_str(&grade)?);
        } else {
            let elo: String = row.get(2)?;
            let eq: String = row.get(3)?;
            set.grades.insert(
                model,
                Grade {
                    elo: serde_json::from_str(&elo)?,
                    eq: serde_json::from_str(&eq)?,
                    graded_effort: None,
                },
            );
        }
    }
    let mut stmt = conn.prepare(
        "SELECT model, reason FROM model_mark_era m WHERE decommissioned = 1 \
         AND id = (SELECT MAX(id) FROM model_mark_era WHERE model = m.model)",
    )?;
    let mut marks = stmt.query([])?;
    while let Some(row) = marks.next()? {
        let model: String = row.get(0)?;
        set.decommissioned.insert(
            model,
            Decommissioned {
                reason: row.get(1)?,
            },
        );
    }
    Ok(set)
}

fn vendor_facts(data: &Path) -> ImportResult<BTreeMap<String, Value>> {
    let mut docs = BTreeMap::new();
    for entry in fs::read_dir(data)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("non-UTF-8 document name")?;
        if name.starts_with("models-dev-seed") || name == "engram-catalog" {
            continue;
        }
        let raw = fs::read(&path)?;
        if raw.len() > DOCUMENT_LIMIT {
            return Err(format!("{name} exceeds the {DOCUMENT_LIMIT}-byte value limit").into());
        }
        let doc: Value = serde_json::from_slice(&raw).map_err(|err| format!("{name}: {err}"))?;
        if release::canonical_json(&doc).len() > DOCUMENT_LIMIT {
            return Err(
                format!("canonical {name} exceeds the {DOCUMENT_LIMIT}-byte value limit").into(),
            );
        }
        docs.insert(name.to_owned(), doc);
    }
    Ok(docs)
}

fn write_history(conn: &Connection, path: &Path) -> ImportResult<BTreeMap<&'static str, usize>> {
    let mut out = BufWriter::new(File::create(path)?);
    out.write_all(br#"{"format":"ck-models-history-base/v1","release":1,"tables":{"#)?;
    let mut counts = BTreeMap::new();
    // Canonical table keys are byte-sorted; original row ids preserve SQLite's
    // tie order as well as the foreign keys to observations and artifact events.
    for table in [
        "cache_lifetime_era",
        "curated_model_era",
        "era",
        "fact_override_era",
        "model_mark_era",
        "observation",
        "observation_artifact_event",
        "plan_price_era",
        "quality_grade_era",
    ] {
        if !counts.is_empty() {
            out.write_all(b",")?;
        }
        write!(out, "\"{table}\":[")?;
        let mut stmt = conn.prepare(&format!("SELECT * FROM {table} ORDER BY id"))?;
        let names: Vec<_> = stmt.column_names().into_iter().map(str::to_owned).collect();
        let mut rows = stmt.query([])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            let mut values = Map::new();
            for (i, name) in names.iter().enumerate() {
                let value = match row.get_ref(i)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(n) => Value::from(n),
                    ValueRef::Text(text) => {
                        let text = str::from_utf8(text)?;
                        if contains_home_path(text) {
                            return Err(
                                format!("home path in {table}.{name}; refusing export").into()
                            );
                        }
                        Value::from(text)
                    }
                    _ => return Err(format!("unexpected SQLite type for {table}.{name}").into()),
                };
                values.insert(name.clone(), value);
            }
            if count > 0 {
                out.write_all(b",")?;
            }
            release::write_canonical_json(&mut out, &Value::Object(values))?;
            count += 1;
        }
        out.write_all(b"]")?;
        counts.insert(table, count);
    }
    out.write_all(b"}}")?;
    out.flush()?;
    Ok(counts)
}

fn contains_home_path(text: &str) -> bool {
    if !text.contains(['/', '\\', '~']) {
        return false;
    }
    let path = text.to_ascii_lowercase().replace('\\', "/");
    let path = if path.contains("//") {
        path.replace("//", "/")
    } else {
        path
    };
    if [
        "/users/",
        "/home/",
        "/root/",
        "/documents and settings/",
        "~/",
    ]
    .iter()
    .any(|prefix| path.contains(prefix))
    {
        return true;
    }
    // JSON-in-TEXT can escape path separators or letters. Decode it before
    // checking again so the same path does not pass with a different spelling.
    if text.contains("\\u") || text.contains(r"\/") {
        fn nested(value: &Value) -> bool {
            match value {
                Value::String(text) => contains_home_path(text),
                Value::Array(items) => items.iter().any(nested),
                Value::Object(items) => items
                    .iter()
                    .any(|(key, value)| contains_home_path(key) || nested(value)),
                _ => false,
            }
        }
        return serde_json::from_str::<Value>(text).is_ok_and(|value| nested(&value));
    }
    false
}
