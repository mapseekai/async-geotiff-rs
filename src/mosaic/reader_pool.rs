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

use super::ssrf::{checked_http_store, HttpAccessPolicy};

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

static NEXT_POOL_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub struct ReaderPool {
    work_budget: crate::resource::WorkBudget,
    namespace: u64,
    reads: Arc<tokio::sync::Semaphore>,
    cache: moka::future::Cache<(String, String), Arc<CogReader>>,
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
    work_budget: crate::resource::WorkBudget,
    capacity: u64,
    block_cache: Option<Arc<BlockCache>>,
    backend: Backend,
    http_policy: HttpAccessPolicy,
}

impl ReaderPoolBuilder {
    /// Share active working-buffer accounting with mosaic/tiler work.
    pub fn work_budget(mut self, budget: crate::resource::WorkBudget) -> Self {
        self.work_budget = budget;
        self
    }

    /// Use a caller-supplied shared [`BlockCache`], so the pool's decoded
    /// blocks live in (and are bounded by) the same cache the caller can
    /// observe via [`BlockCache::stats`] and share with other readers
    /// (entries remain isolated by pool namespace).
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
            work_budget: self.work_budget,
            namespace: NEXT_POOL_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            reads: Arc::new(tokio::sync::Semaphore::new(8)),
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
            work_budget: crate::resource::WorkBudget::global(),
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

    /// Forget the cached reader so the next open discovers a fresh validator.
    /// Existing reader handles remain pinned to their original snapshot.
    pub async fn invalidate(&self, key: &str, source_id: &str) {
        self.cache
            .invalidate(&(key.to_owned(), source_id.to_owned()))
            .await;
    }

