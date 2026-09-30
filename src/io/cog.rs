//! `CogReader` — async-tiff reader orchestration with GeoTransform/CRS derivation.

#[path = "cog_blocks.rs"]
mod blocks;

use blocks::{block_work_bytes, transport_error};
#[cfg(test)]
use blocks::{prefetch_blocks, ring_coords};

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use std::time::Duration;
#[cfg(feature = "perf-tracing")]
use std::time::Instant;

use crate::blocking::{default_blocking_permits, CpuLimiter};
use crate::bounds::{Bbox, GeoTransform};
use crate::crs::Crs;
use crate::error::{AgError, AgResult};
use crate::io::block_cache::{
    BlockCache, BlockKey, BlockKind, BlockResult, Claim, DecodedBlock, InflightGuard, PendingBlock,
};
use crate::io::cache::CacheConfig;
use crate::io::scale_offset::ScaleOffset;
use crate::io::window::{tile_range_for_window, validate_window_shape, IfdInfo, ValidatedIfdInfo};
#[cfg(feature = "perf-tracing")]
use crate::perf::duration_ms;
use crate::pixel::{ImageData, PixelData};
use async_tiff::metadata::{cache::ReadaheadMetadataCache, TiffMetadataReader};
use async_tiff::reader::ObjectReader;
use async_tiff::tags::PhotometricInterpretation;
use async_tiff::{ImageFileDirectory, TIFF};
use ndarray::Array2;
use object_store::http::HttpBuilder;
use object_store::path::Path;
use object_store::{ClientOptions, ObjectStore};

/// Client/TLS initialization is synchronous and may read platform trust stores.
/// Keep it off async workers, with capacity held until a cancelled job exits.
pub(crate) async fn build_http_transport<T: Send + 'static>(
    build: impl FnOnce() -> AgResult<T> + Send + 'static,
) -> AgResult<T> {
    static LIMITER: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let limiter = LIMITER
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(8)))
        .clone();
    let permit = limiter
        .acquire_owned()
        .await
        .map_err(|_| AgError::Tiff("HTTP construction limiter closed".into()))?;
    let request = crate::request::RequestContext::current();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        crate::request::RequestContext::run_blocking(request, build)
    })
    .await
    .map_err(|_| AgError::Tiff("HTTP construction task failed".into()))?
}

static NEXT_SOURCE_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
const TEST_OVERVIEW_PYRAMID: [f64; 4] = [10.0, 20.0, 40.0, 80.0];

/// Stitched outputs at or below this size are assembled inline on the async
/// thread instead of through `spawn_blocking`. Stitching is memcpy-bound and
/// output-size-proportional; for tile-size windows (a 256×256×3 u16 window +
/// mask is ~460 KiB) the copy costs tens of microseconds — less than the
/// dispatch + CPU-permit round-trip it would otherwise pay. 1 MiB keeps the
/// worst inline stall around ~100 µs, well under Tokio's blocking guidance.
const INLINE_STITCH_MAX_BYTES: u64 = 1 << 20;

/// Max concurrent background prefetch tasks per reader. When the budget is
/// exhausted, new windows simply skip prefetching — the foreground path is
/// never delayed by speculation.
const MAX_PREFETCH_TASKS: usize = 2;

/// Runtime limits for a [`CogReader`].
///
/// `max_decode_tasks` caps concurrent CPU-heavy decode/stitch jobs submitted
/// through `spawn_blocking`. Waiting for a permit is async, so excess requests
/// yield the Tokio worker instead of flooding the blocking pool.
///
/// `#[non_exhaustive]`: construct with [`ReaderConfig::default()`] and assign
/// fields (or use struct-update syntax inside this crate). This lets new fields
/// be added without breaking external callers.
#[derive(Clone)]
#[non_exhaustive]
pub struct ReaderConfig {
    /// Maximum IFDs and entries per IFD accepted before upstream allocations.
    pub max_ifds: usize,
    pub max_ifd_entries: u64,
    pub(crate) snapshot_size: Option<u64>,

    /// Strong validator required for managed sources by default. Opaque
    /// ObjectReader handles require explicit AssumeImmutable.
    pub snapshot_policy: crate::io::SnapshotPolicy,
    /// Maximum cumulative metadata bytes fetched while opening (default: 8 MiB).
    pub max_metadata_bytes: u64,

    /// Shared active-work byte allowance; completed caller-owned images and
    /// resident caches have separate ownership/budgets.
    pub work_budget: crate::resource::WorkBudget,
    pub(crate) redact_transport_errors: bool,
    /// Maximum pixels in a single source window (default: 16 million).
    pub max_window_pixels: u64,
    /// Maximum declared window data plus mask bytes, and compressed bytes
    /// per fetch batch (default: 256 MiB).
    pub max_window_bytes: u64,
    /// Maximum blocks requested by one window (default: 4096).
    pub max_window_blocks: usize,
    /// Maximum declared uncompressed bytes in any TIFF block (default: 64 MiB).
    pub max_decoded_block_bytes: u64,
    /// Concurrent foreground windows per reader (default: 8), clamped to
    /// `1..=tokio::sync::Semaphore::MAX_PERMITS`.
    pub max_concurrent_reads: usize,
    /// Optional shared foreground read limiter, e.g. across a mosaic pool.
    pub read_limiter: Option<Arc<tokio::sync::Semaphore>>,
    pub max_decode_tasks: usize,
    /// Caller-supplied id for this logical source (same id ⇒ shared blocks).
    pub source_id: String,
    /// Shared decoded-block cache injected by the caller.
    pub block_cache: Arc<BlockCache>,
    /// Shared CPU-job limiter. When `None`, the reader builds its own sized to
    /// `max_decode_tasks`. Inject the same [`CpuLimiter`] into several readers
    /// (and their [`Tiler`]s) to bound the whole process/mosaic to one CPU
    /// budget — see [`crate::blocking::CpuLimiter`]. [`Tiler`]: crate::tiler::Tiler
    pub cpu_limiter: Option<CpuLimiter>,
    /// Speculative block prefetch radius, in native COG blocks. After each
    /// `read_window`, the blocks in a ring of this width around the window
    /// are fetched + decoded into the shared block cache in the background —
    /// map-browsing patterns make those exactly the blocks the neighbouring
    /// XYZ tiles need next, turning their cold misses into warm hits.
    ///
    /// `0` (the default) disables prefetch. Each ring step issues extra
    /// object-store GETs, so leave it off for pay-per-request storage unless
    /// the latency win is worth it; `1` is the sweet spot for tile serving.
    pub prefetch_ring: u8,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            work_budget: crate::resource::WorkBudget::global(),
            max_ifds: 256,
            max_ifd_entries: 4096,
            snapshot_size: None,
            snapshot_policy: crate::io::SnapshotPolicy::RequireValidator,
            max_metadata_bytes: 8 * 1024 * 1024,
            redact_transport_errors: false,
            max_window_pixels: 16 * 1024 * 1024,
            max_window_bytes: 256 * 1024 * 1024,
            max_window_blocks: 4096,
            max_decoded_block_bytes: 64 * 1024 * 1024,
            max_concurrent_reads: 8,
            read_limiter: None,
            max_decode_tasks: default_blocking_permits(),
            source_id: format!("cog-{}", NEXT_SOURCE_ID.fetch_add(1, Ordering::Relaxed)),
            block_cache: Arc::new(BlockCache::new(CacheConfig::default())),
            cpu_limiter: None,
            prefetch_ring: 0,
        }
    }
}

