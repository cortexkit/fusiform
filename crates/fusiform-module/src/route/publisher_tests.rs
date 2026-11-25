use super::*;
use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_curate::curate;

#[test]
fn publisher_seed_matches_catalog_get_for_every_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("catalog.db").to_string_lossy().into_owned(),
        },
    })
    .unwrap();
    crate::seed::seed_if_empty(&store).unwrap();
    // Lift only the transport size cap. The read is still the same plain
    // catalog.get that parses, renders, and merges the whole seeded store.
    let mut served = serve_catalog_get_with_limits(&store, b"{}", usize::MAX, usize::MAX).unwrap();
    let mut upstream = crate::seed::upstream_state().unwrap();
    upstream.resolved_at_ms = served.resolved_at_ms;
    // A fresh store has operator claims from the historical schema import.
    // Pass them separately: they are not facts in the upstream models.dev seed.
    let quality = read_quality_at(&store, None).unwrap();
    let mut published = curate(upstream, &crate::curation::embedded(), &quality);
    assert_eq!(published.models.len(), served.models.len());
    for (id, facts) in std::mem::take(&mut published.models) {
        let actual = served
            .models
            .remove(&id)
            .unwrap_or_else(|| panic!("no catalog.get row for {id}"));
        assert_eq!(
            serde_json::to_string(&facts).unwrap(),
            serde_json::to_string(&actual).unwrap(),
            "model {id}"
        );
    }
    assert!(served.models.is_empty());
    assert_eq!(
        serde_json::to_value(published).unwrap(),
        serde_json::to_value(served).unwrap()
    );
}