    /// Get the cached reader for `key`, opening it on first use. In HTTP mode
    /// `key` is an HTTP URL; in object_store mode it is a decoded object key
    /// within the injected store. Concurrent first-opens for
    /// the same `key` are coalesced by moka. The caller-supplied `source_id` is
    /// the immutable source version for this COG. Change it when bytes change.
    /// Cache identities also include the key and a private pool namespace;
    /// sharing a BlockCache shares its memory budget, not cross-pool entries.
    /// All readers share the pool's single [`BlockCache`] and [`CpuLimiter`].
    pub async fn get_or_open(&self, key: &str, source_id: &str) -> AgResult<Arc<CogReader>> {
        // Fast path: hits dominate in steady state (adjacent tiles reuse the
        // same COG), including its caller-supplied immutable version.
        let cache_key = (key.to_owned(), source_id.to_owned());
        if let Some(r) = self.cache.get(&cache_key).await {
            return Ok(r);
        }
        let key_owned = key.to_string();
        let source_id = format!(
            "pool:{}:{}:{}:{}",
            self.namespace,
            source_id.len(),
            source_id,
            key
        );
        let reads = self.reads.clone();
        let work_budget = self.work_budget.clone();
        let block_cache = Arc::clone(&self.block_cache);
        let cpu = self.cpu.clone();
        let backend = self.backend.clone();
        let http_policy = self.http_policy.clone();
        self.cache
            .try_get_with(cache_key, async move {
                let cfg = ReaderConfig {
                    work_budget,
                    redact_transport_errors: matches!(backend, Backend::Http),
                    source_id,
                    block_cache,
                    cpu_limiter: Some(cpu),
                    read_limiter: Some(reads),
                    ..ReaderConfig::default()
                };
                let builder = CogReader::builder().reader_config(cfg);
                let r = match backend {
                    Backend::Store(store) => builder.open_object_store(store, &key_owned).await?,
                    Backend::Http => {
                        let store = checked_http_store(&key_owned, &http_policy).await?;
                        // Complete URL, including encoded path and credentials,
                        // is the store base; no path reconstruction is needed.
                        builder.open_object_store(store, "").await.map_err(|e| {
                            // Transport errors may embed a normalized signed URL.
                            crate::io::snapshot::resource_error(&e)
                                .unwrap_or_else(|| AgError::Mosaic("HTTP asset read failed".into()))
                        })?
                    }
                };
                Ok::<_, AgError>(Arc::new(r))
            })
            .await
            .map_err(|e: Arc<AgError>| {
                if let Some(resource) = crate::io::snapshot::resource_error(e.as_ref()) {
                    return resource;
                }
                AgError::Mosaic(format!(
                    "open asset {}: {}",
                    redact_query(key),
                    e.to_string().replace(key, redact_query(key))
                ))
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
        let key = "tests/fixtures/rgb_nodata.tif";
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

    mod identity {
        use super::*;
        #[tokio::test]
        async fn audit_reader_identity_includes_source_version() {
            let store =
                Arc::new(object_store::local::LocalFileSystem::new_with_prefix(".").unwrap());
            let pool = ReaderPool::builder(8)
                .backend(Backend::Store(store))
                .build();
            let key = "tests/fixtures/rgb_nodata.tif";
            let first = pool.get_or_open(key, "version1").await.unwrap();
            let second = pool.get_or_open(key, "version2").await.unwrap();
            assert!(!Arc::ptr_eq(&first, &second));
        }
    }

    mod redirect {
        use super::*;
        #[tokio::test]
        async fn audit_redirect_cannot_escape_validated_host() {
            use axum::{response::Redirect, routing::get, Router};
            use std::sync::atomic::{AtomicUsize, Ordering};
            let hits = Arc::new(AtomicUsize::new(0));
            let capture = hits.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app =
                Router::new()
                    .route(
                        "/start",
                        get(move || async move {
                            Redirect::temporary(&format!("http://{addr}/blocked"))
                        }),
                    )
                    .route(
                        "/blocked",
                        get(move || {
                            let hits = capture.clone();
                            async move {
                                hits.fetch_add(1, Ordering::SeqCst);
                                "bad"
                            }
                        }),
                    );
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let pool = ReaderPool::builder(1)
                .http_policy(HttpAccessPolicy::default().allow_host("localhost"))
                .build();
            assert!(pool
                .get_or_open(
                    &format!("http://localhost:{}/start", addr.port()),
                    "redirect"
                )
                .await
                .is_err());
            server.abort();
            assert_eq!(
                hits.load(Ordering::SeqCst),
                0,
                "redirect contacted blocked destination"
            );
        }
    }

    mod namespace {
        use super::*;
        #[tokio::test]
        async fn audit_shared_cache_isolates_independent_pools() {
            let cache = Arc::new(BlockCache::new(CacheConfig::default()));
            let store =
                Arc::new(object_store::local::LocalFileSystem::new_with_prefix(".").unwrap());
            for _ in 0..2 {
                let pool = ReaderPool::builder(1)
                    .backend(Backend::Store(store.clone()))
                    .block_cache(cache.clone())
                    .build();
                pool.get_or_open("tests/fixtures/rgb_nodata.tif", "same-id")
                    .await
                    .unwrap()
                    .read_window(0, 0, 0, 16, 16)
                    .await
                    .unwrap();
            }
            assert_eq!(
                cache.stats().fetched,
                2,
                "independent pools must not assume source IDs share a namespace"
            );
        }
    }

    mod snapshot_refresh {
        use super::*;
        use object_store::{path::Path, ObjectStoreExt};
        #[tokio::test]
        async fn invalidation_opens_a_new_snapshot_without_reusing_old_blocks() {
            let store = Arc::new(object_store::memory::InMemory::new());
            let path = Path::from("changing.tif");
            let mut bytes = include_bytes!("../../tests/fixtures/rgb_nodata.tif").to_vec();
            store.put(&path, bytes.clone().into()).await.unwrap();
            let pool = ReaderPool::builder(2)
                .backend(Backend::Store(store.clone()))
                .build();
            let old = pool.get_or_open(path.as_ref(), "logical").await.unwrap();
            let old_pixel = old.read_window(0, 0, 0, 1, 1).await.unwrap();
            let ifd = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
            let n = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
            let entry = (0..n)
                .map(|i| ifd + 2 + 12 * i)
                .find(|&i| u16::from_le_bytes(bytes[i..i + 2].try_into().unwrap()) == 324)
                .unwrap();
            let offsets =
                u32::from_le_bytes(bytes[entry + 8..entry + 12].try_into().unwrap()) as usize;
            let pixel =
                u32::from_le_bytes(bytes[offsets..offsets + 4].try_into().unwrap()) as usize;
            bytes[pixel] = 150;
            store.put(&path, bytes.into()).await.unwrap();
            assert!(Arc::ptr_eq(
                &old,
                &pool.get_or_open(path.as_ref(), "logical").await.unwrap()
            ));
            assert!(old.read_window(0, 32, 0, 1, 1).await.is_err());
            pool.invalidate(path.as_ref(), "logical").await;
            let fresh = pool.get_or_open(path.as_ref(), "logical").await.unwrap();
            assert!(!Arc::ptr_eq(&old, &fresh));
            let new_pixel = fresh.read_window(0, 0, 0, 1, 1).await.unwrap();
            assert_ne!(old_pixel.pixels.to_f64(), new_pixel.pixels.to_f64());
        }
    }
}
