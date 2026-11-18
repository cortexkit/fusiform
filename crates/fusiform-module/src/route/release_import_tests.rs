use std::{cell::Cell, collections::BTreeMap, fs, path::Path};

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::{normalize_models_dev, BoundaryKind, ChargeUnitEvidence, ObservationOutcome};
use fusiform_publish::{publish_step, snapshot_bytes_from_state, FactIndex};
use fusiform_release::{
    canonical_json, sha256, sign_envelope, sign_history_attestation, verify_envelope,
    verify_history_attestation, HistoryAttestationBody, HistoryFile, ReleaseEnvelope, SigningKey,
    Snapshot,
};
use fusiform_store::{
    ingest::plan_ingest, ImportedTables, NewEra, NewObservation, QualityGrade, ReleaseArrival,
    StoreRelease,
};
use rusqlite::{params_from_iter, types::Value as SqlValue, Connection, OpenFlags};
use serde_json::{json, Value};

use super::{serve_catalog_get_with_limits, CatalogStore, FactKey, SourceId, Timestamp};

#[path = "../../../fusiform-publish/examples/import_release/build.rs"]
mod build;

thread_local! {
    // A current read still uses the current serving layers. Pin only the
    // database read instant so both stores can serve precisely the same time.
    static CURRENT_AT: Cell<Option<Timestamp>> = const { Cell::new(None) };
}

pub(super) fn current_at() -> Option<Timestamp> {
    CURRENT_AT.get()
}

const KID: &str = "release-format-v1-test-key";
const PUBLIC_KEY: &[u8] = br#"{"kid":"release-format-v1-test-key","public_key":"GX9rI-FshTLGq8g4-s1ep4m-DHaykgM0A5v6iz02jWE"}"#;
const TABLES_IN_LOAD_ORDER: [&str; 9] = [
    "observation",
    "observation_artifact_event",
    "era",
    "quality_grade_era",
    "model_mark_era",
    "fact_override_era",
    "curated_model_era",
    "cache_lifetime_era",
    "plan_price_era",
];

fn open(path: &Path) -> CatalogStore {
    CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: path.to_string_lossy().into_owned(),
        },
    })
    .unwrap()
}

fn wire(value: impl serde::Serialize) -> Vec<u8> {
    canonical_json(&serde_json::to_value(value).unwrap())
}

