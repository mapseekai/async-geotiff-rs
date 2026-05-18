//! URL-keyed cache of opened `CogReader`s. Opening a COG = an HTTP range
//! read of the TIFF footer + IFD parse; the same COG is reused by many
//! adjacent tiles, so cache the opened reader keyed by its address.

use std::sync::Arc;

use crate::error::{AgError, AgResult};
use crate::io::CogReader;

pub struct ReaderPool {
    cache: moka::future::Cache<String, Arc<CogReader>>,
}

impl ReaderPool {
    pub fn new(capacity: u64) -> Self {
        Self {
            cache: moka::future::Cache::new(capacity),
        }
    }

    /// Get the cached reader for `url`, opening it (over HTTP) on first use.
    /// Concurrent first-opens for the same URL are coalesced by moka.
    pub async fn get_or_open(&self, url: &str) -> AgResult<Arc<CogReader>> {
        let url_owned = url.to_string();
        self.cache
            .try_get_with(url_owned.clone(), async move {
                let r = CogReader::open_http(&url_owned).await?;
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
        let result = pool.get_or_open("http://127.0.0.1:1/nope.tif").await;
        match result {
            Err(AgError::Mosaic(_)) => {}
            Err(other) => {
                panic!("expected AgError::Mosaic, got a different error variant: {other}")
            }
            Ok(_) => panic!("expected error opening unreachable URL, but got Ok"),
        }
    }
}