impl ReaderConfig {
    fn decode_task_limit(&self) -> usize {
        self.max_decode_tasks.max(1)
    }
}

pub struct CogReader {
    metadata_reservations: Arc<Vec<crate::resource::WorkReservation>>,
    prefetch_tasks: std::sync::Mutex<tokio::task::JoinSet<()>>,
    work_budget: crate::resource::WorkBudget,
    redact_transport_errors: bool,
    max_window_pixels: u64,
    max_window_bytes: u64,
    max_window_blocks: usize,
    reads: Arc<tokio::sync::Semaphore>,
    pub(crate) tiff: Arc<TIFF>,
    pub(crate) reader: ObjectReader,
    pub(crate) src_crs: Crs,
    pub(crate) transform: GeoTransform,
    data_ifds: Vec<usize>,
    /// Ground-sample distance for each data level, computed once when the
    /// immutable IFD pyramid is opened.
    level_gsds: Vec<f64>,
    mask_ifds: Vec<Option<usize>>,
    /// Per data-level, the band index (0-based) whose `ExtraSamples` is
    /// Associated/UnassociatedAlpha. Mirrors rio-tiler's
    /// "alpha band > nodata > none" mask priority.
    alpha_bands: Vec<Option<usize>>,
    /// Per-band SCALE/OFFSET from GDAL metadata (identity where absent).
    band_scale_offset: Vec<ScaleOffset>,
    /// Logical source id; combined with level/kind/tile to key the shared
    /// block cache so identical tiles from the same source are reused.
    source_id: Arc<str>,
    // Instance identity also isolates opaque readers with independently scoped source IDs.
    statistics_id: u64,
    /// Shared decoded-block cache (may be shared across readers).
    block_cache: Arc<BlockCache>,
    /// Shared CPU-job limiter, covering decode + stitch here and warp + render
    /// in the [`Tiler`] built from this reader. [`Tiler`]: crate::tiler::Tiler
    cpu: CpuLimiter,
    /// Ring width (in blocks) for speculative neighbour prefetch; 0 = off.
    prefetch_ring: u8,
    /// Number of live background prefetch tasks; bounds prefetch fan-out.
    prefetch_inflight: Arc<std::sync::atomic::AtomicUsize>,
}

/// Builds a [`CogReader`]. Setters mirror [`ReaderConfig`] fields (plus HTTP
/// [`ClientOptions`]); the async terminal methods pick the source:
/// [`Self::open_http`], [`Self::open_object_store`], or [`Self::open`] for an
/// existing [`ObjectReader`].
///
/// ```no_run
/// # async fn demo(cache: std::sync::Arc<async_geotiff::io::BlockCache>) -> async_geotiff::error::AgResult<()> {
/// use async_geotiff::io::CogReader;
/// let reader = CogReader::builder()
///     .source_id("scene-42")
///     .block_cache(cache)
///     .prefetch_ring(1)
///     .open_http("https://example.com/scene.tif")
///     .await?;
/// # Ok(()) }
/// ```
pub struct CogReaderBuilder {
    config: ReaderConfig,
    client_options: Option<ClientOptions>,
}

impl CogReaderBuilder {
    /// Caller-supplied id for this logical source — same id ⇒ shared decoded
    /// blocks in the block cache (see [`ReaderConfig::source_id`]).
    pub fn source_id(mut self, id: impl Into<String>) -> Self {
        self.config.source_id = id.into();
        self
    }

    /// Shared decoded-block cache injected by the caller. Inject the same
    /// `Arc<BlockCache>` into several readers (with distinct `source_id`s) to
    /// bound them to one memory budget.
    pub fn block_cache(mut self, block_cache: Arc<BlockCache>) -> Self {
        self.config.block_cache = block_cache;
        self
    }

    /// Size a fresh private decoded-block cache — shorthand for
    /// [`Self::block_cache`] with a new [`BlockCache`] built from `cache`.
    pub fn cache_config(mut self, cache: CacheConfig) -> Self {
        self.config.block_cache = Arc::new(BlockCache::new(cache));
        self
    }

    /// Shared CPU-job limiter — see [`ReaderConfig::cpu_limiter`].
    pub fn cpu_limiter(mut self, cpu: CpuLimiter) -> Self {
        self.config.cpu_limiter = Some(cpu);
        self
    }

    /// Cap on concurrent CPU-heavy decode/stitch jobs — see
    /// [`ReaderConfig::max_decode_tasks`]. Ignored when a shared
    /// [`Self::cpu_limiter`] is injected.
    pub fn max_decode_tasks(mut self, n: usize) -> Self {
        self.config.max_decode_tasks = n;
        self
    }

    /// Speculative block prefetch radius — see [`ReaderConfig::prefetch_ring`].
    pub fn prefetch_ring(mut self, ring: u8) -> Self {
        self.config.prefetch_ring = ring;
        self
    }

    /// Replace the whole [`ReaderConfig`] at once. Setters called afterwards
    /// still apply on top of it.
    pub fn reader_config(mut self, config: ReaderConfig) -> Self {
        self.config = config;
        self
    }

    /// Choose whether source immutability is verified or explicitly assumed.
    pub fn snapshot_policy(mut self, policy: crate::io::SnapshotPolicy) -> Self {
        self.config.snapshot_policy = policy;
        self
    }

    /// Override HTTP [`ClientOptions`] (timeouts, proxy, custom headers).
    /// Only consulted by [`Self::open_http`]; the default is tuned for
    /// tile-server workloads (large per-host idle pool, 30s timeout).
    pub fn client_options(mut self, options: ClientOptions) -> Self {
        self.client_options = Some(options);
        self
    }