fn backup(original: &Path, copy: &Path) {
    let conn = Connection::open_with_flags(original, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.backup(rusqlite::DatabaseName::Main, copy, None)
        .unwrap();
}

fn observe(store: &CatalogStore, body: &[u8], at: i64, seeded: bool) -> i64 {
    let id = store
        .record_observation(&NewObservation {
            source: SourceId::ModelsDev,
            observed_at: Timestamp(at),
            outcome: if seeded {
                ObservationOutcome::Seeded
            } else {
                ObservationOutcome::Changed { snapshot_seq: 1 }
            },
            normalized_hash: None,
            raw_hash: None,
            etag: None,
            duration_ms: None,
            detail: None,
        })
        .unwrap();
    let catalog = normalize_models_dev(body).unwrap().catalog;
    let plan = plan_ingest(
        store,
        &catalog,
        Timestamp(at),
        if seeded {
            BoundaryKind::Seed
        } else {
            BoundaryKind::Observed
        },
        (!seeded).then_some(id),
    )
    .unwrap();
    assert!(store.append_eras(&plan.eras).unwrap() > 0);
    id
}

fn sign_snapshot(out: &Path, base: &[u8]) -> Snapshot {
    let unsigned: Value =
        serde_json::from_slice(&fs::read(out.join("release-1.json")).unwrap()).unwrap();
    assert_eq!(unsigned.as_object().unwrap().len(), 2);
    let envelope: ReleaseEnvelope = serde_json::from_value(unsigned["envelope"].clone()).unwrap();
    assert_eq!(envelope.release, 1);
    assert_eq!(envelope.parent, None);
    let key = SigningKey::from_bytes(&[42; 32]);
    let signatures = sign_envelope(&envelope, KID, &key).unwrap();
    let body = HistoryAttestationBody {
        release: 1.try_into().unwrap(),
        state_hash: envelope.state_hash,
        history: vec![HistoryFile {
            file: "base.json".into(),
            sha256: sha256(base),
        }],
    };
    let state = fs::read(out.join("state.json")).unwrap();
    let signed = snapshot_bytes_from_state(
        &state,
        &wire(&envelope),
        &wire(&signatures),
        &wire(&body.history),
        &wire(sign_history_attestation(&body, KID, &key)),
        PUBLIC_KEY,
    )
    .unwrap();
    println!(
        "test-signed snapshot: {} bytes, sha256 {}",
        signed.len(),
        sha256(&signed)
    );
    let snapshot: Snapshot = serde_json::from_slice(&signed).unwrap();
    let pinned = BTreeMap::from([(KID.to_owned(), key.verifying_key())]);
    verify_envelope(&snapshot.envelope, &snapshot.signatures, &pinned).unwrap();
    let attestation = snapshot.history_attestation.as_ref().unwrap();
    verify_history_attestation(attestation, &snapshot.envelope, &pinned).unwrap();
    let [file] = attestation.body.history.as_slice() else {
        panic!("one base file")
    };
    assert_eq!(file.file, "base.json");
    file.verify_bytes(base).unwrap();
    snapshot
}

fn load_history(conn: &mut Connection, base: &Value) {
    assert_eq!(base["format"], "ck-models-history-base/v1");
    assert_eq!(base["release"], 1);
    let tables = base["tables"].as_object().unwrap();
    assert_eq!(tables.len(), TABLES_IN_LOAD_ORDER.len());
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    let tx = conn.transaction().unwrap();
    // Some migrations seed the data tables. An install takes the exact rows
    // from the attested import instead of combining them with migration seeds.
    for table in TABLES_IN_LOAD_ORDER.into_iter().rev() {
        tx.execute(&format!("DELETE FROM {table}"), []).unwrap();
    }
    for table in TABLES_IN_LOAD_ORDER {
        let mut columns = tx.prepare(&format!("PRAGMA table_info({table})")).unwrap();
        let names: Vec<String> = columns
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let placeholders = vec!["?"; names.len()].join(",");
        let sql = format!(
            "INSERT INTO {table} ({}) VALUES ({placeholders})",
            names.join(",")
        );
        let mut stmt = tx.prepare(&sql).unwrap();
        let mut previous_id = 0;
        for row in tables[table].as_array().unwrap() {
            let row = row.as_object().unwrap();
            assert_eq!(row.len(), names.len());
            let id = row["id"].as_i64().unwrap();
            assert!(id > previous_id);
            previous_id = id;
            stmt.execute(params_from_iter(names.iter().map(|name| {
                match &row[name] {
                    Value::Null => SqlValue::Null,
                    Value::Number(n) => SqlValue::Integer(n.as_i64().unwrap()),
                    Value::String(s) => SqlValue::Text(s.clone()),
                    other => panic!("unexpected SQLite value {other}"),
                }
            })))
            .unwrap();
        }
    }
    tx.commit().unwrap();
}

fn assert_catalog_bytes(source: &CatalogStore, destination: &CatalogStore, at: [i64; 3], now: i64) {
    CURRENT_AT.set(Some(Timestamp(now)));
    for request in at
        .map(|t| wire(json!({"at_ms": t})))
        .into_iter()
        .chain([b"{}".to_vec()])
    {
        let read = |store| {
            let mut response =
                serve_catalog_get_with_limits(store, &request, usize::MAX, usize::MAX).unwrap();
            // Catalog versions are local watermarks and are not in the import.
            response.catalog_version = 0;
            assert!(!response.models.is_empty());
            serde_json::to_vec(&response).unwrap()
        };
        let a = read(source);
        let b = read(destination);
        assert_eq!(
            sha256(&a),
            sha256(&b),
            "catalog.get {}",
            String::from_utf8_lossy(&request)
        );
        assert!(a == b);
        println!(
            "catalog.get {}: {} bytes, sha256 {}",
            String::from_utf8_lossy(&request),
            a.len(),
            sha256(&a)
        );
    }
    CURRENT_AT.set(None);
}

fn load_and_compare(
    source: &CatalogStore,
    out: &Path,
    data: &Path,
    instants: [i64; 3],
    current: i64,
) -> (Value, Snapshot) {
    let base = fs::read(out.join("history/base.json")).unwrap();
    let snapshot = sign_snapshot(out, &base);
    let rows: Value = serde_json::from_slice(&base).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("consumer.db");
    let destination = open(&path);
    let mut conn = Connection::open(&path).unwrap();
    load_history(&mut conn, &rows);
    // Base already includes the release-1 facts and curation eras. Install
    // the snapshot by checking those loaded tables against its current state,
    // not by appending duplicates that would manufacture history.
    let (observed_at, loaded_state) = build::read_state(&conn, data).unwrap();
    assert_eq!(observed_at, snapshot.envelope.upstream_observed_at_ms);
    assert_eq!(loaded_state, snapshot.state);
    assert_catalog_bytes(source, &destination, instants, current);
    (rows, snapshot)
}

#[test]
fn dated_store_release_keeps_era_facts_but_not_current_layers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.db");
    let source = open(&path);
    let published_at = 1_795_000_000_000_i64;
    const CARBON: &str = "google/gemini-4-carbon";
    const ALIAS: &str = "google/antigravity-gemini-4-carbon";
    const SONNET: &str = "anthropic/claude-sonnet-5-5";
    observe(
        &source,
        &wire(json!({
            "google": {"name": "Google", "npm": "@ai-sdk/google", "models": {
                "gemini-4-carbon": {"name": "Gemini 4 Carbon", "family": "gemini",
                    "limit": {"output": 32768}, "cost": {"input": 2, "output": 8}}
            }},
            "anthropic": {"name": "Anthropic", "npm": "@ai-sdk/anthropic", "models": {
                "claude-sonnet-5-5": {"name": "Claude Sonnet 5.5", "family": "claude-sonnet",
                    "cost": {"input": 3, "output": 15, "cache_write": 3.75}}
            }}
        })),
        published_at - 10,
        true,
    );
    let tables = crate::curation::embedded();
    crate::curated_models::apply_to_store(&source, tables.curated_models, published_at).unwrap();
    crate::fact_overrides::apply_to_store(&source, tables.fact_overrides, published_at).unwrap();
    source
        .append_cache_lifetimes(
            &crate::cache_lifetimes::stored_rules(tables.cache_lifetimes),
            published_at,
        )
        .unwrap();
    source
        .append_quality_grade(
            CARBON,
            None,
            Some(&QualityGrade::Chat {
                elo: 1012.into(),
                eq: 780.into(),
            }),
            "operator",
            "fixture chat grade",
            published_at - 1,
        )
        .unwrap();

    let copy = dir.path().join("backup.db");
    backup(&path, &copy);
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
    let out = dir.path().join("import");
    build::import(&copy, &data, &out, published_at).unwrap();
    let base: Value =
        serde_json::from_slice(&fs::read(out.join("history/base.json")).unwrap()).unwrap();
    let base: ImportedTables = serde_json::from_value(base["tables"].clone()).unwrap();
    let documents = ["model-aliases", "model-lineage", "wire-families"]
        .into_iter()
        .map(|name| {
            let bytes = fs::read(data.join(format!("{name}.json"))).unwrap();
            (name.to_owned(), serde_json::from_slice(&bytes).unwrap())
        })
        .collect();
    let mut rows = ImportedTables::new();
    crate::curation::document_rows(&mut rows, None, &documents, published_at).unwrap();
    let consumer = CatalogStore::open_for_store(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "consumer".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir
                .path()
                .join("consumer.db")
                .to_string_lossy()
                .into_owned(),
        },
    })
    .unwrap();
    consumer
        .apply_store_releases(
            Some(&base),
            &[ReleaseArrival {
                published_at_ms: published_at,
                release: StoreRelease {
                    release: 1,
                    state_hash: "a".repeat(64),
                    curation_hash: "b".repeat(64),
                    applied_at_ms: published_at,
                },
                rows,
            }],
        )
        .unwrap();

    let read = |store, args| {
        serve_catalog_get_with_limits(store, &wire(args), usize::MAX, usize::MAX).unwrap()
    };
    let current = read(&consumer, json!({}));
    assert!(current.models.contains_key(ALIAS));
    assert_eq!(current.models[CARBON]["model.lineage"], "gemini");
    assert_eq!(
        current.models[CARBON]["wire.family"],
        "google.generate_content"
    );

    let at = published_at + 1;
    let args = json!({"at_ms": at});
    let dated = read(&consumer, args.clone());
    let expected = read(&source, args);
    assert!(
        !dated.models.contains_key(ALIAS),
        "dated catalog has an alias row"
    );
    assert!(dated.aliased.is_empty());
    assert!(!dated.models[CARBON].contains_key("model.lineage"));
    assert!(!dated.models[CARBON].contains_key("wire.family"));
    assert_eq!(dated.models[CARBON]["quality.elo"], 1012);
    assert_eq!(dated.models[CARBON]["limit.output"], 65536);
    assert!(!dated.fact_overrides.is_empty());
    assert_eq!(
        dated.models[SONNET]["rate.cache_write.ttl.1h"]["units"],
        6_000_000_000_i64
    );
    assert!(!dated.curated.is_empty());
    assert_eq!(dated.models, expected.models);
    assert_eq!(dated.curated, expected.curated);
    assert_eq!(dated.fact_overrides, expected.fact_overrides);

    let alias_at = wire(
        json!({"provider_id":"google", "model_id":"antigravity-gemini-4-carbon", "at_ms": at}),
    );
    let refusal =
        serve_catalog_get_with_limits(&consumer, &alias_at, usize::MAX, usize::MAX).unwrap_err();
    assert_eq!(refusal.code, "no_coverage");
    assert!(refusal.message.contains("current reads only"));
}

