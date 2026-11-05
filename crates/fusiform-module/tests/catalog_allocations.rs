//! Resource regression for the store's catalog-sized read buffers.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use cortexkit_store_types::{Isolation, StorageBackend, StorageDescriptor};
use fusiform_core::SourceId;
use fusiform_store::{serve::CatalogQuery, CatalogStore};

struct MeasureSystem;
static LARGEST: AtomicUsize = AtomicUsize::new(0);

// Only this isolated test binary wraps the system allocator. Production keeps
// its allocator unchanged; the counter observes individual allocation requests.
unsafe impl GlobalAlloc for MeasureSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        // SAFETY: forward the allocator's unchanged layout to the system.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the pointer and layout came from the same system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        LARGEST.fetch_max(size, Ordering::Relaxed);
        // SAFETY: preserve the allocation identity and requested new size.
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: MeasureSystem = MeasureSystem;

fn assert_bounded(operation: &str) {
    let largest = LARGEST.load(Ordering::Relaxed);
    assert!(
        largest <= 8 * 1024 * 1024,
        "{operation} allocated {largest} bytes in one request"
    );
}

#[test]
fn catalog_rows_and_digest_avoid_large_allocations() {
    let dir = tempfile::tempdir().unwrap();
    let store = CatalogStore::open(&StorageDescriptor {
        module_id: "fusiform".into(),
        storage_namespace: "default".into(),
        isolation: Isolation::Module,
        backend: StorageBackend::Sqlite {
            path: dir.path().join("store.db").to_string_lossy().into_owned(),
        },
    })
    .unwrap();
    fusiform_module::seed::seed_if_empty(&store).unwrap();

    LARGEST.store(0, Ordering::Relaxed);
    let snapshot = store
        .read_catalog(&CatalogQuery::current(SourceId::ModelsDev))
        .unwrap();
    assert!(snapshot.fact_count() > 150_000);
    assert_bounded("read_catalog");
    drop(snapshot);

    LARGEST.store(0, Ordering::Relaxed);
    let current = store.current_values(SourceId::ModelsDev).unwrap();
    assert!(current.len() > 8_000);
    assert_bounded("current_values");
    drop(current);

    let catalog = fusiform_core::normalize::normalize_models_dev(include_bytes!(
        "../data/models-dev-seed.json"
    ))
    .unwrap()
    .catalog;
    LARGEST.store(0, Ordering::Relaxed);
    let digest = fusiform_store::ingest::catalog_digest(&catalog);
    assert_eq!(digest.len(), 64);
    assert_bounded("catalog_digest");
}
