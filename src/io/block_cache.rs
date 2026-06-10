//! Shared decoded-block cache.
//!
//! A single `BlockCache` can be shared across multiple readers via `Arc<BlockCache>`.
//! It caches decoded `async_tiff::Array` tiles under a configurable byte budget
//! using moka's TinyLFU policy.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_tiff::Array;
use tokio::sync::watch;

use crate::error::AgError;
use crate::io::cache::CacheConfig;

/// Result of an in-flight block fetch+decode, cloneable so concurrent waiters
/// each get a copy. The error is `Arc`-wrapped because `AgError` is not `Clone`.
pub(crate) type BlockResult = Result<Arc<DecodedBlock>, Arc<AgError>>;

/// A receiver a concurrent caller awaits while another caller fetches the block.
pub(crate) type PendingBlock = watch::Receiver<Option<BlockResult>>;

/// Outcome of [`BlockCache::claim`]: either this caller owns the fetch (and must
/// `resolve` the returned guard), or another caller is already fetching it.
pub(crate) enum Claim {
    Owned(InflightGuard),
    Pending(PendingBlock),
}

/// Held by the caller that owns a block's fetch+decode. Dropping it without
/// [`resolve`](InflightGuard::resolve) closes the channel, so waiters observe a
/// cancellation and can retry, and the in-flight slot is always cleaned up.
pub(crate) struct InflightGuard {
    key: BlockKey,
    tx: watch::Sender<Option<BlockResult>>,
    inflight: Arc<Mutex<HashMap<BlockKey, PendingBlock>>>,
    resolved: bool,
}

impl InflightGuard {
    /// Publish the fetched+decoded result to any waiters and free the slot.
    pub(crate) fn resolve(mut self, result: BlockResult) {
        let _ = self.tx.send(Some(result));
        self.inflight.lock().unwrap().remove(&self.key);
        self.resolved = true;
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if !self.resolved {
            // Owner gave up (fetch/decode error or task abort): remove the slot;
            // dropping `tx` makes waiters' `changed()` return Err → they retry.
            self.inflight.lock().unwrap().remove(&self.key);
        }
    }
}

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

// ── Stats ───────────────────────────────────────────────────────────────────

/// Monotonic counters shared by all clones of a [`BlockCache`].
#[derive(Debug, Default)]
struct Counters {
    /// Probe (`get`) calls that found a live entry.
    hits: AtomicU64,
    /// Probe (`get`) calls that found nothing.
    misses: AtomicU64,
    /// Blocks actually decoded (one per `try_get_with` miss; single-flighted).
    decoded: AtomicU64,
    /// Blocks fetched from the object store. May exceed `decoded` when
    /// concurrent requests fetch the same cold block before either inserts it.
    fetched: AtomicU64,
}

/// Point-in-time snapshot of [`BlockCache`] activity, for metrics/monitoring.
///
/// `hits`/`misses`/`decoded`/`fetched` are monotonic since the cache was
/// created; `entries`/`resident_bytes` are the current (eventually-consistent)
/// cache occupancy. Poll this periodically and export to your metrics backend
/// (e.g. OpenTelemetry gauges/counters):
/// - hit ratio = `hits / (hits + misses)`,
/// - decode-saving = `1 - decoded / misses`,
/// - fetch amplification = `fetched / decoded` (1.0 = no duplicate fetches),
/// - `resident_bytes` vs the configured budget = memory pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockCacheStats {
    /// Probe hits (block already decoded and resident).
    pub hits: u64,
    /// Probe misses (block needed fetch + decode).
    pub misses: u64,
    /// Blocks decoded (single-flighted; ~= unique cold blocks).
    pub decoded: u64,
    /// Blocks fetched from the object store.
    pub fetched: u64,
    /// Current number of cached blocks (eventually consistent).
    pub entries: u64,
    /// Current resident byte size of cached blocks (eventually consistent).
    pub resident_bytes: u64,
}

