//! Shared decoded-block cache.
//!
//! A single `BlockCache` can be shared across multiple readers via `Arc<BlockCache>`.
//! It caches decoded `async_tiff::Array` tiles under a configurable byte budget
//! using moka's TinyLFU policy.

use std::future::Future;
use std::sync::Arc;

use async_tiff::Array;

use crate::error::AgError;
use crate::io::cache::CacheConfig;

// ── BlockKind ───────────────────────────────────────────────────────────────

/// Whether a block carries image data or an alpha/validity mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockKind {
    Data,
    Mask,
}

// ── BlockKey ────────────────────────────────────────────────────────────────

/// Cache key identifying a single decoded TIFF block.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlockKey {
    /// Source URL / path of the COG.
    pub source: Arc<str>,
    /// Overview level index (0 = full resolution).
    pub level: u32,
    /// Data or mask.
    pub kind: BlockKind,
    /// Tile column index.
    pub tx: u32,
    /// Tile row index.
    pub ty: u32,
}

// ── DecodedBlock ────────────────────────────────────────────────────────────

/// A decoded TIFF block together with its pre-computed resident byte size.
///
/// The weight is computed once at construction time so the moka weigher can
/// call it without re-traversing the array.
pub struct DecodedBlock {
    array: Array,
    weight: u32,
}

impl DecodedBlock {
    /// Wrap `array` and pre-compute its byte weight.
    ///
    /// Weight = element count × bytes-per-element, saturating into `u32`.
    pub fn new(array: Array) -> Self {
        let elems = array.data().len();
        let elem_bytes = array.data_type().map(|dt| dt.size()).unwrap_or(1);
        let weight = elems
            .saturating_mul(elem_bytes)
            .try_into()
            .unwrap_or(u32::MAX);
        Self { array, weight }
    }

    /// Resident byte size of this block (used by moka weigher).
    pub fn weight(&self) -> u32 {
        self.weight
    }

    /// Access the underlying decoded array.
    pub fn array(&self) -> &Array {
        &self.array
    }
}

// ── BlockCache ──────────────────────────────────────────────────────────────

/// Shared, byte-bounded cache of decoded TIFF blocks.
///
/// `BlockCache` is cheaply cloneable (`Clone` delegates to the inner
/// `moka::future::Cache` which is `Arc`-backed).
#[derive(Clone)]
pub struct BlockCache {
    inner: moka::future::Cache<BlockKey, Arc<DecodedBlock>>,
}

impl BlockCache {
    /// Build a new cache honoring `cfg`.
    pub fn new(cfg: CacheConfig) -> Self {
        let mut builder = moka::future::Cache::builder()
            .max_capacity(cfg.max_bytes)
            .weigher(|_k: &BlockKey, v: &Arc<DecodedBlock>| v.weight());
        if let Some(ttl) = cfg.ttl {
            builder = builder.time_to_live(ttl);
        }
        Self {
            inner: builder.build(),
        }
    }

    /// Return the cached block for `key`, calling `init` exactly once when
    /// there is no live entry (concurrent requests are coalesced).
    ///
    /// `init` should produce a raw `DecodedBlock`; this method wraps it in
    /// `Arc` before insertion so that all callers receive `Arc`-equal
    /// pointers on a cache hit.
    pub async fn try_get_with(
        &self,
        key: BlockKey,
        init: impl Future<Output = Result<DecodedBlock, AgError>>,
    ) -> Result<Arc<DecodedBlock>, AgError> {
        self.inner
            .try_get_with(key, async move { init.await.map(Arc::new) })
            .await
            .map_err(|e: Arc<AgError>| AgError::Tiff(format!("block decode: {e}")))
    }

    /// Probe the cache without populating it.
    pub async fn get(&self, key: &BlockKey) -> Option<Arc<DecodedBlock>> {
        self.inner.get(key).await
    }

