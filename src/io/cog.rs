//! `CogReader` — async-tiff reader orchestration with GeoTransform/CRS derivation.

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
    pub(crate) tiff: TIFF,
    pub(crate) reader: ObjectReader,
    pub(crate) src_crs: Crs,
    pub(crate) transform: GeoTransform,
    data_ifds: Vec<usize>,
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

    /// Override HTTP [`ClientOptions`] (timeouts, proxy, custom headers).
    /// Only consulted by [`Self::open_http`]; the default is tuned for
    /// tile-server workloads (large per-host idle pool, 30s timeout).
    pub fn client_options(mut self, options: ClientOptions) -> Self {
        self.client_options = Some(options);
        self
    }

    /// Open a remote COG over HTTPS (or any URL `object_store::http` supports).
    pub async fn open_http(self, url: &str) -> AgResult<CogReader> {
        let (base, path) = split_http_url(url)?;
        let options = self.client_options.unwrap_or_else(default_client_options);
        let store: Arc<dyn ObjectStore> = Arc::new(
            HttpBuilder::new()
                .with_url(base)
                .with_client_options(options)
                .build()
                .map_err(|e| AgError::Tiff(format!("http store: {e}")))?,
        );
        // `split_http_url` returns the percent-ENCODED URL path, so use
        // the decoding constructor. `Path::from` would treat `%` as a
        // literal and double-encode non-ASCII names (e.g. CJK filenames),
        // making the loopback GET 404 → tile 500.
        let object_path = Path::from_url_path(&path)
            .map_err(|e| AgError::Tiff(format!("invalid object path {path:?}: {e}")))?;
        CogReader::open_impl(ObjectReader::new(store, object_path), self.config).await
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
        CogReader::open_impl(ObjectReader::new(store, Path::from(key)), self.config).await
    }

    /// Open from an existing [`ObjectReader`] (lets callers wire any
    /// object_store themselves).
    pub async fn open(self, reader: ObjectReader) -> AgResult<CogReader> {
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

    /// Shared open path: every builder terminal method and deprecated
    /// constructor funnels through here.
    async fn open_impl(reader: ObjectReader, config: ReaderConfig) -> AgResult<Self> {
        let md_cache = ReadaheadMetadataCache::new(reader.clone());
        let mut md = TiffMetadataReader::try_open(&md_cache)
            .await
            .map_err(|e| AgError::Tiff(format!("metadata open: {e}")))?;
        let ifds = md
            .read_all_ifds(&md_cache)
            .await
            .map_err(|e| AgError::Tiff(format!("read ifds: {e}")))?;
        let tiff = TIFF::new(ifds, md.endianness());

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
        Ok(Self {
            tiff,
            reader,
            src_crs,
            transform,
            data_ifds,
            mask_ifds,
            alpha_bands,
            band_scale_offset,
            source_id: Arc::from(config.source_id.as_str()),
            block_cache: config.block_cache.clone(),
            cpu: config
                .cpu_limiter
                .clone()
                .unwrap_or_else(|| CpuLimiter::new(config.decode_task_limit())),
            prefetch_ring: config.prefetch_ring,
            prefetch_inflight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
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

    pub fn src_crs(&self) -> &Crs {
        &self.src_crs
    }

    pub fn transform(&self) -> GeoTransform {
        self.transform
    }

    /// Full-resolution image extent in WGS84 lon/lat as
    /// `[west, south, east, north]`. Reprojects the four corners from the
    /// source CRS (skipped when already 4326). Used by viewers to fly to
    /// the dataset.
    pub fn wgs84_bounds(&self) -> AgResult<[f64; 4]> {
        let (w, h) = self.level_dimensions(0)?;
        let tr = self.transform;
        let mut corners = [
            tr.forward(0.0, 0.0),
            tr.forward(w as f64, 0.0),
            tr.forward(w as f64, h as f64),
            tr.forward(0.0, h as f64),
        ];
        let wgs84 = Crs::epsg(4326);
        if self.src_crs != wgs84 {
            crate::warp::Projector::new(&self.src_crs, &wgs84)?.convert(&mut corners)?;
        }
        let mut west = f64::INFINITY;
        let mut east = f64::NEG_INFINITY;
        let mut south = f64::INFINITY;
        let mut north = f64::NEG_INFINITY;
        for (lon, lat) in corners {
            west = west.min(lon);
            east = east.max(lon);
            south = south.min(lat);
            north = north.max(lat);
        }
        Ok([west, south, east, north])
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

    pub fn level_gsd(&self, level: usize) -> AgResult<f64> {
        let tr = self.level_transform(level)?.0;
        let x_res = (tr.a.powi(2) + tr.d.powi(2)).sqrt();
        let y_res = (tr.b.powi(2) + tr.e.powi(2)).sqrt();
        Ok(x_res.max(y_res))
    }

    /// Faithful port of rio-tiler `get_overview_level`
    /// (`rio_tiler/utils.py`, itself adapted from GDAL `gdalwarp_lib.cpp`).
    ///
    /// Level 0 is full resolution; 1.. are progressively coarser overviews
    /// (gsd strictly increasing). The chosen level is the **finest one that
    /// is still no coarser than the target** — i.e. it brackets the target
    /// from below and the subsequent warp *downsamples* to the exact tile
    /// resolution. Reading a finer level and downsampling preserves detail;
    /// reading a coarser level and upsampling would blur the tile. This is
    /// the single most important quality/bandwidth decision in the pipeline.
    ///
    /// Rules (matching upstream exactly):
    /// - `target ≤ src_res` (zoomed in past native): use full resolution.
    /// - otherwise walk levels; pick `L` where `gsd(L) < target < gsd(L+1)`,
    ///   or snap to `L` if `|gsd(L) − target| < 0.1`.
    /// - target coarser than every overview: use the coarsest level.
    pub fn pick_overview(&self, target_gsd: f64) -> usize {
        let n = self.level_count();
        let gsds: Vec<f64> = (0..n)
            .map(|l| self.level_gsd(l).unwrap_or(f64::INFINITY))
            .collect();
        pick_overview_from_gsds(&gsds, target_gsd)
    }

    /// Read a pixel window. Each native tile in the window is resolved through
    /// the shared block cache (single-flight decode on a miss), and the decoded
    /// blocks are stitched into the requested window. Blocks are cached and may
    /// be shared across readers with the same `source_id`.
    #[tracing::instrument(
        skip(self),
        fields(source = %self.source_id, level, width, height),
        err
    )]
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
        checked_window_end(col_off, width, "column")?;
        checked_window_end(row_off, height, "row")?;

        let ifd = self.data_ifd(level)?;
        let ((tx0, tx1), (ty0, ty1)) = tile_range_for_window(ifd, col_off, row_off, width, height)
            .ok_or_else(|| AgError::Tiff("ifd lacks tile dims".into()))?;
        let tile_cols = (tx1 - tx0 + 1) as usize;
        let tile_rows = (ty1 - ty0 + 1) as usize;
        let mut coords = Vec::with_capacity(tile_cols.saturating_mul(tile_rows));
        for x in tx0..=tx1 {
            for y in ty0..=ty1 {
                coords.push((x as usize, y as usize));
            }
        }
        let data_info = IfdInfo::from(ifd).validate()?;

        let mask_ifd = self.mask_ifd(level)?;
        #[cfg(feature = "perf-tracing")]
        let plan_done = started.elapsed();
        let (data_blocks, mask_blocks) = if let Some(mask_ifd) = mask_ifd {
            let mask_info = IfdInfo::from(mask_ifd).validate()?;
            let data_blocks = self.blocks_for(level, BlockKind::Data, ifd, &coords);
            let mask_blocks = self.blocks_for(level, BlockKind::Mask, mask_ifd, &coords);
            let (data_blocks, mask_blocks) = tokio::try_join!(data_blocks, mask_blocks)?;
            (data_blocks, Some((mask_blocks, mask_info)))
        } else {
            (
                self.blocks_for(level, BlockKind::Data, ifd, &coords)
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
        let stitch_all = {
            let coords2 = coords.clone();
            let info = data_info;
            let (w, h) = (width, height);
            let (co, ro) = (col_off, row_off);
            move || -> AgResult<(PixelData, Option<Array2<bool>>)> {
                let refs: Vec<&async_tiff::Array> = data_blocks.iter().map(|b| b.array()).collect();
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
                Ok((stitched, mask))
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

/// Block coordinates of the `ring`-wide frame around the inclusive tile range
/// `(tx0..=tx1, ty0..=ty1)`, clamped to the IFD's tile grid and excluding the
/// inner (already-read) rectangle.
fn ring_coords(
    info: &IfdInfo<ValidatedIfdInfo>,
    (tx0, tx1): (u32, u32),
    (ty0, ty1): (u32, u32),
    ring: i64,
) -> Vec<(usize, usize)> {
    if info.tile_width == 0 || info.tile_height == 0 {
        return Vec::new();
    }
    let max_tx = i64::from((info.image_width.saturating_sub(1)) / info.tile_width);
    let max_ty = i64::from((info.image_height.saturating_sub(1)) / info.tile_height);
    let (itx0, itx1) = (i64::from(tx0), i64::from(tx1));
    let (ity0, ity1) = (i64::from(ty0), i64::from(ty1));
    let x0 = (itx0 - ring).max(0);
    let x1 = (itx1 + ring).min(max_tx);
    let y0 = (ity0 - ring).max(0);
    let y1 = (ity1 + ring).min(max_ty);
    let mut out = Vec::new();
    for x in x0..=x1 {
        for y in y0..=y1 {
            let inside = (itx0..=itx1).contains(&x) && (ity0..=ity1).contains(&y);
            if !inside {
                out.push((x as usize, y as usize));
            }
        }
    }
    out
}

/// Best-effort fetch + decode of `coords` into the shared block cache.
///
/// Already-cached blocks are skipped via a non-counting [`BlockCache::peek`]
/// (so prefetch probes don't skew hit/miss stats), and blocks another caller is
/// already fetching are skipped via [`BlockCache::claim`] dedup. One coalesced
/// `fetch_tiles` covers all claimed blocks; decodes run one at a time under the
/// shared CPU limiter so speculation never floods the foreground decode budget.
/// All errors are logged at debug and swallowed — a failed prefetch must never
/// surface anywhere; dropped claim guards let foreground waiters retry fresh.
#[allow(clippy::too_many_arguments)]
async fn prefetch_blocks(
    ifd: &ImageFileDirectory,
    kind: BlockKind,
    level: usize,
    coords: &[(usize, usize)],
    source: &Arc<str>,
    cache: &Arc<BlockCache>,
    cpu: &CpuLimiter,
    reader: &ObjectReader,
) {
    let mut owned: Vec<(BlockKey, InflightGuard)> = Vec::new();
    let mut fetch_coords: Vec<(usize, usize)> = Vec::new();
    for &(tx, ty) in coords {
        let key = BlockKey {
            source: Arc::clone(source),
            level: level as u32,
            kind,
            tx: tx as u32,
            ty: ty as u32,
        };
        if cache.peek(&key) {
            continue;
        }
        match cache.claim(&key) {
            Claim::Owned(guard) => {
                owned.push((key, guard));
                fetch_coords.push((tx, ty));
            }
            // A foreground request (or another prefetch) is on it already.
            Claim::Pending(_) => {}
        }
    }
    if owned.is_empty() {
        return;
    }
    cache.record_fetched(fetch_coords.len() as u64);
    let tiles = match ifd.fetch_tiles(&fetch_coords, reader).await {
        Ok(t) => t,
        Err(e) => {
            // Guards drop unresolved → any waiters observe a cancel and retry.
            tracing::debug!(error = %e, kind = ?kind, "block prefetch fetch failed");
            return;
        }
    };
    for ((key, guard), tile) in owned.into_iter().zip(tiles.into_iter()) {
        let cpu = cpu.clone();
        let res: AgResult<Arc<DecodedBlock>> = cache
            .try_get_with(key, async move {
                let permit = cpu.acquire().await?;
                let arr = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    crate::io::decode::decode_one(tile)
                })
                .await
                .map_err(|e| AgError::Tiff(format!("join: {e}")))??;
                Ok(DecodedBlock::new(arr))
            })
            .await;
        match res {
            Ok(block) => guard.resolve(Ok(block)),
            Err(e) => {
                // Same error-forwarding semantics as the foreground path: a
                // decode error is deterministic for this block, so publishing
                // it to waiters matches what their own decode would produce.
                tracing::debug!(error = %e, kind = ?kind, "block prefetch decode failed");
                guard.resolve(Err(Arc::new(e)));
            }
        }
    }
}

impl CogReader {
    /// Resolve every `(tx, ty)` in `coords` to a decoded block. Probes the
    /// shared cache first, issues ONE coalesced `fetch_tiles` for the misses,
    /// then decodes each miss exactly once via the cache's single-flight.
    async fn blocks_for(
        &self,
        level: usize,
        kind: BlockKind,
        ifd: &ImageFileDirectory,
        coords: &[(usize, usize)],
    ) -> AgResult<Vec<Arc<DecodedBlock>>> {
        #[cfg(feature = "perf-tracing")]
        let started = Instant::now();
        let mut keys = Vec::with_capacity(coords.len());
        for &(tx, ty) in coords {
            keys.push(BlockKey {
                source: Arc::clone(&self.source_id),
                level: level as u32,
                kind,
                tx: tx as u32,
                ty: ty as u32,
            });
        }
        let mut resolved: Vec<Option<Arc<DecodedBlock>>> = Vec::with_capacity(keys.len());
        let mut miss_idx: Vec<usize> = Vec::with_capacity(keys.len());
        let mut miss_coords: Vec<(usize, usize)> = Vec::with_capacity(keys.len());
        for (i, k) in keys.iter().enumerate() {
            match self.block_cache.get(k).await {
                Some(b) => resolved.push(Some(b)),
                None => {
                    resolved.push(None);
                    miss_idx.push(i);
                    miss_coords.push(coords[i]);
                }
            }
        }
        #[cfg(feature = "perf-tracing")]
        let cache_probe_done = started.elapsed();
        #[cfg(feature = "perf-tracing")]
        let cache_hits = keys.len().saturating_sub(miss_idx.len());
        #[cfg(feature = "perf-tracing")]
        let mut owned_count = 0usize;
        #[cfg(feature = "perf-tracing")]
        let mut pending_count = 0usize;
        #[cfg(feature = "perf-tracing")]
        let mut fetch_ms = Duration::ZERO;
        #[cfg(feature = "perf-tracing")]
        let mut decode_ms = Duration::ZERO;
        #[cfg(feature = "perf-tracing")]
        let mut pending_wait_ms = Duration::ZERO;

        // Retry loop: claim → fetch+resolve owned → await pending.
        // Pending waiters that observe Ok(None) (owner aborted) re-enter the
        // next pass with a fresh claim instead of propagating a 500 to the caller.
        // Each pass preserves the deadlock-freedom invariant: all owned guards are
        // resolved (JoinSet drained) before any pending awaits begin.
        const MAX_CANCEL_RETRIES: u8 = 3;
        let mut retry: Vec<(usize, (usize, usize))> = miss_idx
            .iter()
            .zip(miss_coords.iter())
            .map(|(&mi, &coord)| (mi, coord))
            .collect();
        let mut cancel_attempts: u8 = 0;

        while !retry.is_empty() {
            let mut to_fetch: Vec<(usize, (usize, usize), InflightGuard)> = Vec::new();
            let mut pending: Vec<(usize, (usize, usize), PendingBlock)> = Vec::new();
            for &(mi, coord) in &retry {
                match self.block_cache.claim(&keys[mi]) {
                    Claim::Owned(guard) => to_fetch.push((mi, coord, guard)),
                    Claim::Pending(rx) => pending.push((mi, coord, rx)),
                }
            }
            crate::perf_trace! {
                owned_count += to_fetch.len();
                pending_count += pending.len();
            }

            if !to_fetch.is_empty() {
                // One coalesced fetch for only the blocks we own (object_store
                // merges adjacent ranges). On error, the guards drop → waiters
                // observe Ok(None) and retry.
                let fetch_coords: Vec<(usize, usize)> =
                    to_fetch.iter().map(|(_, c, _)| *c).collect();
                self.block_cache.record_fetched(fetch_coords.len() as u64);
                #[cfg(feature = "perf-tracing")]
                let fetch_started = Instant::now();
                let tiles = ifd
                    .fetch_tiles(&fetch_coords, &self.reader)
                    .await
                    .map_err(|e| AgError::Tiff(format!("fetch_tiles: {e}")))?;
                crate::perf_trace! {
                    fetch_ms += fetch_started.elapsed();
                }
                // Decode owned misses CONCURRENTLY (bounded by the CPU limiter).
                // `try_get_with` single-flights each block's decode + insert;
                // the guard then publishes the result to any waiters.
                let mut set = tokio::task::JoinSet::new();
                for ((mi, _coord, guard), tile) in to_fetch.into_iter().zip(tiles.into_iter()) {
                    let key = keys[mi].clone();
                    #[cfg(feature = "perf-tracing")]
                    let log_level = key.level;
                    #[cfg(feature = "perf-tracing")]
                    let log_kind = key.kind;
                    #[cfg(feature = "perf-tracing")]
                    let log_tx = key.tx;
                    #[cfg(feature = "perf-tracing")]
                    let log_ty = key.ty;
                    let sem = self.cpu.clone();
                    let cache = Arc::clone(&self.block_cache);
                    set.spawn(async move {
                        let block_res: AgResult<Arc<DecodedBlock>> = cache
                            .try_get_with(key, async move {
                                #[cfg(feature = "perf-tracing")]
                                let decode_started = Instant::now();
                                #[cfg(feature = "perf-tracing")]
                                let wait_started = Instant::now();
                                let permit = sem.acquire().await?;
                                #[cfg(feature = "perf-tracing")]
                                let cpu_wait_ms = wait_started.elapsed();
                                #[cfg(feature = "perf-tracing")]
                                let job_started = Instant::now();
                                let arr = tokio::task::spawn_blocking(move || {
                                    let _permit = permit;
                                    crate::io::decode::decode_one(tile)
                                })
                                .await
                                .map_err(|e| AgError::Tiff(format!("join: {e}")))??;
                                #[cfg(feature = "perf-tracing")]
                                let decode_job_ms = job_started.elapsed();
                                crate::perf_trace! {
                                    tracing::debug!(
                                        level = log_level,
                                        kind = ?log_kind,
                                        tx = log_tx,
                                        ty = log_ty,
                                        cpu_wait_ms = duration_ms(cpu_wait_ms),
                                        decode_job_ms = duration_ms(decode_job_ms),
                                        total_ms = duration_ms(decode_started.elapsed()),
                                        "decode_block timing"
                                    );
                                }
                                Ok(DecodedBlock::new(arr))
                            })
                            .await;
                        // Publish to concurrent waiters (Arc-wrap the error since
                        // AgError isn't Clone), then return to this caller.
                        let forward: BlockResult = match &block_res {
                            Ok(b) => Ok(Arc::clone(b)),
                            Err(e) => Err(Arc::new(AgError::Tiff(format!("block fetch: {e}")))),
                        };
                        guard.resolve(forward);
                        block_res.map(|b| (mi, b))
                    });
                }
                #[cfg(feature = "perf-tracing")]
                let decode_started = Instant::now();
                while let Some(res) = set.join_next().await {
                    let (mi, block) =
                        res.map_err(|e| AgError::Tiff(format!("decode task: {e}")))??;
                    resolved[mi] = Some(block);
                }
                crate::perf_trace! {
                    decode_ms += decode_started.elapsed();
                }
            }

            // INVARIANT (deadlock-freedom): every owned guard above is resolved
            // (inside the drained JoinSet) BEFORE we await any pending block.
            // This breaks cross-request wait cycles — e.g. request A owns {1,2}
            // and waits on {3} while request B owns {3} and waits on {1,2}: both
            // publish their owned results before blocking on the other's, so
            // neither stalls. Do NOT move pending awaits before the JoinSet drain.
            #[cfg(feature = "perf-tracing")]
            let pending_started = Instant::now();
            let mut next_retry: Vec<(usize, (usize, usize))> = Vec::new();
            for (mi, coord, rx) in pending {
                match BlockCache::await_pending(rx).await {
                    Ok(Some(block)) => {
                        resolved[mi] = Some(block);
                    }
                    Ok(None) => {
                        // Owner aborted (disconnect/cancellation): re-claim next pass.
                        next_retry.push((mi, coord));
                    }
                    Err(e) => return Err(e),
                }
            }
            crate::perf_trace! {
                pending_wait_ms += pending_started.elapsed();
            }

            if !next_retry.is_empty() {
                cancel_attempts += 1;
                if cancel_attempts >= MAX_CANCEL_RETRIES {
                    return Err(AgError::Tiff(
                        "in-flight block fetch canceled after retries".into(),
                    ));
                }
            }
            retry = next_retry;
        }
        crate::perf_trace! {
            tracing::debug!(
                level,
                kind = ?kind,
                requested = keys.len(),
                cache_hits,
                cache_misses = miss_idx.len(),
                owned = owned_count,
                pending = pending_count,
                cache_probe_ms = duration_ms(cache_probe_done),
                fetch_ms = duration_ms(fetch_ms),
                decode_ms = duration_ms(decode_ms),
                pending_wait_ms = duration_ms(pending_wait_ms),
                total_ms = duration_ms(started.elapsed()),
                "blocks_for timing"
            );
        }
        resolved
            .into_iter()
            .map(|b| b.ok_or_else(|| AgError::Tiff("block unresolved after fetch".into())))
            .collect()
    }

    /// Spawn (at most [`MAX_PREFETCH_TASKS`] concurrently) a detached task that
    /// fetches + decodes the block ring around the window just read. Best-effort
    /// by design: it never blocks or fails the foreground request, dedups with
    /// in-flight foreground fetches via [`BlockCache::claim`], decodes one block
    /// at a time (gentle on the shared CPU budget), and skips entirely when the
    /// task budget is exhausted.
    fn maybe_spawn_prefetch(&self, level: usize, tx_range: (u32, u32), ty_range: (u32, u32)) {
        use std::sync::atomic::AtomicUsize;
        if self.prefetch_ring == 0 {
            return;
        }
        if self
            .prefetch_inflight
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < MAX_PREFETCH_TASKS).then_some(n + 1)
            })
            .is_err()
        {
            return;
        }
        /// Decrements the in-flight counter however the task ends.
        struct Slot(Arc<AtomicUsize>);
        impl Drop for Slot {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let slot = Slot(Arc::clone(&self.prefetch_inflight));

        let ring = i64::from(self.prefetch_ring);
        let mut targets: Vec<(BlockKind, ImageFileDirectory)> = Vec::with_capacity(2);
        match self.data_ifd(level) {
            Ok(ifd) => targets.push((BlockKind::Data, ifd.clone())),
            Err(_) => return, // slot decrements via drop
        }
        if let Ok(Some(mask_ifd)) = self.mask_ifd(level) {
            targets.push((BlockKind::Mask, mask_ifd.clone()));
        }
        let reader = self.reader.clone();
        let cache = Arc::clone(&self.block_cache);
        let cpu = self.cpu.clone();
        let source = Arc::clone(&self.source_id);
        tokio::spawn(async move {
            let _slot = slot;
            for (kind, ifd) in targets {
                let Ok(info) = IfdInfo::from(&ifd).validate() else {
                    continue;
                };
                let coords = ring_coords(&info, tx_range, ty_range, ring);
                if coords.is_empty() {
                    continue;
                }
                prefetch_blocks(&ifd, kind, level, &coords, &source, &cache, &cpu, &reader).await;
            }
        });
    }

    /// Build the `ImageData` from a stitched window: applies alpha-band → mask
    /// promotion (rio-tiler priority: alpha > nodata), then derives the window
    /// transform / bbox / nodata. Pixel output is identical to the previous
    /// per-window decode path.
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
        // Alpha-band → mask promotion. Only applies when no separate mask IFD
        // was present and the data IFD's ExtraSamples tags one band as alpha.
        // `split_off_band` only runs when the index is in range.
        let alpha_idx = self.alpha_bands.get(level).copied().flatten();
        let (pixels, mask) = match (mask, alpha_idx) {
            (None, Some(b)) if b < pixels.bands() => {
                if let Some((rest, alpha)) = pixels.split_off_band(b) {
                    (rest, Some(alpha))
                } else {
                    // Unreachable given the guard above, but keep the path total.
                    return Err(AgError::Tiff("alpha-band split failed".into()));
                }
            }
            (m, _) => (pixels, m),
        };

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

    let gt = if let Some(t) = ifd.model_transformation() {
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
        } else {
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

/// Pure overview-selection logic, faithful to rio-tiler `get_overview_level`.
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
        if (cur - target_gsd).abs() < 0.1 {
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
mod scale_offset_reader_tests {
    #[tokio::test]
    async fn band_scale_offset_defaults_to_identity() {
        use crate::io::{CogReader, ObjectReader};
        use object_store::local::LocalFileSystem;
        use std::sync::Arc;
        let fs = Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        let r = CogReader::open_with_reader(ObjectReader::new(
            fs,
            object_store::path::Path::from("data/cog_uint8_rgb_nodata.tif"),
        ))
        .await
        .unwrap();
        // RGB fixture without SCALE/OFFSET metadata → identity per band.
        let so = r.band_scale_offset();
        assert!(so.iter().all(|s| s.is_identity()));
        assert_eq!(so.len(), 3);
    }
}

#[cfg(test)]
mod open_object_store_tests {
    use crate::io::{CogReader, ObjectReader};
    use object_store::local::LocalFileSystem;
    use std::sync::Arc;

    #[tokio::test]
    async fn open_object_store_matches_open_with_reader() {
        let fs = Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        let key = "data/cog_uint8_rgb_nodata.tif";

        // New convenience path.
        let via_helper = CogReader::open_object_store(fs.clone(), key)
            .await
            .expect("open_object_store");

        // Old explicit path (must still produce an equivalent reader).
        let reader = ObjectReader::new(fs, object_store::path::Path::from(key));
        let via_reader = CogReader::open_with_reader(reader)
            .await
            .expect("open_with_reader");

        // GeoTransform does not impl PartialEq (wraps raffine::Affine);
        // compare wgs84_bounds() which returns [f64; 4] and does.
        assert_eq!(
            via_helper.wgs84_bounds().unwrap(),
            via_reader.wgs84_bounds().unwrap()
        );
        assert_eq!(via_helper.src_crs(), via_reader.src_crs());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open_local_for_test(
        tif: &std::path::Path,
        source_id: &str,
        cache: std::sync::Arc<crate::io::block_cache::BlockCache>,
    ) -> CogReader {
        use async_tiff::reader::ObjectReader;
        use object_store::local::LocalFileSystem;
        use object_store::path::Path as OsPath;
        let os_path = OsPath::from_filesystem_path(tif).unwrap();
        let reader = ObjectReader::new(std::sync::Arc::new(LocalFileSystem::new()), os_path);
        CogReader::builder()
            .source_id(source_id)
            .block_cache(cache)
            .open(reader)
            .await
            .unwrap()
    }

    #[test]
    fn ring_coords_clamps_to_grid_and_excludes_inner_rect() {
        use crate::io::window::IfdInfo;
        use async_tiff::tags::PlanarConfiguration;
        // 4×4 block grid (1024/256), window = block (0,0), ring = 1.
        let info = IfdInfo::from_parts(256, 256, 3, PlanarConfiguration::Planar, 1024, 1024)
            .validate()
            .unwrap();
        let mut got = super::ring_coords(&info, (0, 0), (0, 0), 1);
        got.sort_unstable();
        // Corner window: only the 3 in-grid neighbours; (−1, *) clamped away.
        assert_eq!(got, vec![(0, 1), (1, 0), (1, 1)]);

        // Interior window gets the full 8-neighbour ring.
        let mut got = super::ring_coords(&info, (1, 1), (1, 1), 1);
        got.sort_unstable();
        assert_eq!(got.len(), 8);
        assert!(!got.contains(&(1, 1)), "inner rect must be excluded");

        // Window already covering the whole grid → nothing to prefetch.
        assert!(super::ring_coords(&info, (0, 3), (0, 3), 1).is_empty());
    }

    #[tokio::test]
    async fn prefetch_ring_warms_neighbor_blocks() {
        use crate::io::block_cache::BlockCache;
        use crate::io::cache::CacheConfig;
        use crate::io::window::IfdInfo;
        use async_tiff::reader::ObjectReader;
        use object_store::local::LocalFileSystem;
        use object_store::path::Path as OsPath;

        let tif = std::env::current_dir()
            .unwrap()
            .join("data/cog_uint8_rgb_nodata.tif");
        if !tif.exists() {
            eprintln!("fixture missing, skipping");
            return;
        }
        let cache = std::sync::Arc::new(BlockCache::new(CacheConfig::default()));
        let os_path = OsPath::from_filesystem_path(&tif).unwrap();
        let reader = ObjectReader::new(std::sync::Arc::new(LocalFileSystem::new()), os_path);
        let cog = CogReader::builder()
            .source_id("prefetch-test")
            .block_cache(std::sync::Arc::clone(&cache))
            .prefetch_ring(1)
            .open(reader)
            .await
            .unwrap();

        // Need at least a 2-block grid at level 0 for a ring to exist.
        let info = IfdInfo::from(cog.data_ifd(0).unwrap()).validate().unwrap();
        let ring = super::ring_coords(&info, (0, 0), (0, 0), 1);
        if ring.is_empty() {
            eprintln!("fixture is single-block at level 0, skipping");
            return;
        }

        // Read a window inside block (0, 0) only.
        cog.read_window(0, 0, 0, 16, 16).await.unwrap();

        // The detached prefetch task should eventually decode the ring blocks
        // (data kind at minimum). Poll with a deadline; no decode budget
        // contention exists in this test so it resolves in milliseconds.
        let want = 1 + ring.len() as u64; // foreground block + its ring
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let stats = cache.stats();
            if stats.decoded >= want {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "prefetch did not warm ring blocks in time: decoded={} want>={want}",
                stats.decoded
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn read_window_reuses_blocks_on_identical_window() {
        use crate::io::block_cache::BlockCache;
        use crate::io::cache::CacheConfig;
        let tif = std::env::current_dir()
            .unwrap()
            .join("data/cog_uint8_rgb_nodata.tif");
        if !tif.exists() {
            eprintln!("fixture missing, skipping");
            return;
        }
        let cache = std::sync::Arc::new(BlockCache::new(CacheConfig::default()));
        let r = open_local_for_test(&tif, "src-A", cache.clone()).await;
        let a = r.read_window(0, 0, 0, 64, 64).await.unwrap();
        let n1 = cache.sync_count().await;
        let b = r.read_window(0, 0, 0, 64, 64).await.unwrap();
        let n2 = cache.sync_count().await;
        assert_eq!(
            a.pixels.to_f64(),
            b.pixels.to_f64(),
            "identical window → identical pixels"
        );
        assert!(n1 > 0, "blocks must be cached");
        assert_eq!(n1, n2, "identical window must not add blocks");
    }

    #[tokio::test]
    async fn distinct_sources_add_separate_blocks() {
        use crate::io::block_cache::BlockCache;
        use crate::io::cache::CacheConfig;
        let tif = std::env::current_dir()
            .unwrap()
            .join("data/cog_uint8_rgb_nodata.tif");
        if !tif.exists() {
            eprintln!("fixture missing, skipping");
            return;
        }
        let cache = std::sync::Arc::new(BlockCache::new(CacheConfig::default()));
        let a = open_local_for_test(&tif, "src-A", cache.clone()).await;
        let b = open_local_for_test(&tif, "src-B", cache.clone()).await;
        let _ = a.read_window(0, 0, 0, 32, 32).await.unwrap();
        let na = cache.sync_count().await;
        let _ = b.read_window(0, 0, 0, 32, 32).await.unwrap();
        let nb = cache.sync_count().await;
        assert!(
            nb > na,
            "different source ids must add separate block entries"
        );
    }

    #[tokio::test]
    async fn concurrent_identical_cold_reads_single_flight_the_fetch() {
        use crate::io::block_cache::BlockCache;
        use crate::io::cache::CacheConfig;
        let tif = std::env::current_dir()
            .unwrap()
            .join("data/cog_uint8_rgb_nodata.tif");
        if !tif.exists() {
            eprintln!("fixture missing, skipping");
            return;
        }
        let cache = std::sync::Arc::new(BlockCache::new(CacheConfig::default()));
        let r = std::sync::Arc::new(open_local_for_test(&tif, "src-sf", cache.clone()).await);

        // Two requests race the SAME cold window. `claim` is synchronous, so the
        // first to enter the miss path owns every block's fetch; the second sees
        // the in-flight entries and awaits them instead of re-fetching.
        let r1 = std::sync::Arc::clone(&r);
        let r2 = std::sync::Arc::clone(&r);
        let (a, b) = tokio::join!(
            async move { r1.read_window(0, 0, 0, 64, 64).await },
            async move { r2.read_window(0, 0, 0, 64, 64).await },
        );
        let a = a.unwrap();
        let b = b.unwrap();
        assert_eq!(
            a.pixels.to_f64(),
            b.pixels.to_f64(),
            "both racing readers must observe identical pixels"
        );

        let blocks = cache.sync_count().await;
        let fetched = cache.stats().fetched;
        assert!(blocks > 0, "cold reads must populate the cache");
        // Single-flight: each cold block is fetched exactly once across the two
        // concurrent requests. Without dedup the loser would re-fetch every
        // block, driving `fetched` up to 2× the number of cached blocks.
        assert_eq!(
            fetched, blocks,
            "each cold block must be fetched exactly once despite the race"
        );
    }

    #[test]
    fn overview_brackets_target_from_below_downsample() {
        // target 30 sits between L1(20) and L2(40): rio-tiler picks the
        // FINER bracket (L1) and downsamples 20→30. Not L2 (upsample).
        assert_eq!(pick_overview_from_gsds(&TEST_OVERVIEW_PYRAMID, 30.0), 1);
    }

    #[test]
    fn overview_zoomed_in_uses_full_res() {
        // target finer than native → full res.
        assert_eq!(pick_overview_from_gsds(&TEST_OVERVIEW_PYRAMID, 5.0), 0);
        assert_eq!(pick_overview_from_gsds(&TEST_OVERVIEW_PYRAMID, 10.0), 0);
    }

    #[test]
    fn overview_snaps_within_tolerance() {
        // target ≈ L2(40) within 0.1 → snap to L2.
        assert_eq!(pick_overview_from_gsds(&TEST_OVERVIEW_PYRAMID, 40.05), 2);
    }

    #[test]
    fn overview_coarser_than_all_uses_coarsest() {
        assert_eq!(pick_overview_from_gsds(&TEST_OVERVIEW_PYRAMID, 200.0), 3);
    }

    #[test]
    fn overview_single_level_is_zero() {
        assert_eq!(pick_overview_from_gsds(&[10.0], 100.0), 0);
    }

    #[test]
    fn reader_config_clamps_decode_limit_to_one() {
        let cfg = ReaderConfig {
            max_decode_tasks: 0,
            ..ReaderConfig::default()
        };
        assert_eq!(cfg.decode_task_limit(), 1);
    }

    #[test]
    fn split_http_url_extracts_parts() {
        let (base, path) = split_http_url("https://example.com/foo/bar.tif").unwrap();
        assert_eq!(base, "https://example.com");
        assert_eq!(path, "foo/bar.tif");
    }

    #[test]
    fn split_http_url_keeps_port() {
        let (base, _path) = split_http_url("http://localhost:8080/data/scene.tif").unwrap();
        assert_eq!(base, "http://localhost:8080");
    }

    #[test]
    fn non_ascii_path_round_trips_via_from_url_path() {
        use object_store::path::Path;

        // Mirror server::dataset_url: the `url` crate percent-encodes a
        // non-ASCII filename into the path segment.
        let mut u = url::Url::parse("http://127.0.0.1:8080").unwrap();
        u.path_segments_mut()
            .unwrap()
            .push("data")
            .push("测试-中文.tif");
        let (_base, path) = split_http_url(u.as_str()).unwrap();
        assert!(path.contains('%'), "split keeps the URL-encoded path");

        // The trap: `Path::from` treats input as a DECODED logical path,
        // so it re-encodes the `%`, double-encoding the filename.
        assert_ne!(Path::from(path.as_str()).as_ref(), "data/测试-中文.tif");

        // The fix: `Path::from_url_path` decodes the URL-encoded path, so
        // object_store re-encodes it exactly once for the loopback GET and
        // the server's `/data/{name}` handler sees the original filename.
        assert_eq!(
            Path::from_url_path(&path).unwrap().as_ref(),
            "data/测试-中文.tif"
        );
    }
}