    /// Open a remote COG over HTTPS (or any URL `object_store::http` supports).
    pub async fn open_http(mut self, url: &str) -> AgResult<CogReader> {
        self.config.redact_transport_errors = true;
        let (base, path) = split_http_url(url)?;
        let options = self.client_options.unwrap_or_else(default_client_options);
        let store: Arc<dyn ObjectStore> = build_http_transport(move || {
            Ok(Arc::new(
                HttpBuilder::new()
                    .with_url(base)
                    .with_client_options(options)
                    .build()
                    .map_err(|_| AgError::Tiff("HTTP store construction failed".into()))?,
            ) as Arc<dyn ObjectStore>)
        })
        .await?;
        // `split_http_url` returns the percent-ENCODED URL path, so use
        // the decoding constructor. `Path::from` would treat `%` as a
        // literal and double-encode non-ASCII names (e.g. CJK filenames),
        // making the loopback GET 404 → tile 500.
        let object_path = Path::from_url_path(&path)
            .map_err(|e| AgError::Tiff(format!("invalid object path {path:?}: {e}")))?;
        CogReader::open_managed(store, object_path, self.config)
            .await
            .map_err(|e| {
                crate::io::snapshot::resource_error(&e)
                    .unwrap_or_else(|| AgError::Tiff("HTTP COG open failed".into()))
            })
    }

    /// Open a COG through any `object_store` backend (S3/MinIO, GCS, local, …).
    ///
    /// `key` must be a **decoded** object key (e.g. `"data/my file.tif"`),
    /// not a URL-encoded string — it is passed verbatim to `Path::from`.
    /// The library never holds credentials — wire them into `store` yourself.
    pub async fn open_object_store(
        self,
        store: Arc<dyn ObjectStore>,
        key: &str,
    ) -> AgResult<CogReader> {
        CogReader::open_managed(store, Path::from(key), self.config).await
    }

    /// Open an opaque reader with an explicit AssumeImmutable policy. Its
    /// underlying store cannot be inspected for conditional reads; prefer
    /// open_object_store for managed snapshots and bounded response bodies.
    pub async fn open(self, reader: ObjectReader) -> AgResult<CogReader> {
        if self.config.snapshot_policy != crate::io::SnapshotPolicy::AssumeImmutable {
            return Err(AgError::Tiff("opaque ObjectReader cannot verify versions; use open_object_store or explicitly choose AssumeImmutable".into()));
        }
        CogReader::open_impl(reader, self.config).await
    }
}

impl CogReader {
    /// Start building a reader — see [`CogReaderBuilder`].
    pub fn builder() -> CogReaderBuilder {
        CogReaderBuilder {
            config: ReaderConfig::default(),
            client_options: None,
        }
    }

    /// Open a remote COG over HTTPS (or any URL `object_store::http` supports)
    /// with default configuration — shorthand for
    /// `CogReader::builder().open_http(url)`.
    ///
    /// Uses tuned `ClientOptions` for tile-server workloads: large per-host
    /// idle pool so concurrent tile requests reuse connections instead of
    /// paying TLS handshake per fetch, and a 30s timeout.
    pub async fn open_http(url: &str) -> AgResult<Self> {
        Self::builder().open_http(url).await
    }