#[test]
fn release_import_preserves_catalog_get_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.db");
    let store = open(&path);
    let models = |price| {
        let first = if price == 2 {
            json!({"name":"First", "structured_output":true, "cost":{"input":price,"output":4}})
        } else {
            json!({"name":"First", "cost":{"input":price}})
        };
        wire(json!({"fixture": {"name":"Fixture Labs", "models": {
            "first":first,
            "steady":{"name":"Steady", "cost":{"input":5,"output":6}}
        }}}))
    };
    let first = models(2);
    let last = models(3);
    let seed = 1_800_000_000_000;
    let end = seed + 400;
    observe(&store, &first, seed, true);
    let marked = observe(&store, &last, end, false);
    let mut rate: Value = serde_json::from_str(
        &store.current_values(SourceId::ModelsDev).unwrap()[&("fixture".into(), "steady".into())]
            [&FactKey::from_stored("rate.input".into())],
    )
    .unwrap();
    rate["unit_provenance"] = json!("producer annotation");
    store
        .append_eras(&[NewEra {
            source: SourceId::ModelsDev,
            provider_id: "fixture".into(),
            model_id: "steady".into(),
            fact_key: FactKey::from_stored("rate.input".into()),
            value_json: rate.to_string(),
            boundary_at: Timestamp(end),
            boundary_kind: BoundaryKind::Observed,
            observation_id: Some(marked),
        }])
        .unwrap();
    store
        .mark_observation_artifact(marked, "historical producer annotation", Timestamp(end + 1))
        .unwrap();
    assert_eq!(store.same_claim_era_fraction(marked).unwrap(), (1, 4));
    store
        .append_quality_grade(
            "fixture/first",
            None,
            Some(&QualityGrade::Chat {
                elo: serde_json::from_str("1012.30").unwrap(),
                eq: 780.into(),
            }),
            "operator",
            "fixture chat grade",
            end + 2,
        )
        .unwrap();
    store
        .append_quality_grade(
            "fixture/first",
            Some("search"),
            Some(&QualityGrade::NonChat { grade: 91.into() }),
            "operator",
            "fixture kind grade",
            end + 3,
        )
        .unwrap();
    store
        .append_quality_grade(
            "local:fixture/transcriber",
            Some("transcription"),
            Some(&QualityGrade::NonChat { grade: 78.into() }),
            "operator",
            "fixture on-device grade",
            end + 3,
        )
        .unwrap();
    store
        .append_model_mark("fixture/steady", true, "operator", "fixture mark", end + 4)
        .unwrap();

    let copy = dir.path().join("backup.db");
    backup(&path, &copy);
    let input_before = sha256(&fs::read(&copy).unwrap());
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
    let out = dir.path().join("import");
    let counts = build::import(&copy, &data, &out, end + 5).unwrap();
    assert_eq!(sha256(&fs::read(&copy).unwrap()), input_before);
    assert_eq!(counts["observation"], 2);
    assert_eq!(counts["observation_artifact_event"], 1);
    let (rows, snapshot) = load_and_compare(&store, &out, &data, [seed, seed + 200, end], end + 5);
    assert_eq!(
        fs::read(out.join("history/base.json")).unwrap(),
        wire(&rows)
    );
    let imported: Vec<_> = rows["tables"]["era"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["observation_id"] == marked)
        .collect();
    assert_eq!(
        imported.len(),
        4,
        "base keeps the artifact restatement, genuine change, and withdrawals"
    );
    assert_eq!(imported[0]["model_id"], "first");
    assert_eq!(imported[3]["model_id"], "steady");
    let empty = FactIndex::from_snapshot_upstream(&BTreeMap::new()).unwrap();
    let published = publish_step(&last, &empty, &ChargeUnitEvidence::default()).unwrap();
    assert_eq!(published.upstream_hash, snapshot.envelope.upstream_hash);
    let curation = snapshot.state.curation_set;
    assert_eq!(curation.approved_at_ms, Some(end + 4));
    assert_eq!(curation.approved_drafts, ["import-2026-11"]);
    assert_eq!(curation.grades["fixture/first"].elo.to_string(), "1012.30");
    assert_eq!(curation.grades["fixture/first"].graded_effort, None);
    assert_eq!(
        curation.kind_grades["fixture/first"]["search"].as_i64(),
        Some(91)
    );
    assert_eq!(
        curation.kind_grades["local:fixture/transcriber"]["transcription"].as_i64(),
        Some(78)
    );
    assert_eq!(
        curation.decommissioned["fixture/steady"].reason,
        "fixture mark"
    );
    assert!(curation.vendor_facts.contains_key("plan-prices"));
    assert!(!curation.vendor_facts.contains_key("models-dev-seed"));
    assert!(!curation.vendor_facts.contains_key("engram-catalog"));
}