    /// Flush pending internal tasks and return the number of live entries.
    ///
    /// `moka`'s `entry_count` is eventually consistent; calling
    /// `run_pending_tasks` first makes it deterministic in unit tests.
    ///
    /// Intended for tests and diagnostics only; do not call on hot paths.
    pub async fn sync_count(&self) -> u64 {
        self.inner.run_pending_tasks().await;
        self.inner.entry_count()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use async_tiff::metadata::{cache::ReadaheadMetadataCache, TiffMetadataReader};
    use async_tiff::reader::ObjectReader;
    use async_tiff::{DataType, TIFF};
    use object_store::local::LocalFileSystem;
    use object_store::path::Path;

    use super::*;
    use crate::error::AgError;
    use crate::io::cache::CacheConfig;

    /// Open the AOT.tif fixture (UInt16, Deflate) and decode tile (0, 0).
    ///
    /// This is the only public path to an `async_tiff::Array` since
    /// `Array::try_new` is `pub(crate)` in async-tiff 0.3.x.
    ///
    /// Returns `None` when the fixture file is absent so tests can skip
    /// gracefully rather than hard-panicking in CI environments without data.
    async fn sample_array() -> Option<Array> {
        let tif = std::env::current_dir().unwrap().join("data/AOT.tif");
        if !tif.exists() {
            return None;
        }
        let fs = Arc::new(LocalFileSystem::new());
        let reader = ObjectReader::new(fs, Path::from_filesystem_path(&tif).unwrap());
        let md_cache = ReadaheadMetadataCache::new(reader.clone());
        let mut md = TiffMetadataReader::try_open(&md_cache).await.unwrap();
        let ifds = md.read_all_ifds(&md_cache).await.unwrap();
        let tiff = TIFF::new(ifds, md.endianness());
        let ifd = &tiff.ifds()[0];
        let tile = ifd.fetch_tile(0, 0, &reader).await.unwrap();
        Some(tile.decode(&Default::default()).unwrap())
    }

    fn make_key(source: &str) -> BlockKey {
        BlockKey {
            source: Arc::from(source),
            level: 0,
            kind: BlockKind::Data,
            tx: 0,
            ty: 0,
        }
    }

    fn default_cache() -> BlockCache {
        BlockCache::new(CacheConfig::default())
    }

    // ── Test 1 ──────────────────────────────────────────────────────────────

    /// `DecodedBlock::weight` must equal the raw byte count of the array data.
    ///
    /// We verify this two independent ways:
    ///  (a) `weight == elements × dtype_size` (the formula under test)
    ///  (b) `weight == TypedArray::as_ref().len()` (the actual byte buffer)
    ///
    /// With a UInt16 fixture the multiply is exercised: dropping the
    /// `× dtype.size()` factor would make (a) half of (b) and the test fails.
    #[tokio::test]
    async fn decoded_block_weight_is_byte_size() {
        let Some(array) = sample_array().await else {
            eprintln!("fixture missing, skipping");
            return;
        };

        // Verify we got a non-trivial UInt16 tile.
        assert_eq!(
            array.data_type(),
            Some(DataType::UInt16),
            "fixture must be UInt16 for this test to be meaningful"
        );
        let elems = array.data().len();
        assert!(elems > 0, "fixture tile must contain data");

        let block = DecodedBlock::new(array.clone());

        // (a) formula: elements × bytes-per-element
        let expected_via_formula = elems
            .saturating_mul(DataType::UInt16.size())
            .try_into()
            .unwrap_or(u32::MAX);
        assert_eq!(block.weight(), expected_via_formula);

        // (b) raw byte view (different code path)
        let raw_bytes: &[u8] = array.data().as_ref();
        assert_eq!(block.weight() as usize, raw_bytes.len());
    }

    // ── Test 2 ──────────────────────────────────────────────────────────────

    /// Two `try_get_with` calls with the same key must share one `Arc` and
    /// call the init closure exactly once.
    #[tokio::test]
    async fn same_key_returns_same_arc_and_decodes_once() {
        let cache = default_cache();
        let key = make_key("aot");
        let calls = Arc::new(AtomicUsize::new(0));

        let Some(array_a) = sample_array().await else {
            eprintln!("fixture missing, skipping");
            return;
        };
        let Some(array_b) = sample_array().await else {
            eprintln!("fixture missing, skipping");
            return;
        };

        let calls_a = Arc::clone(&calls);
        let a = cache
            .try_get_with(key.clone(), async move {
                calls_a.fetch_add(1, Ordering::SeqCst);
                Ok::<_, AgError>(DecodedBlock::new(array_a))
            })
            .await
            .unwrap();

        let calls_b = Arc::clone(&calls);
        let b = cache
            .try_get_with(key, async move {
                calls_b.fetch_add(1, Ordering::SeqCst);
                Ok::<_, AgError>(DecodedBlock::new(array_b))
            })
            .await
            .unwrap();

        assert!(Arc::ptr_eq(&a, &b), "both lookups must return the same Arc");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "init must be called exactly once"
        );
    }

    // ── Test 3 ──────────────────────────────────────────────────────────────

    /// Two keys differing only in `source` must produce distinct `Arc`s, each
    /// carrying data that matches the weight of its own array.
    #[tokio::test]
    async fn distinct_sources_do_not_collide() {
        let cache = default_cache();

        let Some(array1) = sample_array().await else {
            eprintln!("fixture missing, skipping");
            return;
        };
        let Some(array2) = sample_array().await else {
            eprintln!("fixture missing, skipping");
            return;
        };
        let expected_weight = DecodedBlock::new(array1.clone()).weight();

        let key1 = make_key("source-a");
        let key2 = make_key("source-b");

        let b1 = cache
            .try_get_with(
                key1,
                async move { Ok::<_, AgError>(DecodedBlock::new(array1)) },
            )
            .await
            .unwrap();

        let b2 = cache
            .try_get_with(
                key2,
                async move { Ok::<_, AgError>(DecodedBlock::new(array2)) },
            )
            .await
            .unwrap();

        assert!(
            !Arc::ptr_eq(&b1, &b2),
            "different sources must not alias the same Arc"
        );
        // Both tiles come from the same fixture so their weights should match.
        assert_eq!(b2.weight(), expected_weight, "second block weight mismatch");

        assert_eq!(cache.sync_count().await, 2, "cache should hold 2 entries");
    }
}