    /// Like [`Self::open_http`] but lets the caller override [`ClientOptions`]
    /// (e.g. timeouts, proxy, custom headers).
    #[deprecated(
        since = "0.0.9",
        note = "use `CogReader::builder().client_options(options).open_http(url)`"
    )]
    pub async fn open_http_with_options(url: &str, options: ClientOptions) -> AgResult<Self> {
        Self::builder().client_options(options).open_http(url).await
    }

    /// Like [`Self::open_http_with_options`] but also lets the caller size the
    /// decoded-block cache.
    #[deprecated(
        since = "0.0.9",
        note = "use `CogReader::builder().client_options(options).cache_config(cache).open_http(url)`"
    )]
    pub async fn open_http_with_cache(
        url: &str,
        options: ClientOptions,
        cache: CacheConfig,
    ) -> AgResult<Self> {
        Self::builder()
            .client_options(options)
            .cache_config(cache)
            .open_http(url)
            .await
    }

    /// Like [`Self::open_http`] but lets the caller inject a full [`ReaderConfig`].
    #[deprecated(
        since = "0.0.9",
        note = "use `CogReader::builder().reader_config(config).open_http(url)`"
    )]
    pub async fn open_http_with_config_default(url: &str, config: ReaderConfig) -> AgResult<Self> {
        Self::builder().reader_config(config).open_http(url).await
    }

    /// Like [`Self::open_http_with_cache`] but lets callers configure both cache
    /// capacity and CPU decode concurrency.
    #[deprecated(
        since = "0.0.9",
        note = "use `CogReader::builder().reader_config(config).client_options(options).open_http(url)`"
    )]
    pub async fn open_http_with_config(
        url: &str,
        options: ClientOptions,
        config: ReaderConfig,
    ) -> AgResult<Self> {
        Self::builder()
            .reader_config(config)
            .client_options(options)
            .open_http(url)
            .await
    }

    /// Open from an existing `ObjectReader` with default configuration —
    /// shorthand for `CogReader::builder().open(reader)`.
    pub async fn open_with_reader(reader: ObjectReader) -> AgResult<Self> {
        Self::builder().open(reader).await
    }

    /// Open from an existing `ObjectReader` with an explicit cache config.
    #[deprecated(
        since = "0.0.9",
        note = "use `CogReader::builder().cache_config(cache).open(reader)`"
    )]
    pub async fn open_with_reader_and_cache(
        reader: ObjectReader,
        cache: CacheConfig,
    ) -> AgResult<Self> {
        Self::builder().cache_config(cache).open(reader).await
    }

    /// Open from an existing `ObjectReader` with explicit cache and decode
    /// concurrency settings.
    #[deprecated(
        since = "0.0.9",
        note = "use `CogReader::builder().reader_config(config).open(reader)`"
    )]
    pub async fn open_with_reader_and_config(
        reader: ObjectReader,
        config: ReaderConfig,
    ) -> AgResult<Self> {
        Self::builder().reader_config(config).open(reader).await
    }

    async fn open_managed(
        store: Arc<dyn ObjectStore>,
        path: Path,
        mut config: ReaderConfig,
    ) -> AgResult<Self> {
        let snapshot = crate::io::snapshot::SnapshotStore::open(
            store,
            path.clone(),
            config.snapshot_policy,
            config.max_window_bytes,
            config.max_metadata_bytes,
            config.work_budget.clone(),
        )
        .await
        .map_err(crate::io::snapshot::into_ag_error)?;
        config.source_id = snapshot.cache_identity(&config.source_id);
        config.snapshot_size = Some(snapshot.size());
        let result = Self::open_impl(ObjectReader::new(snapshot.clone(), path), config).await;
        snapshot.finish_metadata();
        result
    }

    /// Shared open path: every builder terminal method and deprecated
    /// constructor funnels through here.
    async fn open_impl(reader: ObjectReader, config: ReaderConfig) -> AgResult<Self> {
        let md_cache = ReadaheadMetadataCache::new(reader.clone());
        let mut md = TiffMetadataReader::try_open(&md_cache)
            .await
            .map_err(|e| transport_error(&e, config.redact_transport_errors))?;
        let mut ifds = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut remaining_metadata = config.max_metadata_bytes;
        let mut metadata_reservations = Vec::new();
        while let Some(offset) = md.next_ifd_offset() {
            if ifds.len() >= config.max_ifds || !seen.insert(offset) {
                return Err(AgError::Tiff(
                    "cyclic or excessive TIFF directory chain".into(),
                ));
            }
            let allocation = crate::io::snapshot::preflight_ifd(
                &md_cache,
                offset,
                md.bigtiff(),
                md.endianness(),
                &config,
                remaining_metadata,
            )
            .await?;
            remaining_metadata -= allocation;
            metadata_reservations.push(config.work_budget.try_reserve(allocation)?);
            use futures_util::FutureExt;
            let ifd = std::panic::AssertUnwindSafe(md.read_next_ifd(&md_cache))
                .catch_unwind()
                .await
                .map_err(|_| AgError::Tiff("malformed TIFF directory".into()))?
                .map_err(|e| transport_error(&e, config.redact_transport_errors))?
                .ok_or_else(|| AgError::Tiff("missing TIFF directory".into()))?;
            ifds.push(ifd);
        }
        let tiff = TIFF::new(ifds, md.endianness());
        for ifd in tiff.ifds() {
            let tw = u64::from(ifd.tile_width().unwrap_or(ifd.image_width()));
            let th = u64::from(ifd.tile_height().unwrap_or(ifd.image_height()));
            let bytes_per_pixel: u64 = u64::from(ifd.samples_per_pixel())
                * u64::from(ifd.bits_per_sample().iter().copied().max().unwrap_or(8)).div_ceil(8);
            let decoded = tw
                .checked_mul(th)
                .and_then(|n| n.checked_mul(bytes_per_pixel));
            if decoded.is_none_or(|n| n > config.max_decoded_block_bytes) {
                return Err(AgError::Tiff(
                    "declared decoded block exceeds byte limit".into(),
                ));
            }
        }

        let (transform, src_crs) = derive_geo(&tiff)?;
        let (data_ifds, mask_ifds) = classify_ifds(&tiff)?;
        let alpha_bands: Vec<Option<usize>> = data_ifds
            .iter()
            .map(|&idx| tiff.ifds().get(idx).and_then(alpha_band_index))
            .collect();
        let band_scale_offset = {
            let ifd = data_ifds.first().and_then(|&idx| tiff.ifds().get(idx));
            let bands = ifd.map(|i| i.samples_per_pixel() as usize).unwrap_or(0);
            let md = ifd.and_then(|i| i.gdal_metadata());
            crate::io::scale_offset::parse_scale_offset(md, bands)
        };
        let mut cog = Self {
            metadata_reservations: Arc::new(metadata_reservations),
            prefetch_tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
            work_budget: config.work_budget.clone(),
            redact_transport_errors: config.redact_transport_errors,
            max_window_pixels: config.max_window_pixels,
            max_window_bytes: config.max_window_bytes,
            max_window_blocks: config.max_window_blocks,
            reads: config.read_limiter.clone().unwrap_or_else(|| {
                Arc::new(tokio::sync::Semaphore::new(
                    config
                        .max_concurrent_reads
                        .clamp(1, tokio::sync::Semaphore::MAX_PERMITS),
                ))
            }),
            tiff: Arc::new(tiff),
            reader,
            src_crs,
            transform,
            data_ifds,
            level_gsds: Vec::new(),
            mask_ifds,
            alpha_bands,
            band_scale_offset,
            source_id: Arc::from(config.source_id.as_str()),
            statistics_id: NEXT_SOURCE_ID.fetch_add(1, Ordering::Relaxed),
            block_cache: config.block_cache.clone(),
            cpu: config
                .cpu_limiter
                .clone()
                .unwrap_or_else(|| CpuLimiter::new(config.decode_task_limit())),
            prefetch_ring: config.prefetch_ring,
            prefetch_inflight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        cog.level_gsds = (0..cog.level_count())
            .map(|level| cog.compute_level_gsd(level).unwrap_or(f64::INFINITY))
            .collect();
        Ok(cog)
    }

    /// Open a COG from an existing object store and key, building the
    /// `ObjectReader` internally, with default configuration — shorthand for
    /// `CogReader::builder().open_object_store(store, key)`.
    ///
    /// `key` must be a **decoded** object key (e.g. `"data/my file.tif"`),
    /// not a URL-encoded string — it is passed verbatim to `Path::from`.
    pub async fn open_object_store(store: Arc<dyn ObjectStore>, key: &str) -> AgResult<Self> {
        Self::builder().open_object_store(store, key).await
    }

    /// Like [`Self::open_object_store`] but lets the caller size the decoded-block
    /// cache.
    #[deprecated(
        since = "0.0.9",
        note = "use `CogReader::builder().cache_config(cache).open_object_store(store, key)`"
    )]
    pub async fn open_object_store_with_cache(
        store: Arc<dyn ObjectStore>,
        key: &str,
        cache: CacheConfig,
    ) -> AgResult<Self> {
        Self::builder()
            .cache_config(cache)
            .open_object_store(store, key)
            .await
    }

    /// Like [`Self::open_object_store`] but lets callers configure cache capacity
    /// and CPU decode concurrency.
    #[deprecated(
        since = "0.0.9",
        note = "use `CogReader::builder().reader_config(config).open_object_store(store, key)`"
    )]
    pub async fn open_object_store_with_config(
        store: Arc<dyn ObjectStore>,
        key: &str,
        config: ReaderConfig,
    ) -> AgResult<Self> {
        Self::builder()
            .reader_config(config)
            .open_object_store(store, key)
            .await
    }

    /// The shared CPU-job limiter backing this reader. A [`Tiler`] built from
    /// this reader reuses it so decode/stitch/warp/render all draw from one CPU
    /// budget. [`Tiler`]: crate::tiler::Tiler
    pub fn cpu_limiter(&self) -> CpuLimiter {
        self.cpu.clone()
    }

    pub fn work_budget(&self) -> crate::resource::WorkBudget {
        self.work_budget.clone()
    }

    pub fn src_crs(&self) -> &Crs {
        &self.src_crs
    }

    pub(crate) fn statistics_identity(&self) -> u64 {
        self.statistics_id
    }

    pub fn transform(&self) -> GeoTransform {
        self.transform
    }

    /// Full-resolution image extent in WGS84 lon/lat as
    /// `[west, south, east, north]`. Densifies the raster boundary from the
    /// source CRS (skipped when already 4326). Used by viewers to fly to
    /// the dataset.
    pub fn wgs84_bounds(&self) -> AgResult<[f64; 4]> {
        let (w, h) = self.level_dimensions(0)?;
        let tr = self.transform;
        let corners = [
            tr.forward(0.0, 0.0),
            tr.forward(w as f64, 0.0),
            tr.forward(w as f64, h as f64),
            tr.forward(0.0, h as f64),
        ];
        let bounds =
            crate::warp::projection::reproject_boundary(corners, &self.src_crs, &Crs::epsg(4326))?;
        Ok([bounds.xmin, bounds.ymin, bounds.xmax, bounds.ymax])
    }

    pub fn overview_count(&self) -> usize {
        self.data_ifds.len().saturating_sub(1)
    }

    pub fn level_count(&self) -> usize {
        self.data_ifds.len()
    }

    pub fn level_dimensions(&self, level: usize) -> AgResult<(u32, u32)> {
        let ifd = self.data_ifd(level)?;
        Ok((ifd.image_width(), ifd.image_height()))
    }

    /// Internal tile dimensions of the chosen overview, or the full image
    /// dims if the IFD is strip-based. Used by the `is_aligned` padding
    /// optimisation (rio-tiler `_requested_tile_aligned_with_internal_tile`).
    pub fn internal_tile_dims(&self, level: usize) -> AgResult<(u32, u32)> {
        let ifd = self.data_ifd(level)?;
        Ok((
            ifd.tile_width().unwrap_or_else(|| ifd.image_width()),
            ifd.tile_height().unwrap_or_else(|| ifd.image_height()),
        ))
    }

    pub fn level_transform(&self, level: usize) -> AgResult<GeoTransform> {
        let (full_w, full_h) = self.level_dimensions(0)?;
        let (level_w, level_h) = self.level_dimensions(level)?;
        let scale_x = full_w as f64 / level_w as f64;
        let scale_y = full_h as f64 / level_h as f64;
        Ok(self.transform.scaled(scale_x, scale_y))
    }

    fn compute_level_gsd(&self, level: usize) -> AgResult<f64> {
        let tr = self.level_transform(level)?.0;
        let x_res = (tr.a.powi(2) + tr.d.powi(2)).sqrt();
        let y_res = (tr.b.powi(2) + tr.e.powi(2)).sqrt();
        Ok(x_res.max(y_res))
    }

    pub fn level_gsd(&self, level: usize) -> AgResult<f64> {
        match self
            .level_gsds
            .get(level)
            .copied()
            .filter(|v| v.is_finite())
        {
            Some(gsd) => Ok(gsd),
            // Preserve the previous error/non-finite behavior for malformed or
            // out-of-range levels; normal levels stay on the cached fast path.
            None => self.compute_level_gsd(level),
        }
    }

    /// Select an overview without a tolerance tied to CRS units.
    ///
    /// Level 0 is full resolution; 1.. are progressively coarser overviews
    /// (gsd strictly increasing). The chosen level is the **coarsest one that
    /// is still no coarser than the target** — i.e. it brackets the target
    /// from below and the subsequent warp *downsamples* to the exact tile
    /// resolution. Reading a finer level and downsampling preserves detail;
    /// reading a coarser level and upsampling would blur the tile. This is
    /// the single most important quality/bandwidth decision in the pipeline.
    ///
    /// Rules:
    /// - `target ≤ src_res` (zoomed in past native): use full resolution.
    /// - otherwise pick `L` where `gsd(L) ≤ target < gsd(L+1)`.
    /// - target coarser than every overview: use the coarsest level.
    pub fn pick_overview(&self, target_gsd: f64) -> usize {
        pick_overview_from_gsds(&self.level_gsds, target_gsd)
    }

    /// Read a pixel window. Each native tile in the window is resolved through
    /// the shared block cache (single-flight decode on a miss), and the decoded
    /// blocks are stitched into the requested window. Blocks are cached and may
    /// be shared across readers with the same `source_id`.
    #[tracing::instrument(skip(self), fields(level, width, height), err)]
    pub async fn read_window(
        &self,
        level: usize,
        col_off: i64,
        row_off: i64,
        width: u64,
        height: u64,
    ) -> AgResult<Arc<ImageData>> {
        #[cfg(feature = "perf-tracing")]
        let started = Instant::now();
        validate_window_shape(width, height)?;
        let _read_permit = self
            .reads
            .acquire()
            .await
            .map_err(|_| AgError::Tiff("read limiter closed".into()))?;
        let pixels = width
            .checked_mul(height)
            .ok_or_else(|| AgError::Tiff("window pixel count overflow".into()))?;
        let ifd = self.data_ifd(level)?;
        let bytes_per_pixel: u64 = u64::from(ifd.samples_per_pixel())
            * u64::from(ifd.bits_per_sample().iter().copied().max().unwrap_or(8)).div_ceil(8);
        let bytes = pixels.checked_mul(bytes_per_pixel.saturating_add(1));
        if pixels > self.max_window_pixels || bytes.is_none_or(|n| n > self.max_window_bytes) {
            return Err(AgError::Tiff(
                "source window exceeds pixel/byte limit".into(),
            ));
        }
        checked_window_end(col_off, width, "column")?;
        checked_window_end(row_off, height, "row")?;
        // Bound the window to the level's pixel extent so a caller-supplied
        // width/height cannot drive an allocation larger than the image itself
        // (the output buffers below are sized `bands × height × width`). Every
        // in-crate caller already clamps to the extent; this rejects the
        // out-of-bounds windows an external caller could otherwise pass.
        let (lvl_w, lvl_h) = self.level_dimensions(level)?;
        if col_off < 0 || row_off < 0 {
            return Err(AgError::Tiff(format!(
                "window origin ({col_off},{row_off}) is negative"
            )));
        }
        if col_off as u64 + width > u64::from(lvl_w) || row_off as u64 + height > u64::from(lvl_h) {
            return Err(AgError::Tiff(format!(
                "window {width}x{height}+{col_off}+{row_off} exceeds level {level} extent {lvl_w}x{lvl_h}"
            )));
        }

        let ifd = self.data_ifd(level)?;
        let ((tx0, tx1), (ty0, ty1)) = tile_range_for_window(ifd, col_off, row_off, width, height)
            .ok_or_else(|| AgError::Tiff("ifd lacks tile dims".into()))?;
        let tile_cols = (tx1 - tx0 + 1) as usize;
        let tile_rows = (ty1 - ty0 + 1) as usize;
        let block_count = tile_cols
            .checked_mul(tile_rows)
            .ok_or_else(|| AgError::Tiff("window block count overflow".into()))?;
        if block_count > self.max_window_blocks {
            return Err(AgError::Tiff("source window exceeds block limit".into()));
        }
        let mut coords = Vec::with_capacity(block_count);
        for x in tx0..=tx1 {
            for y in ty0..=ty1 {
                coords.push((x as usize, y as usize));
            }
        }
        let data_info = IfdInfo::from(ifd).validate()?;

        let mask_ifd = self.mask_ifd(level)?;
        let mut work_bytes = block_work_bytes(ifd, &coords, self.max_window_bytes)?;
        if let Some(mask_ifd) = mask_ifd {
            work_bytes = work_bytes
                .checked_add(block_work_bytes(mask_ifd, &coords, self.max_window_bytes)?)
                .ok_or_else(|| AgError::ResourceLimit("read work estimate overflow".into()))?;
        }
        let output_bytes = bytes.unwrap(); // checked against window limits above
        work_bytes = work_bytes
            .checked_add(output_bytes.saturating_mul(2))
            .ok_or_else(|| AgError::ResourceLimit("read work estimate overflow".into()))?;
        let work = self.work_budget.try_reserve(work_bytes)?;
        #[cfg(feature = "perf-tracing")]
        let plan_done = started.elapsed();
        let (data_blocks, mask_blocks) = if let Some(mask_ifd) = mask_ifd {
            let mask_info = IfdInfo::from(mask_ifd).validate()?;
            let data_blocks = self.blocks_for(level, BlockKind::Data, ifd, &coords, &work);
            let mask_blocks = self.blocks_for(level, BlockKind::Mask, mask_ifd, &coords, &work);
            let (data_blocks, mask_blocks) = tokio::try_join!(data_blocks, mask_blocks)?;
            (data_blocks, Some((mask_blocks, mask_info)))
        } else {
            (
                self.blocks_for(level, BlockKind::Data, ifd, &coords, &work)
                    .await?,
                None,
            )
        };
        // Foreground blocks are resolved — speculatively warm the surrounding
        // block ring in the background. It overlaps the stitch/warp/render
        // below and turns the neighbouring XYZ tiles' cold misses into hits.
        self.maybe_spawn_prefetch(level, (tx0, tx1), (ty0, ty1));
        #[cfg(feature = "perf-tracing")]
        let blocks_done = started.elapsed();
        #[cfg(feature = "perf-tracing")]
        let data_stitch_started = Instant::now();
        #[cfg(feature = "perf-tracing")]
        let mut data_stitch_cpu_wait_ms = Duration::ZERO;
        // Fused stitch: ONE pass builds both the data and the mask windows.
        // Previously this was two sequential spawn_blocking jobs, each paying
        // its own dispatch latency and CPU-permit wait. Additionally, tile-size
        // windows are memcpy-bound and cost less than the spawn_blocking
        // dispatch itself, so small outputs stitch inline on the async thread.
        // Estimated stitched output footprint: planar data + 1-byte mask cells.
        let alpha_idx = self.alpha_bands.get(level).copied().flatten();
        let elem_bytes = data_blocks
            .first()
            .and_then(|b| b.array().data_type())
            .map(|dt| dt.size() as u64)
            .unwrap_or(8);
        let est_out_bytes = (data_info.bands as u64)
            .saturating_mul(width)
            .saturating_mul(height)
            .saturating_mul(elem_bytes)
            .saturating_add(if mask_blocks.is_some() {
                width.saturating_mul(height)
            } else {
                0
            });
        // Alpha promotion builds another planar buffer and a mask. Include
        // that pass in the inline ceiling rather than splitting it afterwards
        // on the async worker, outside the CPU permit.
        let est_out_bytes = if mask_blocks.is_none() && alpha_idx.is_some() {
            est_out_bytes.saturating_mul(2)
        } else {
            est_out_bytes
        };
        let stitch_all = {
            let work = work.clone();
            let request = crate::request::RequestContext::current();
            let coords2 = coords.clone();
            let info = data_info;
            let (w, h) = (width, height);
            let (co, ro) = (col_off, row_off);
            move || -> AgResult<(PixelData, Option<Array2<bool>>)> {
                let _work = work;
                crate::request::RequestContext::run_blocking(request, || {
                    let refs: Vec<&async_tiff::Array> =
                        data_blocks.iter().map(|b| b.array()).collect();
                    let stitched =
                        crate::io::window::stitch_with_info(&refs, &coords2, &info, co, ro, w, h)?;
                    let mask = match &mask_blocks {
                        Some((mask_blocks, mask_info)) => {
                            let refs: Vec<&async_tiff::Array> =
                                mask_blocks.iter().map(|b| b.array()).collect();
                            Some(crate::io::window::stitch_mask_with_info(
                                &refs, &coords2, mask_info, co, ro, w, h,
                            )?)
                        }
                        None => None,
                    };
                    match (mask, alpha_idx) {
                        (None, Some(b)) if b < stitched.bands() => stitched
                            .split_off_band(b)
                            .map(|(pixels, alpha)| (pixels, Some(alpha)))
                            .ok_or_else(|| AgError::Tiff("alpha-band split failed".into())),
                        (mask, _) => Ok((stitched, mask)),
                    }
                })
            }
        };
        let (stitched, mask) = if est_out_bytes <= INLINE_STITCH_MAX_BYTES {
            // Small window (typical 256/512 px tile): the copy is cheaper than
            // a spawn_blocking round-trip + permit wait — run it inline.
            stitch_all()?
        } else {
            #[cfg(feature = "perf-tracing")]
            let wait_started = Instant::now();
            let permit = self.cpu.acquire().await?;
            crate::perf_trace! {
                data_stitch_cpu_wait_ms = wait_started.elapsed();
            }
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                stitch_all()
            })
            .await
            .map_err(|e| AgError::Tiff(format!("join: {e}")))??
        };
        #[cfg(feature = "perf-tracing")]
        let data_stitch_ms = data_stitch_started.elapsed();
        #[cfg(feature = "perf-tracing")]
        let data_stitch_job_ms = data_stitch_ms.saturating_sub(data_stitch_cpu_wait_ms);
        #[cfg(feature = "perf-tracing")]
        let mask_stitch_ms = Duration::ZERO;
        #[cfg(feature = "perf-tracing")]
        let mask_stitch_cpu_wait_ms = Duration::ZERO;
        #[cfg(feature = "perf-tracing")]
        let mask_stitch_job_ms = Duration::ZERO;

        #[cfg(feature = "perf-tracing")]
        let assemble_started = Instant::now();
        let image = self.assemble_image(level, stitched, mask, col_off, row_off, width, height)?;
        #[cfg(feature = "perf-tracing")]
        let assemble_ms = assemble_started.elapsed();
        crate::perf_trace! {
            tracing::debug!(
                level,
                tiles = coords.len(),
                plan_ms = duration_ms(plan_done),
                blocks_ms = duration_ms(blocks_done - plan_done),
                data_stitch_ms = duration_ms(data_stitch_ms),
                data_stitch_cpu_wait_ms = duration_ms(data_stitch_cpu_wait_ms),
                data_stitch_job_ms = duration_ms(data_stitch_job_ms),
                mask_stitch_ms = duration_ms(mask_stitch_ms),
                mask_stitch_cpu_wait_ms = duration_ms(mask_stitch_cpu_wait_ms),
                mask_stitch_job_ms = duration_ms(mask_stitch_job_ms),
                assemble_ms = duration_ms(assemble_ms),
                total_ms = duration_ms(started.elapsed()),
                "read_window timing"
            );
        }
        Ok(Arc::new(image))
    }
}

