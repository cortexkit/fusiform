use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_module::route::serve_tool_call;
use fusiform_store::{CatalogStore, QualityGrade};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

fn render(store: &CatalogStore, arguments: Value) -> String {
    let request = json!({"name":"quality.grades", "arguments":arguments});
    let case = match serve_tool_call(store, &serde_json::to_vec(&request).unwrap()) {
        Ok(response) => json!({"request":request, "response":response}),
        Err(error) => {
            json!({"request":request, "error":{"code":error.code,"message":error.message}})
        }
    };
    serde_json::to_string_pretty(&case).unwrap() + "\n"
}

#[test]
fn quality_grades_prefix_fixtures_match_real_serializer() {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("prefix.db").to_string_lossy().into(),
        },
    })
    .unwrap();
    let mut files = BTreeMap::new();
    files.insert(
        "local-empty.json",
        render(&store, json!({"prefix":"local:"})),
    );
    for (key, kind, n, at) in [
        ("local:apple/phone-transcriber", "transcription", "59", 1000),
        ("local:example/offline-speaker", "speech", "72.50", 1001),
    ] {
        store
            .append_quality_grade(
                key,
                Some(kind),
                Some(&QualityGrade::NonChat {
                    grade: serde_json::from_str(n).unwrap(),
                }),
                "operator",
                "Synthetic local engine evaluation",
                at,
            )
            .unwrap();
    }
    files.insert(
        "local-two-entries.json",
        render(&store, json!({"prefix":"local:"})),
    );
    files.insert(
        "prefix-and-model-refused.json",
        render(
            &store,
            json!({"prefix":"local:","model":"local:apple/phone-transcriber"}),
        ),
    );
    let sums: String = files
        .iter()
        .map(|(name, text)| format!("{:x}  {name}\n", Sha256::digest(text.as_bytes())))
        .collect();
    files.insert("SHA256SUMS", sums);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/quality-grades-prefix-v1");
    if std::env::var_os("UPDATE_QUALITY_PREFIX_FIXTURES").is_some() {
        std::fs::create_dir_all(&root).unwrap();
        for (name, text) in files {
            std::fs::write(root.join(name), text).unwrap();
        }
        eprintln!("regenerated 3 quality.grades JSON fixtures and SHA256SUMS");
    } else {
        for (name, text) in files {
            assert_eq!(
                std::fs::read_to_string(root.join(name)).unwrap(),
                text,
                "{name} drifted; regenerate with UPDATE_QUALITY_PREFIX_FIXTURES=1"
            );
        }
    }
}
