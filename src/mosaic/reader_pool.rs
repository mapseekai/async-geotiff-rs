//! URL-keyed cache of opened `CogReader`s. Opening a COG = an HTTP range
//! read of the TIFF footer + IFD parse; the same COG is reused by many
//! adjacent tiles, so cache the opened reader keyed by its address.
//!
//! All readers in the pool share **one** [`BlockCache`] so decoded tile
//! blocks from the same asset are reused across reader instances, and the
//! total memory budget is bounded by a single `CacheConfig` rather than
//! multiplied by the number of open assets.

use std::sync::Arc;

use object_store::ObjectStore;

use crate::blocking::CpuLimiter;
use crate::error::{AgError, AgResult};
use crate::io::{BlockCache, CacheConfig, CogReader, ReaderConfig};

pub struct ReaderPool {
    cache: moka::future::Cache<String, Arc<CogReader>>,
    block_cache: Arc<BlockCache>,
    /// One CPU-job limiter shared by every reader the pool opens, so the whole
    /// mosaic (all assets + their tilers + the final composite render) is bound
    /// to a single CPU budget instead of N independent per-reader semaphores.
    cpu: CpuLimiter,
    /// Backing store for assets. `None` = HTTP mode (each key is an HTTP URL,
    /// the default, fully backward compatible). `Some(store)` = object_store
    /// mode: every key is opened as a *decoded object key* against this one
    /// caller-injected store (single server, single bucket — credentials and
    /// endpoint are the caller's concern, never held by the library).
    store: Option<Arc<dyn ObjectStore>>,
}

impl ReaderPool {
    pub fn new(capacity: u64) -> Self {
        Self::with_block_cache(capacity, Arc::new(BlockCache::new(CacheConfig::default())))
    }

    /// Like [`Self::new`] but uses a caller-supplied shared block cache, so the
    /// pool's decoded blocks live in (and are bounded by) the same cache the
    /// caller can observe via [`BlockCache::stats`] and share with other readers.
    pub fn with_block_cache(capacity: u64, block_cache: Arc<BlockCache>) -> Self {
        Self {
            cache: moka::future::Cache::new(capacity),
            block_cache,
            cpu: CpuLimiter::default(),
            store: None,
        }
    }

    /// Like [`Self::new`] but opens every asset through one caller-injected
    /// `object_store` backend (S3/MinIO/local/…) instead of HTTP. Each key
    /// passed to [`Self::get_or_open`] is then a *decoded object key* within
    /// that single store (e.g. `"scenes/2026/tile.tif"`), not an HTTP URL.
    /// The library never holds credentials — wire them into `store` yourself.
    pub fn with_object_store(capacity: u64, store: Arc<dyn ObjectStore>) -> Self {
        Self {
            cache: moka::future::Cache::new(capacity),
            block_cache: Arc::new(BlockCache::new(CacheConfig::default())),
            cpu: CpuLimiter::default(),
            store: Some(store),
        }
    }

    /// [`Self::with_object_store`] plus a caller-supplied shared [`BlockCache`],
    /// so object_store-backed assets share one decode-memory budget with the
    /// caller's other readers (keyed by `source_id`).
    pub fn with_object_store_and_block_cache(
        capacity: u64,
        store: Arc<dyn ObjectStore>,
        block_cache: Arc<BlockCache>,
    ) -> Self {
        Self {
            cache: moka::future::Cache::new(capacity),
            block_cache,
            cpu: CpuLimiter::default(),
            store: Some(store),
        }
    }

    /// The shared block cache backing this pool (clone of the `Arc`).
    pub fn block_cache(&self) -> Arc<BlockCache> {
        Arc::clone(&self.block_cache)
    }

    /// The shared CPU-job limiter backing every reader in this pool. The mosaic
    /// final render acquires from it too, so all CPU work shares one budget.
    pub fn cpu_limiter(&self) -> CpuLimiter {
        self.cpu.clone()
    }

    /// Get the cached reader for `key`, opening it on first use. In HTTP mode
    /// (`store == None`) `key` is an HTTP URL; in object_store mode it is a
    /// decoded object key within the injected store. Concurrent first-opens for
    /// the same `key` are coalesced by moka. The caller-supplied `source_id` is
    /// the block-cache identity for this COG — reuse the same string for the
    /// same COG (here and on any single-dataset reader) to share decoded blocks.
    /// All readers share the pool's single [`BlockCache`] and [`CpuLimiter`].
    pub async fn get_or_open(&self, key: &str, source_id: &str) -> AgResult<Arc<CogReader>> {
        let key_owned = key.to_string();
        let source_id = source_id.to_string();
        let block_cache = Arc::clone(&self.block_cache);
        let cpu = self.cpu.clone();
        let store = self.store.clone();
        self.cache
            .try_get_with(key_owned.clone(), async move {
                let cfg = ReaderConfig {
                    source_id,
                    block_cache,
                    cpu_limiter: Some(cpu),
                    ..ReaderConfig::default()
                };
                let r = match store {
                    Some(store) => {
                        CogReader::open_object_store_with_config(store, &key_owned, cfg).await?
                    }
                    None => CogReader::open_http_with_config_default(&key_owned, cfg).await?,
                };
                Ok::<_, AgError>(Arc::new(r))
            })
            .await
            .map_err(|e: Arc<AgError>| AgError::Mosaic(format!("open asset {key}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_failure_is_mosaic_error() {
        let pool = ReaderPool::new(8);
        let result = pool
            .get_or_open("http://127.0.0.1:1/nope.tif", "nope")
            .await;
        match result {
            Err(AgError::Mosaic(_)) => {}
            Err(other) => {
                panic!("expected AgError::Mosaic, got a different error variant: {other}")
            }
            Ok(_) => panic!("expected error opening unreachable URL, but got Ok"),
        }
    }

    #[tokio::test]
    async fn injected_store_is_used() {
        use object_store::local::LocalFileSystem;

        // The key is a *decoded object key* (not an HTTP URL). HTTP mode would
        // fail to open it; success here proves the injected store path is taken.
        let key = "data/cog_uint8_rgb_nodata.tif";
        if !std::path::Path::new(key).exists() {
            eprintln!("fixture missing, skipping injected_store_is_used");
            return;
        }
        let store = Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        let pool = ReaderPool::with_object_store(8, store);
        let reader = pool
            .get_or_open(key, key)
            .await
            .expect("store-backed open must succeed");
        assert!(
            reader.wgs84_bounds().is_ok(),
            "store-opened reader must expose bounds"
        );
    }
}