impl CogReader {
    /// Attach georeferencing and nodata to a window already stitched and
    /// alpha-promoted by the same CPU job.
    #[allow(clippy::too_many_arguments)]
    fn assemble_image(
        &self,
        level: usize,
        pixels: PixelData,
        mask: Option<Array2<bool>>,
        col_off: i64,
        row_off: i64,
        width: u64,
        height: u64,
    ) -> AgResult<ImageData> {
        let level_transform = self.level_transform(level)?;
        let window_transform = level_transform.translated(col_off as f64, row_off as f64);
        let bbox_world = transform_bbox(level_transform, col_off, row_off, width, height)?;
        let nodata = self
            .data_ifd(level)?
            .gdal_nodata()
            .and_then(|s| s.parse::<f64>().ok());

        Ok(ImageData {
            pixels,
            mask,
            bbox: bbox_world,
            crs: self.src_crs.clone(),
            transform: window_transform,
            nodata,
        })
    }

    /// Per-band [`ScaleOffset`] from GDAL metadata; [`ScaleOffset::IDENTITY`]
    /// where absent. Apply as `physical = raw * scale + offset` when unscaling.
    pub fn band_scale_offset(&self) -> &[ScaleOffset] {
        &self.band_scale_offset
    }

    pub(crate) fn data_ifd(&self, level: usize) -> AgResult<&ImageFileDirectory> {
        let idx = self.data_ifd_index(level)?;
        self.tiff
            .ifds()
            .get(idx)
            .ok_or(AgError::MissingGeo("overview IFD"))
    }

