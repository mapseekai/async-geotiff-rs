//! URL-keyed cache of opened `CogReader`s. Opening a COG = an HTTP range
//! read of the TIFF footer + IFD parse; the same COG is reused by many
//! adjacent tiles, so cache the opened reader keyed by its address.
//!
//! All readers in the pool share **one** [`BlockCache`] so decoded tile
//! blocks from the same asset are reused across reader instances, and the
//! total memory budget is bounded by a single `CacheConfig` rather than
//! multiplied by the number of open assets.

use std::sync::Arc;

use crate::error::{AgError, AgResult};
use crate::io::{BlockCache, CacheConfig, CogReader, ReaderConfig};

pub struct ReaderPool {
    cache: moka::future::Cache<String, Arc<CogReader>>,
    block_cache: Arc<BlockCache>,
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
        }
    }

    /// The shared block cache backing this pool (clone of the `Arc`).
    pub fn block_cache(&self) -> Arc<BlockCache> {
        Arc::clone(&self.block_cache)
    }

    /// Get the cached reader for `url`, opening it (over HTTP) on first use.
    /// Concurrent first-opens for the same URL are coalesced by moka. The
    /// caller-supplied `source_id` is the block-cache identity for this COG —
    /// reuse the same string for the same COG (here and on any single-dataset
    /// reader) to share decoded blocks. All readers share the pool's single
    /// [`BlockCache`].
    pub async fn get_or_open(&self, url: &str, source_id: &str) -> AgResult<Arc<CogReader>> {
        let url_owned = url.to_string();
        let source_id = source_id.to_string();
        let block_cache = Arc::clone(&self.block_cache);
        self.cache
            .try_get_with(url_owned.clone(), async move {
                let cfg = ReaderConfig {
                    source_id,
                    block_cache,
                    ..ReaderConfig::default()
                };
                let r = CogReader::open_http_with_config_default(&url_owned, cfg).await?;
                Ok::<_, AgError>(Arc::new(r))
            })
            .await
            .map_err(|e: Arc<AgError>| AgError::Mosaic(format!("open asset {url}: {e}")))
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
}