/// Shared, byte-bounded cache of decoded TIFF blocks.
///
/// `BlockCache` is cheaply cloneable; clones share the same underlying cache
/// (the inner `moka::future::Cache` is `Arc`-backed) **and** the same stats
/// counters.
#[derive(Clone)]
pub struct BlockCache {
    inner: moka::future::Cache<BlockKey, Arc<DecodedBlock>>,
    counters: Arc<Counters>,
    /// In-flight compressed-block fetches, keyed identically to the decoded
    /// cache. Lets concurrent requests for the same cold block coalesce the
    /// network range GET (not just the decode, which moka already single-flights).
    inflight: Arc<Mutex<HashMap<BlockKey, PendingBlock>>>,
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
            counters: Arc::new(Counters::default()),
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Claim the right to fetch+decode `key`, or get a handle to await whoever
    /// already claimed it. The first caller for a cold block receives
    /// [`Claim::Owned`]; concurrent callers receive [`Claim::Pending`] and avoid
    /// issuing a duplicate network fetch.
    pub(crate) fn claim(&self, key: &BlockKey) -> Claim {
        let mut map = self.inflight.lock().unwrap();
        if let Some(rx) = map.get(key) {
            return Claim::Pending(rx.clone());
        }
        let (tx, rx) = watch::channel(None);
        map.insert(key.clone(), rx);
        Claim::Owned(InflightGuard {
            key: key.clone(),
            tx,
            inflight: Arc::clone(&self.inflight),
            resolved: false,
        })
    }

    /// Await an in-flight block another caller is fetching.
    ///
    /// Returns `Ok(Some(block))` on success, `Ok(None)` when the owner aborted
    /// without publishing (caller should retry with a fresh `claim`), or `Err`
    /// for decode errors forwarded by the owner.
    pub(crate) async fn await_pending(
        mut rx: PendingBlock,
    ) -> Result<Option<Arc<DecodedBlock>>, AgError> {
        if let Some(v) = rx.borrow().clone() {
            return v
                .map(Some)
                .map_err(|e| AgError::Tiff(format!("block decode: {e}")));
        }
        loop {
            if rx.changed().await.is_err() {
                // Sender dropped without publishing: owner was aborted (client
                // disconnect or task cancellation). Signal caller to retry.
                return Ok(None);
            }
            if let Some(v) = rx.borrow().clone() {
                return v
                    .map(Some)
                    .map_err(|e| AgError::Tiff(format!("block decode: {e}")));
            }
        }
    }

    /// Snapshot the cache's activity counters and current occupancy.
    ///
    /// Cheap and non-blocking; intended to be polled periodically by a metrics
    /// exporter. `entries`/`resident_bytes` come from moka and are eventually
    /// consistent.
    pub fn stats(&self) -> BlockCacheStats {
        BlockCacheStats {
            hits: self.counters.hits.load(Ordering::Relaxed),
            misses: self.counters.misses.load(Ordering::Relaxed),
            decoded: self.counters.decoded.load(Ordering::Relaxed),
            fetched: self.counters.fetched.load(Ordering::Relaxed),
            entries: self.inner.entry_count(),
            resident_bytes: self.inner.weighted_size(),
        }
    }

    /// Record that `n` blocks were fetched from the object store. Called by the
    /// reader after a coalesced fetch of cache misses.
    pub(crate) fn record_fetched(&self, n: u64) {
        self.counters.fetched.fetch_add(n, Ordering::Relaxed);
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
        // The init future runs only on a miss (moka single-flights it), so
        // counting here counts actual decodes, deduped across concurrent races.
        let counters = Arc::clone(&self.counters);
        self.inner
            .try_get_with(key, async move {
                let v = init.await.map(Arc::new);
                if v.is_ok() {
                    counters.decoded.fetch_add(1, Ordering::Relaxed);
                }
                v
            })
            .await
            .map_err(|e: Arc<AgError>| AgError::Tiff(format!("block decode: {e}")))
    }

    /// Existence check that does NOT touch the hit/miss counters. Used by the
    /// background prefetcher, whose probes would otherwise skew the cache
    /// stats that operators tune capacity against.
    pub(crate) fn peek(&self, key: &BlockKey) -> bool {
        self.inner.contains_key(key)
    }