    fn data_ifd_index(&self, level: usize) -> AgResult<usize> {
        self.data_ifds
            .get(level)
            .copied()
            .ok_or(AgError::MissingGeo("overview level"))
    }

    fn mask_ifd(&self, level: usize) -> AgResult<Option<&ImageFileDirectory>> {
        let Some(mask_idx) = self
            .mask_ifds
            .get(level)
            .ok_or(AgError::MissingGeo("overview level"))?
        else {
            return Ok(None);
        };
        Ok(self.tiff.ifds().get(*mask_idx))
    }
}

/// World-space AABB of a pixel rectangle under any (possibly rotated/sheared)
/// GeoTransform. Previously only sampled two opposite corners, which produced
/// the wrong bbox for rotated COGs (where the world-space pixel grid is not
/// axis-aligned).
fn checked_window_end(offset: i64, len: u64, axis: &'static str) -> AgResult<i64> {
    let len = i64::try_from(len)
        .map_err(|_| AgError::Tiff(format!("{axis} window length exceeds i64")))?;
    offset
        .checked_add(len)
        .ok_or_else(|| AgError::Tiff(format!("{axis} window end overflows i64")))
}

fn transform_bbox(transform: GeoTransform, col: i64, row: i64, w: u64, h: u64) -> AgResult<Bbox> {
    let c0 = col as f64;
    let r0 = row as f64;
    let c1 = checked_window_end(col, w, "column")? as f64;
    let r1 = checked_window_end(row, h, "row")? as f64;
    let corners = [
        transform.forward(c0, r0),
        transform.forward(c1, r0),
        transform.forward(c1, r1),
        transform.forward(c0, r1),
    ];
    Ok(Bbox::from_corners(&corners))
}

