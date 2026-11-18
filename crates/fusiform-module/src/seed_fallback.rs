use std::{path::PathBuf, sync::Arc};

use cortexkit_store_types::{StorageBackend, StorageDescriptor};
use fusiform_module::{cache_lifetimes, curated_models, fact_overrides, plan_prices, seed};
use fusiform_store::CatalogStore;

pub(crate) const OFFLINE_REASON: &str = "serving the built-in snapshot; the store was not reached";

pub(crate) struct SeedFallback {
    pub(crate) store: Arc<CatalogStore>,
    _directory: Cleanup,
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Err(error) = remove(&self.0) {
            eprintln!("fusiform: cannot remove built-in snapshot fallback: {error}");
        }
    }
}

fn remove(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

impl SeedFallback {
    pub(crate) fn prepare(
        descriptor: &StorageDescriptor,
        build: bool,
    ) -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let StorageBackend::Sqlite { path } = &descriptor.backend else {
            return Ok(None);
        };
        let parent = std::path::Path::new(path)
            .parent()
            .ok_or("store has no parent folder")?;
        let folder = parent.join("seed-fallback");
        remove(&folder)?;
        if !build {
            return Ok(None);
        }
        std::fs::create_dir(&folder)?;
        let cleanup = Cleanup(folder);
        let mut descriptor = descriptor.clone();
        descriptor.storage_namespace = format!("{}-seed-fallback", descriptor.storage_namespace);
        descriptor.backend = StorageBackend::Sqlite {
            path: cleanup
                .0
                .join("seed-fallback.db")
                .to_string_lossy()
                .into_owned(),
        };
        let store = CatalogStore::open(&descriptor)?;
        seed::seed_if_empty(&store)?;
        let now = crate::now_ms();
        store.append_plan_prices(&plan_prices::store_rows(plan_prices::rows()))?;
        curated_models::apply_to_store(&store, curated_models::table(), now)?;
        fact_overrides::apply_to_store(&store, fact_overrides::overrides(), now)?;
        store.append_cache_lifetimes(
            &cache_lifetimes::stored_rules(cache_lifetimes::rules()),
            now,
        )?;
        store.advance_catalog_version_at_start(fusiform_core::Timestamp(now))?;
        Ok(Some(Self {
            store: Arc::new(store),
            _directory: cleanup,
        }))
    }
}

pub(crate) fn mutating_tool(body: &[u8]) -> Option<String> {
    let raw: serde_json::Value = serde_json::from_slice(body).ok()?;
    let name = raw.get("name")?.as_str()?;
    let mutating = fusiform_module::manifest()
        .provides
        .into_iter()
        .any(|role| {
            let subc_protocol::manifest::ProviderRole::ToolProvider { tools, .. } = role else {
                return false;
            };
            tools.iter().any(|tool| {
                tool.name == name
                    && tool.execution_mode == subc_protocol::manifest::ExecutionMode::Mutating
            })
        });
    mutating.then(|| name.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_store_types::Isolation;

    #[test]
    fn store_offline_fallback_rebuilds_and_is_removed_after_handover() {
        let dir = tempfile::tempdir().unwrap();
        let descriptor = StorageDescriptor {
            module_id: "fusiform".into(),
            storage_namespace: "default".into(),
            isolation: Isolation::Module,
            backend: StorageBackend::Sqlite {
                path: dir
                    .path()
                    .join("selected.db")
                    .to_string_lossy()
                    .into_owned(),
            },
        };
        let selected = CatalogStore::open_for_store(&descriptor).unwrap();
        let folder = dir.path().join("seed-fallback");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(
            folder.join("seed-fallback.db"),
            b"partial database from interrupted startup",
        )
        .unwrap();

        let seed = SeedFallback::prepare(&descriptor, true).unwrap().unwrap();
        let seeded = fusiform_module::route::serve_catalog_get(
            &seed.store,
            br#"{"provider_id":"anthropic","model_id":"claude-sonnet-4-5"}"#,
        )
        .unwrap();
        assert!(seeded.models.contains_key("anthropic/claude-sonnet-4-5"));
        assert_eq!(selected.catalog_version().unwrap(), 0);
        assert!(selected.latest_store_release().unwrap().is_none());
        assert_eq!(
            mutating_tool(br#"{"name":"quality.restore"}"#).as_deref(),
            Some("quality.restore")
        );
        assert_eq!(mutating_tool(br#"{"name":"catalog.status"}"#), None);

        let base = fusiform_store::ImportedTables::new();
        selected
            .apply_store_releases(
                Some(&base),
                &[fusiform_store::ReleaseArrival {
                    release: fusiform_store::StoreRelease {
                        release: 1,
                        state_hash: "a".repeat(64),
                        curation_hash: "b".repeat(64),
                        applied_at_ms: i64::MAX / 2,
                    },
                    rows: fusiform_store::ImportedTables::new(),
                }],
            )
            .unwrap();
        drop(seed);
        assert!(!folder.exists());
        assert!(SeedFallback::prepare(&descriptor, false).unwrap().is_none());
        assert_eq!(selected.latest_store_release().unwrap().unwrap().release, 1);
    }
}