    /// Probe the cache without populating it. Records a hit or miss.
    pub async fn get(&self, key: &BlockKey) -> Option<Arc<DecodedBlock>> {
        let hit = self.inner.get(key).await;
        let counter = if hit.is_some() {
            &self.counters.hits
        } else {
            &self.counters.misses
        };
        counter.fetch_add(1, Ordering::Relaxed);
        hit
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

    #[tokio::test]
    async fn claim_dedups_and_await_pending_receives_result() {
        let Some(array) = sample_array().await else {
            eprintln!("fixture missing, skipping");
            return;
        };
        let cache = default_cache();
        let key = make_key("inflight");

        // First claim owns the fetch; a concurrent claim is pending.
        let guard = match cache.claim(&key) {
            Claim::Owned(g) => g,
            Claim::Pending(_) => panic!("first claim must own the fetch"),
        };
        let pending = match cache.claim(&key) {
            Claim::Pending(rx) => rx,
            Claim::Owned(_) => panic!("second concurrent claim must be pending"),
        };

        let block = Arc::new(DecodedBlock::new(array));
        guard.resolve(Ok(Arc::clone(&block)));

        // The waiter receives the exact Arc the owner produced.
        let got = BlockCache::await_pending(pending).await.unwrap().unwrap();
        assert!(
            Arc::ptr_eq(&got, &block),
            "waiter must get the owner's block"
        );

        // Slot is freed after resolve → a later claim owns again.
        assert!(
            matches!(cache.claim(&key), Claim::Owned(_)),
            "resolved slot must be freed for re-claim"
        );
    }

    #[tokio::test]
    async fn dropped_guard_cancels_waiters_and_frees_slot() {
        let cache = default_cache();
        let key = make_key("inflight-drop");
        let guard = match cache.claim(&key) {
            Claim::Owned(g) => g,
            Claim::Pending(_) => unreachable!(),
        };
        let pending = match cache.claim(&key) {
            Claim::Pending(rx) => rx,
            Claim::Owned(_) => unreachable!(),
        };
        // Owner gives up without resolving (e.g. fetch error / task abort).
        drop(guard);
        assert!(
            BlockCache::await_pending(pending).await.unwrap().is_none(),
            "waiter must observe Ok(None) when owner drops (signal to retry)"
        );
        assert!(
            matches!(cache.claim(&key), Claim::Owned(_)),
            "cancelled slot must be freed for re-claim"
        );
    }

    #[tokio::test]
    async fn stats_track_hits_misses_decode_and_fetch() {
        let Some(array) = sample_array().await else {
            eprintln!("fixture missing, skipping");
            return;
        };
        let cache = BlockCache::new(CacheConfig::default());
        let key = make_key("s1");

        // Initial probe → miss.
        assert!(cache.get(&key).await.is_none());
        // Decode + insert (one decode).
        cache
            .try_get_with(key.clone(), async move {
                Ok::<_, AgError>(DecodedBlock::new(array))
            })
            .await
            .unwrap();
        // Two probes → two hits.
        assert!(cache.get(&key).await.is_some());
        assert!(cache.get(&key).await.is_some());
        // A probe for an absent key → miss.
        assert!(cache.get(&make_key("absent")).await.is_none());
        cache.record_fetched(3);

        // `entries`/`resident_bytes` are eventually consistent in moka; flush
        // pending tasks so they reflect the single insert deterministically.
        let _ = cache.sync_count().await;
        let s = cache.stats();
        assert_eq!(s.hits, 2, "hits");
        assert_eq!(s.misses, 2, "misses");
        assert_eq!(s.decoded, 1, "decoded once (single-flight init)");
        assert_eq!(s.fetched, 3, "fetched");
        assert_eq!(s.entries, 1, "one cached entry");
        assert!(s.resident_bytes > 0, "resident bytes after one insert");

        // Stats are shared across clones.
        assert_eq!(cache.clone().stats().hits, 2, "clone shares counters");
    }
}