/// Defaults tuned for COG tile servers: keep many idle keep-alive sockets per
/// origin so back-to-back tile requests skip the TLS handshake. Mirrors the
/// `obstore` connection-pool tuning recommended for async-geotiff.
///
/// `allow_http(true)`: object_store refuses plain `http://` URLs by default
/// (it fails at request-build time with a "builder error", before any
/// network). A COG tile server commonly fronts an internal object store
/// (MinIO, on-prem S3) reached over `http://host:port`, so plain HTTP must
/// be permitted. HTTPS endpoints are unaffected.
fn default_client_options() -> ClientOptions {
    // `with_allow_http2`: object_store defaults to HTTP/1.1-only
    // (`http1_only: true`), so even HTTPS object stores get no
    // multiplexing — every concurrent tile range request needs its own
    // TCP/TLS handshake, which dominates cold-start latency when a viewer
    // fans out a full screen of tiles. Allowing HTTP/2 lets HTTPS
    // endpoints negotiate h2 via ALPN (one connection, many streams);
    // plaintext endpoints transparently stay on HTTP/1.1.
    ClientOptions::new()
        .with_allow_http(true)
        .with_allow_http2()
        .with_pool_max_idle_per_host(64)
        .with_pool_idle_timeout(Duration::from_secs(90))
        .with_timeout(Duration::from_secs(30))
        .with_connect_timeout(Duration::from_secs(10))
}

fn split_http_url(url: &str) -> AgResult<(String, String)> {
    let u = url::Url::parse(url).map_err(|e| AgError::Tiff(format!("invalid url: {e}")))?;
    if u.query().is_some() {
        // Keep the complete signed URL as the store base. Appending an empty
        // object path preserves the encoded path and signature query verbatim.
        return Ok((u.to_string(), String::new()));
    }
    let scheme = u.scheme();
    let host = u
        .host_str()
        .ok_or_else(|| AgError::Tiff("url missing host".into()))?;
    let port = u.port().map(|p| format!(":{}", p)).unwrap_or_default();
    let base = format!("{}://{}{}", scheme, host, port);
    let path = u.path().trim_start_matches('/').to_string();
    Ok((base, path))
}

fn derive_geo(tiff: &TIFF) -> AgResult<(GeoTransform, Crs)> {
    let ifd = tiff.ifds().first().ok_or(AgError::MissingGeo("no IFD"))?;
    derive_ifd_geo(ifd)
}

