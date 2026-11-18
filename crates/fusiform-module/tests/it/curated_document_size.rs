//! The curated model store keeps each curated data document as one database
//! value, and its database caps a value at 1,500,000 bytes of raw JSON. A
//! document that grows past that can't be published, so this test fails in
//! CI first, while splitting the document into several named ones is still
//! a local change.
//!
//! The embedded models.dev seed is not a curated document: upstream travels
//! as release changes and snapshots, never as one stored value.

use std::path::Path;

const STORE_DOCUMENT_LIMIT_BYTES: u64 = 1_500_000;
const UPSTREAM_SEED_FILES: [&str; 2] = ["models-dev-seed.json", "models-dev-seed.meta.json"];

/// Returns how many curated documents were checked and one message per
/// document over `limit`.
fn oversized_documents(data: &Path, limit: u64) -> (usize, Vec<String>) {
    let mut checked = 0;
    let mut over = Vec::new();
    for entry in std::fs::read_dir(data).expect("data folder is readable") {
        let path = entry.expect("data entry is readable").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !name.ends_with(".json") || UPSTREAM_SEED_FILES.contains(&name) {
            continue;
        }
        let size = std::fs::metadata(&path)
            .expect("document is readable")
            .len();
        if size > limit {
            over.push(format!(
                "{name} is {size} bytes, over the store's {limit}-byte value limit; \
                 split it into several named documents"
            ));
        }
        checked += 1;
    }
    (checked, over)
}

#[test]
fn every_curated_document_fits_one_store_value() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
    let (checked, over) = oversized_documents(&data, STORE_DOCUMENT_LIMIT_BYTES);
    assert!(over.is_empty(), "{over:#?}");
    // A scan that matched nothing would pass while checking nothing.
    assert!(
        checked >= 15,
        "only {checked} curated documents found in {}",
        data.display()
    );
}

#[test]
fn a_planted_oversized_document_is_reported_and_the_seed_is_not() {
    let dir = tempfile::tempdir().expect("temp dir");
    let big = vec![b' '; (STORE_DOCUMENT_LIMIT_BYTES + 1) as usize];
    std::fs::write(dir.path().join("big.json"), &big).expect("write planted document");
    std::fs::write(dir.path().join("models-dev-seed.json"), &big).expect("write planted seed");
    std::fs::write(dir.path().join("small.json"), b"{}").expect("write small document");

    let (checked, over) = oversized_documents(dir.path(), STORE_DOCUMENT_LIMIT_BYTES);
    assert_eq!(checked, 2);
    assert_eq!(over.len(), 1, "{over:#?}");
    assert!(
        over[0].starts_with("big.json is 1500001 bytes"),
        "{over:#?}"
    );
}
