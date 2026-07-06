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

use super::ssrf::{validate_http_url, HttpAccessPolicy};

/// How a mosaic reaches its assets. The two modes are mutually exclusive per
/// pool: HTTP (each asset key is an HTTP URL, the default) or one
/// caller-injected `object_store` backend (each key is then a *decoded object
/// key* within that single store — single server, single bucket; credentials
/// and endpoint are the caller's concern, never held by the library).
#[non_exhaustive]
#[derive(Clone, Debug, Default)]
pub enum Backend {
    /// Open each asset over HTTP(S); asset keys are full URLs.
    #[default]
    Http,
    /// Open every asset through this `object_store` backend (S3/MinIO/local/…);
    /// asset keys are decoded object keys, e.g. `"scenes/2026/tile.tif"`.
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
    /// SSRF guard applied to HTTP asset URLs before they are opened. Ignored
    /// for [`Backend::Store`] (keys there are object keys, not URLs, within a
    /// caller-controlled store).
    http_policy: HttpAccessPolicy,
}

/// Builds a [`ReaderPool`]. Both knobs are optional: the default backend is
/// [`Backend::Http`] and the default block cache is a fresh
/// `BlockCache::new(CacheConfig::default())` private to this pool.
pub struct ReaderPoolBuilder {
    capacity: u64,
    block_cache: Option<Arc<BlockCache>>,
    backend: Backend,
    http_policy: HttpAccessPolicy,
}

impl ReaderPoolBuilder {
    /// Use a caller-supplied shared [`BlockCache`], so the pool's decoded
    /// blocks live in (and are bounded by) the same cache the caller can
    /// observe via [`BlockCache::stats`] and share with other readers
    /// (keyed by `source_id`).
    pub fn block_cache(mut self, block_cache: Arc<BlockCache>) -> Self {
        self.block_cache = Some(block_cache);
        self
    }

    /// How assets are opened — see [`Backend`]. With [`Backend::Store`], each
    /// key passed to [`ReaderPool::get_or_open`] is a *decoded object key*
    /// within that single store (e.g. `"scenes/2026/tile.tif"`), not an HTTP
    /// URL. The library never holds credentials — wire them into the store.
    pub fn backend(mut self, backend: Backend) -> Self {
        self.backend = backend;
        self
    }

    /// SSRF policy for HTTP asset URLs — see [`HttpAccessPolicy`]. The default
    /// ([`HttpAccessPolicy::default`]) blocks loopback and link-local (the
    /// cloud-metadata endpoint) while allowing private ranges and plaintext
    /// HTTP for internal object stores.
    pub fn http_policy(mut self, policy: HttpAccessPolicy) -> Self {
        self.http_policy = policy;
        self
    }

    pub fn build(self) -> ReaderPool {
        ReaderPool {
            cache: moka::future::Cache::new(self.capacity),
            block_cache: self
                .block_cache
                .unwrap_or_else(|| Arc::new(BlockCache::new(CacheConfig::default()))),
            cpu: CpuLimiter::default(),
            backend: self.backend,
            http_policy: self.http_policy,
        }
    }
}

impl ReaderPool {
    /// Start building a pool that keeps at most `capacity` opened readers.
    pub fn builder(capacity: u64) -> ReaderPoolBuilder {
        ReaderPoolBuilder {
            capacity,
            block_cache: None,
            backend: Backend::Http,
            http_policy: HttpAccessPolicy::default(),
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
        // SSRF guard on the miss path (cached readers already passed it): for
        // HTTP asset URLs from untrusted mosaic/STAC documents, validate scheme
        // and resolved host before opening. Store-backend keys are object keys
        // within a caller-controlled store, so they are exempt.
        if matches!(self.backend, Backend::Http) {
            validate_http_url(key, &self.http_policy)
                .await
                .map_err(|e| AgError::Mosaic(format!("open asset {}: {e}", redact_query(key))))?;
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
                let builder = CogReader::builder().reader_config(cfg);
                let r = match backend {
                    Backend::Store(store) => builder.open_object_store(store, &key_owned).await?,
                    Backend::Http => builder.open_http(&key_owned).await?,
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
        // in the error text; the path part must, for diagnosability. Use a
        // permissive policy so this exercises the connect-failure path (the
        // SSRF guard has its own tests in `super::ssrf`).
        let pool = ReaderPool::builder(8)
            .http_policy(HttpAccessPolicy::permissive())
            .build();
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
        let pool = ReaderPool::builder(8)
            .http_policy(HttpAccessPolicy::permissive())
            .build();
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
    async fn default_policy_blocks_loopback_asset() {
        // Default pool policy must reject a loopback asset URL (SSRF guard)
        // before ever attempting to connect, as a redacted Mosaic error.
        let pool = ReaderPool::builder(8).build();
        let msg = match pool
            .get_or_open("http://127.0.0.1:9000/x.tif?X-Amz-Signature=s3cr3t", "n")
            .await
        {
            Err(e) => e.to_string(),
            Ok(_) => panic!("loopback asset must be rejected by default policy"),
        };
        assert!(msg.contains("blocked"), "expected policy rejection: {msg}");
        assert!(!msg.contains("s3cr3t"), "credentials leaked: {msg}");
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
        let pool = ReaderPool::builder(8)
            .backend(Backend::Store(store))
            .build();
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