fn derive_ifd_geo(ifd: &ImageFileDirectory) -> AgResult<(GeoTransform, Crs)> {
    let mut gt = if let Some(t) = ifd.model_transformation() {
        // 4x4 row-major; for a north-up image use indices [3, 0, 1, 7, 4, 5].
        if t.len() < 16 {
            return Err(AgError::MissingGeo("model_transformation truncated"));
        }
        GeoTransform::from_gdal(&[t[3], t[0], t[1], t[7], t[4], t[5]])
    } else {
        let tp = ifd
            .model_tiepoint()
            .ok_or(AgError::MissingGeo("model_tiepoint"))?;
        let ps = ifd
            .model_pixel_scale()
            .ok_or(AgError::MissingGeo("model_pixel_scale"))?;
        if tp.len() < 6 || ps.len() < 2 {
            return Err(AgError::MissingGeo("tiepoint/scale truncated"));
        }
        let (i, j, x, y) = (tp[0], tp[1], tp[3], tp[4]);
        let (sx, sy) = (ps[0], ps[1]);
        // World origin = (x - i*sx, y + j*sy) for north-up images.
        GeoTransform::from_gdal(&[x - i * sx, sx, 0.0, y + j * sy, 0.0, -sy])
    };

    if ifd.geo_key_directory().and_then(|keys| keys.raster_type) == Some(2) {
        // GeoTIFF point coordinates locate sample centres; ImageData uses corners.
        gt = gt.translated(-0.5, -0.5);
    }

    let crs = ifd
        .geo_key_directory()
        .and_then(|gk| gk.epsg_code())
        .map(|c| Crs::epsg(c as u32))
        .unwrap_or_else(|| Crs::new("UNKNOWN"));

    Ok((gt, crs))
}

fn classify_ifds(tiff: &TIFF) -> AgResult<(Vec<usize>, Vec<Option<usize>>)> {
    let first = tiff.ifds().first().ok_or(AgError::MissingGeo("no IFD"))?;
    let mut mask_by_dims: HashMap<(u32, u32), usize> = HashMap::new();
    let mut overviews: Vec<((u32, u32), usize)> = Vec::new();
    for (idx, ifd) in tiff.ifds().iter().enumerate().skip(1) {
        let dims = (ifd.image_width(), ifd.image_height());
        if is_mask_ifd(ifd) {
            mask_by_dims.insert(dims, idx);
        } else if ifd.new_subfile_type().is_some_and(|t| t & 1 != 0)
            && dims.0 > 0 && dims.1 > 0
            && dims.0 < first.image_width() && dims.1 < first.image_height()
            && ifd.samples_per_pixel() == first.samples_per_pixel()
            && ifd.bits_per_sample() == first.bits_per_sample()
            && ifd.sample_format() == first.sample_format()
            && ifd.photometric_interpretation() == first.photometric_interpretation()
            // Allow one overview pixel of rounding when the base dimensions
            // are not divisible by the reduction factor.
            && (f64::from(first.image_width()) / f64::from(dims.0)
                - f64::from(first.image_height()) / f64::from(dims.1)).abs()
                <= f64::from(first.image_width()).max(f64::from(first.image_height()))
                    / f64::from(dims.0) / f64::from(dims.1)
        {
            // Tagged overviews must describe the same grid/footprint. Untagged
            // reduced IFDs inherit the base transform, as ordinary COGs do.
            if ifd.model_transformation().is_some() || ifd.model_tiepoint().is_some() {
                let base = derive_geo(tiff)?.0;
                let candidate = derive_ifd_geo(ifd)?.0;
                let sx = f64::from(first.image_width()) / f64::from(dims.0);
                let sy = f64::from(first.image_height()) / f64::from(dims.1);
                let aligned = [(0., 0.), (1., 0.), (0., 1.)].iter().all(|&(x, y)| {
                    let a = base.forward(x * sx, y * sy);
                    let b = candidate.forward(x, y);
                    (a.0 - b.0).abs() <= 1e-8 * a.0.abs().max(1.)
                        && (a.1 - b.1).abs() <= 1e-8 * a.1.abs().max(1.)
                });
                if !aligned {
                    continue;
                }
            }
            if let Some(epsg) = ifd.geo_key_directory().and_then(|g| g.epsg_code()) {
                if first.geo_key_directory().and_then(|g| g.epsg_code()) != Some(epsg) {
                    continue;
                }
            }
            overviews.push((dims, idx));
        }
    }

    overviews.sort_by_key(|((w, h), _)| std::cmp::Reverse(u64::from(*w) * u64::from(*h)));

    let mut data_ifds = vec![0usize];
    let mut mask_ifds = vec![mask_by_dims
        .get(&(first.image_width(), first.image_height()))
        .copied()];
    for (dims, idx) in overviews {
        data_ifds.push(idx);
        mask_ifds.push(mask_by_dims.get(&dims).copied());
    }
    Ok((data_ifds, mask_ifds))
}

/// Pure overview-selection logic, independent of coordinate units.
/// `gsds[0]` is full-resolution; entries are non-decreasing (coarser).
fn pick_overview_from_gsds(gsds: &[f64], target_gsd: f64) -> usize {
    let n = gsds.len();
    if n <= 1 {
        return 0;
    }
    let src_res = gsds[0];
    if target_gsd <= src_res {
        return 0;
    }
    for level in 0..n - 1 {
        let cur = gsds[level];
        let next = gsds[level + 1];
        if cur < target_gsd && next > target_gsd {
            return level;
        }
        if cur == target_gsd {
            return level;
        }
    }
    n - 1
}

fn is_mask_ifd(ifd: &ImageFileDirectory) -> bool {
    ifd.new_subfile_type().is_some_and(|t| t & 4 != 0)
        && ifd.photometric_interpretation() == PhotometricInterpretation::TransparencyMask
}

/// 0-based index of the band tagged as Associated/UnassociatedAlpha via
/// `ExtraSamples`. The alpha band is positioned after the
/// PhotometricInterpretation-defined bands (3 for RGB, 1 for greyscale),
/// so the alpha index is `(bands - extra_samples.len()) + offset_within_extra`.
/// Returns None when there are no extra samples or none is tagged alpha.
fn alpha_band_index(ifd: &ImageFileDirectory) -> Option<usize> {
    use async_tiff::tags::ExtraSamples;
    let extras = ifd.extra_samples()?;
    let total_bands = ifd.samples_per_pixel() as usize;
    let base = total_bands.checked_sub(extras.len())?;
    extras.iter().enumerate().find_map(|(i, e)| {
        matches!(
            e,
            ExtraSamples::AssociatedAlpha | ExtraSamples::UnassociatedAlpha
        )
        .then_some(base + i)
    })
}

#[cfg(test)]
#[path = "cog_tests.rs"]
mod tests;
