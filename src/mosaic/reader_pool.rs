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

/// How the pool reaches its assets. The two modes are mutually exclusive per
/// pool: HTTP (each key is an HTTP URL, the default) or one caller-injected
/// `object_store` backend (each key is a *decoded object key* within that
/// single store — single server, single bucket; credentials and endpoint are
/// the caller's concern, never held by the library).
#[derive(Clone)]
enum Backend {
    Http,
    Store(Arc<dyn ObjectStore>),
}

/// Strip the query string from a URL for error messages and logs: presigned
/// URLs and `?token=…` style auth carry live credentials that must not leak
/// into log lines or error strings (which often end up in HTTP responses).
/// Object keys have no query component, so this is a no-op for store mode.
pub(crate) fn redact_query(addr: &str) -> &str {
    &addr[..addr.find('?').unwrap_or(addr.len())]
}

pub struct ReaderPool {
    cache: moka::future::Cache<String, Arc<CogReader>>,
    block_cache: Arc<BlockCache>,
    /// One CPU-job limiter shared by every reader the pool opens, so the whole
    /// mosaic (all assets + their tilers + the final composite render) is bound
    /// to a single CPU budget instead of N independent per-reader semaphores.
    cpu: CpuLimiter,
    backend: Backend,
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
            backend: Backend::Http,
        }
    }

    /// Like [`Self::new`] but opens every asset through one caller-injected
    /// `object_store` backend (S3/MinIO/local/…) instead of HTTP. Each key
    /// passed to [`Self::get_or_open`] is then a *decoded object key* within
    /// that single store (e.g. `"scenes/2026/tile.tif"`), not an HTTP URL.
    /// The library never holds credentials — wire them into `store` yourself.
    pub fn with_object_store(capacity: u64, store: Arc<dyn ObjectStore>) -> Self {
        Self::with_object_store_and_block_cache(
            capacity,
            store,
            Arc::new(BlockCache::new(CacheConfig::default())),
        )
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
            backend: Backend::Store(store),
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
    /// `key` is an HTTP URL; in object_store mode it is a decoded object key
    /// within the injected store. Concurrent first-opens for
    /// the same `key` are coalesced by moka. The caller-supplied `source_id` is
    /// the block-cache identity for this COG — reuse the same string for the
    /// same COG (here and on any single-dataset reader) to share decoded blocks.
    /// All readers share the pool's single [`BlockCache`] and [`CpuLimiter`].
    pub async fn get_or_open(&self, key: &str, source_id: &str) -> AgResult<Arc<CogReader>> {
        // Fast path: hits dominate in steady state (adjacent tiles reuse the
        // same COG), so look up by borrowed key before allocating anything.
        if let Some(r) = self.cache.get(key).await {
            return Ok(r);
        }
        let key_owned = key.to_string();
        let source_id = source_id.to_string();
        let block_cache = Arc::clone(&self.block_cache);
        let cpu = self.cpu.clone();
        let backend = self.backend.clone();
        self.cache
            .try_get_with(key_owned.clone(), async move {
                let cfg = ReaderConfig {
                    source_id,
                    block_cache,
                    cpu_limiter: Some(cpu),
                    ..ReaderConfig::default()
                };
                let r = match backend {
                    Backend::Store(store) => {
                        CogReader::open_object_store_with_config(store, &key_owned, cfg).await?
                    }
                    Backend::Http => {
                        CogReader::open_http_with_config_default(&key_owned, cfg).await?
                    }
                };
                Ok::<_, AgError>(Arc::new(r))
            })
            .await
            .map_err(|e: Arc<AgError>| {
                AgError::Mosaic(format!("open asset {}: {e}", redact_query(key)))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_query_strips_credentials() {
        assert_eq!(
            redact_query("https://h/x.tif?X-Amz-Signature=s3cr3t"),
            "https://h/x.tif"
        );
        assert_eq!(redact_query("https://h/x.tif"), "https://h/x.tif");
        assert_eq!(redact_query("scenes/a.tif"), "scenes/a.tif");
        assert_eq!(redact_query(""), "");
        assert_eq!(redact_query("?token=t"), "");
    }

    #[tokio::test]
    async fn open_failure_redacts_query_string() {
        // The query string (where presigned credentials live) must not appear
        // in the error text; the path part must, for diagnosability.
        let pool = ReaderPool::new(8);
        let msg = match pool
            .get_or_open("http://127.0.0.1:1/nope.tif?X-Amz-Signature=s3cr3t", "n")
            .await
        {
            Err(e) => e.to_string(),
            Ok(_) => panic!("unreachable URL must fail"),
        };
        assert!(
            !msg.contains("s3cr3t"),
            "credentials leaked into error: {msg}"
        );
        assert!(msg.contains("nope.tif"), "path missing from error: {msg}");
    }

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