#[test]
fn release_import_refuses_home_paths_in_buried_history_values() {
    let at = 1_800_000_000_000;
    for detail in [
        "/Users/example/store.db",
        "file:///home/example/store.db",
        "open ~/private/store.db",
        r#"{"nested":{"file":"C:\\Users\\Example\\store.db"}}"#,
        "open /root/private/store.db",
        r#"{"nested":"\u002fhome\u002fexample\u002fprivate"}"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.db");
        let store = open(&path);
        observe(&store, br#"{"fixture":{"models":{"test":{}}}}"#, at, true);
        store
            .record_observation(&NewObservation {
                source: SourceId::ModelsDev,
                observed_at: Timestamp(at + 1),
                outcome: ObservationOutcome::Unchanged,
                normalized_hash: None,
                raw_hash: None,
                etag: None,
                duration_ms: None,
                detail: Some(detail.into()),
            })
            .unwrap();
        let copy = dir.path().join("backup.db");
        backup(&path, &copy);
        let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
        let out = dir.path().join("import");
        let err = build::import(&copy, &data, &out, at + 2)
            .unwrap_err()
            .to_string();
        assert!(err.contains("home path in observation.detail"), "{err}");
        assert!(
            !err.contains(detail),
            "diagnostics must not repeat the path"
        );
        assert!(
            !out.exists(),
            "refused exports must not leave partial output"
        );
    }
}

#[test]
#[ignore = "requires the operator's store backup-API copy"]
fn release_import_real_backup_preserves_catalog_get_bytes() {
    let input =
        std::env::var_os("FUSIFORM_IMPORT_PROOF_BACKUP").expect("set FUSIFORM_IMPORT_PROOF_BACKUP");
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("source.db");
    backup(Path::new(&input), &copy);
    let source = open(&copy);
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
    let out = dir.path().join("import");
    let at = 1_795_009_999_139;
    build::import(Path::new(&input), &data, &out, at).unwrap();
    let (rows, _) = load_and_compare(
        &source,
        &out,
        &data,
        [1_786_529_249_396, 1_786_865_960_643, 1_793_491_200_000],
        at,
    );
    assert_eq!(
        source.same_claim_era_fraction(203).unwrap(),
        (17_454, 17_455)
    );
    let archived = rows["tables"]["era"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["observation_id"] == 203)
        .count();
    assert_eq!(archived, 17_455);
    println!("observation 203: 17454 restatements and 1 genuine change, all retained");
}
