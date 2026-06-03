//! `CogReader` — async-tiff reader orchestration with GeoTransform/CRS derivation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use std::time::Duration;

use async_tiff::metadata::{cache::ReadaheadMetadataCache, TiffMetadataReader};
use async_tiff::reader::ObjectReader;
use async_tiff::tags::PhotometricInterpretation;
use async_tiff::{ImageFileDirectory, TIFF};
use object_store::http::HttpBuilder;
use object_store::path::Path;
use object_store::{ClientOptions, ObjectStore};
use tokio::sync::Semaphore;

use crate::blocking::default_blocking_permits;
use crate::bounds::{Bbox, GeoTransform};
use crate::crs::Crs;
use crate::error::{AgError, AgResult};
use crate::io::block_cache::{BlockCache, BlockKey, BlockKind, DecodedBlock};
use crate::io::cache::CacheConfig;
use crate::io::window::{tile_range_for_window, validate_window_shape, IfdInfo};
use crate::pixel::{ImageData, PixelData};
use ndarray::Array2;

static NEXT_SOURCE_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
const TEST_OVERVIEW_PYRAMID: [f64; 4] = [10.0, 20.0, 40.0, 80.0];

/// Runtime limits for a [`CogReader`].
///
/// `max_decode_tasks` caps concurrent CPU-heavy decode/stitch jobs submitted
/// through `spawn_blocking`. Waiting for a permit is async, so excess requests
/// yield the Tokio worker instead of flooding the blocking pool.
#[derive(Clone)]
pub struct ReaderConfig {
    pub max_decode_tasks: usize,
    /// Caller-supplied id for this logical source (same id ⇒ shared blocks).
    pub source_id: String,
    /// Shared decoded-block cache injected by the caller.
    pub block_cache: Arc<BlockCache>,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            max_decode_tasks: default_blocking_permits(),
            source_id: format!("cog-{}", NEXT_SOURCE_ID.fetch_add(1, Ordering::Relaxed)),
            block_cache: Arc::new(BlockCache::new(CacheConfig::default())),
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
    /// Per-band `(scale, offset)` from GDAL metadata (identity where absent).
    band_scale_offset: Vec<(f64, f64)>,
    /// Logical source id; combined with level/kind/tile to key the shared
    /// block cache so identical tiles from the same source are reused.
    source_id: Arc<str>,
    /// Shared decoded-block cache (may be shared across readers).
    block_cache: Arc<BlockCache>,
    decode_semaphore: Arc<Semaphore>,
}

impl CogReader {
    /// Open a remote COG over HTTPS (or any URL `object_store::http` supports).
    ///
    /// Uses tuned `ClientOptions` for tile-server workloads: large per-host
    /// idle pool so concurrent tile requests reuse connections instead of
    /// paying TLS handshake per fetch, and a 30s timeout.
    pub async fn open_http(url: &str) -> AgResult<Self> {
        Self::open_http_with_options(url, default_client_options()).await
    }

    /// Like [`Self::open_http`] but lets the caller override [`ClientOptions`]
    /// (e.g. timeouts, proxy, custom headers).
    pub async fn open_http_with_options(url: &str, options: ClientOptions) -> AgResult<Self> {
        Self::open_http_with_cache(url, options, CacheConfig::default()).await
    }

    /// Like [`Self::open_http_with_options`] but also lets the caller size the
    /// decoded-block cache.
    pub async fn open_http_with_cache(
        url: &str,
        options: ClientOptions,
        cache: CacheConfig,
    ) -> AgResult<Self> {
        Self::open_http_with_config(
            url,
            options,
            ReaderConfig {
                block_cache: Arc::new(crate::io::block_cache::BlockCache::new(cache)),
                ..ReaderConfig::default()
            },
        )
        .await
    }

    /// Like [`Self::open_http`] but lets the caller inject a full [`ReaderConfig`]
    /// (shared block cache, stable source id, decode concurrency) while using
    /// the default tuned [`ClientOptions`]. Use this to make several readers
    /// share one `Arc<BlockCache>` (and one memory budget): give each a distinct
    /// `source_id` and the same `block_cache`. `ReaderPool` does exactly this so
    /// all mosaic asset readers share one budget.
    pub async fn open_http_with_config_default(url: &str, config: ReaderConfig) -> AgResult<Self> {
        Self::open_http_with_config(url, default_client_options(), config).await
    }

    /// Like [`Self::open_http_with_cache`] but lets callers configure both cache
    /// capacity and CPU decode concurrency.
    pub async fn open_http_with_config(
        url: &str,
        options: ClientOptions,
        config: ReaderConfig,
    ) -> AgResult<Self> {
        let (base, path) = split_http_url(url)?;
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
        let reader = ObjectReader::new(store, object_path);
        Self::open_with_reader_and_config(reader, config).await
    }

    /// Open from an existing `ObjectReader` (lets callers wire any object_store).
    pub async fn open_with_reader(reader: ObjectReader) -> AgResult<Self> {
        Self::open_with_reader_and_cache(reader, CacheConfig::default()).await
    }

    /// Open from an existing `ObjectReader` with an explicit cache config.
    pub async fn open_with_reader_and_cache(
        reader: ObjectReader,
        cache: CacheConfig,
    ) -> AgResult<Self> {
        Self::open_with_reader_and_config(
            reader,
            ReaderConfig {
                block_cache: Arc::new(crate::io::block_cache::BlockCache::new(cache)),
                ..ReaderConfig::default()
            },
        )
        .await
    }

    /// Open from an existing `ObjectReader` with explicit cache and decode
    /// concurrency settings.
    pub async fn open_with_reader_and_config(
        reader: ObjectReader,
        config: ReaderConfig,
    ) -> AgResult<Self> {
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
            decode_semaphore: Arc::new(Semaphore::new(config.decode_task_limit())),
        })
    }

    /// Open a COG from an existing object store and key, building the
    /// `ObjectReader` internally. Lets callers wire any `object_store`
    /// backend (S3/MinIO, GCS, local, …) without naming `ObjectReader`.
    ///
    /// `key` must be a **decoded** object key (e.g. `"data/my file.tif"`),
    /// not a URL-encoded string — it is passed verbatim to `Path::from`.
    pub async fn open_object_store(store: Arc<dyn ObjectStore>, key: &str) -> AgResult<Self> {
        Self::open_object_store_with_config(store, key, ReaderConfig::default()).await
    }

    /// Like [`Self::open_object_store`] but lets the caller size the decoded-block
    /// cache.
    pub async fn open_object_store_with_cache(
        store: Arc<dyn ObjectStore>,
        key: &str,
        cache: CacheConfig,
    ) -> AgResult<Self> {
        Self::open_object_store_with_config(
            store,
            key,
            ReaderConfig {
                block_cache: Arc::new(crate::io::block_cache::BlockCache::new(cache)),
                ..ReaderConfig::default()
            },
        )
        .await
    }

    /// Like [`Self::open_object_store`] but lets callers configure cache capacity
    /// and CPU decode concurrency.
    pub async fn open_object_store_with_config(
        store: Arc<dyn ObjectStore>,
        key: &str,
        config: ReaderConfig,
    ) -> AgResult<Self> {
        let reader = ObjectReader::new(store, Path::from(key));
        Self::open_with_reader_and_config(reader, config).await
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
    pub async fn read_window(
        &self,
        level: usize,
        col_off: i64,
        row_off: i64,
        width: u64,
        height: u64,
    ) -> AgResult<Arc<ImageData>> {
        validate_window_shape(width, height)?;
        checked_window_end(col_off, width, "column")?;
        checked_window_end(row_off, height, "row")?;

        let ifd = self.data_ifd(level)?;
        let ((tx0, tx1), (ty0, ty1)) = tile_range_for_window(ifd, col_off, row_off, width, height)
            .ok_or_else(|| AgError::Tiff("ifd lacks tile dims".into()))?;
        let coords: Vec<(usize, usize)> = (tx0..=tx1)
            .flat_map(|x| (ty0..=ty1).map(move |y| (x as usize, y as usize)))
            .collect();
        let data_info = IfdInfo::from(ifd);

        let data_blocks = self
            .blocks_for(level, BlockKind::Data, ifd, &coords)
            .await?;
        let stitched = {
            let coords2 = coords.clone();
            let info = data_info;
            let (w, h) = (width, height);
            let (co, ro) = (col_off, row_off);
            let permit = Arc::clone(&self.decode_semaphore)
                .acquire_owned()
                .await
                .map_err(|_| AgError::Tiff("decode semaphore closed".into()))?;
            // Move the Arc<DecodedBlock> handles (no pixel copy) into the
            // blocking job and stitch from references built INSIDE the closure.
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let refs: Vec<&async_tiff::Array> = data_blocks.iter().map(|b| b.array()).collect();
                crate::io::window::stitch_with_info(&refs, &coords2, &info, co, ro, w, h)
            })
            .await
            .map_err(|e| AgError::Tiff(format!("join: {e}")))??
        };

        let mask = if let Some(mask_ifd) = self.mask_ifd(level)? {
            let mask_info = IfdInfo::from(mask_ifd);
            let mask_blocks = self
                .blocks_for(level, BlockKind::Mask, mask_ifd, &coords)
                .await?;
            let coords2 = coords.clone();
            let (w, h) = (width, height);
            let (co, ro) = (col_off, row_off);
            let permit = Arc::clone(&self.decode_semaphore)
                .acquire_owned()
                .await
                .map_err(|_| AgError::Tiff("decode semaphore closed".into()))?;
            let m = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let refs: Vec<&async_tiff::Array> = mask_blocks.iter().map(|b| b.array()).collect();
                crate::io::window::stitch_mask_with_info(&refs, &coords2, &mask_info, co, ro, w, h)
            })
            .await
            .map_err(|e| AgError::Tiff(format!("join: {e}")))??;
            Some(m)
        } else {
            None
        };

        let image = self.assemble_image(level, stitched, mask, col_off, row_off, width, height)?;
        tracing::debug!(level, tiles = coords.len(), "read_window (block cache)");
        Ok(Arc::new(image))
    }

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
        let keys: Vec<BlockKey> = coords
            .iter()
            .map(|&(tx, ty)| BlockKey {
                source: Arc::clone(&self.source_id),
                level: level as u32,
                kind,
                tx: tx as u32,
                ty: ty as u32,
            })
            .collect();
        let mut resolved: Vec<Option<Arc<DecodedBlock>>> = Vec::with_capacity(keys.len());
        let mut miss_idx: Vec<usize> = Vec::new();
        let mut miss_coords: Vec<(usize, usize)> = Vec::new();
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
        if !miss_coords.is_empty() {
            let tiles = ifd
                .fetch_tiles(&miss_coords, &self.reader)
                .await
                .map_err(|e| AgError::Tiff(format!("fetch_tiles: {e}")))?;
            for (mi, tile) in miss_idx.iter().zip(tiles.into_iter()) {
                let key = keys[*mi].clone();
                let sem = Arc::clone(&self.decode_semaphore);
                let block = self
                    .block_cache
                    .try_get_with(key, async move {
                        let permit = sem
                            .acquire_owned()
                            .await
                            .map_err(|_| AgError::Tiff("decode semaphore closed".into()))?;
                        let arr = tokio::task::spawn_blocking(move || {
                            let _permit = permit;
                            crate::io::decode::decode_one(tile)
                        })
                        .await
                        .map_err(|e| AgError::Tiff(format!("join: {e}")))??;
                        Ok(DecodedBlock::new(arr))
                    })
                    .await?;
                resolved[*mi] = Some(block);
            }
        }
        resolved
            .into_iter()
            .map(|b| b.ok_or_else(|| AgError::Tiff("block unresolved after fetch".into())))
            .collect()
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

    /// Per-band `(scale, offset)` from GDAL metadata; identity `(1.0, 0.0)`
    /// where absent. Apply as `physical = raw * scale + offset` when unscaling.
    pub fn band_scale_offset(&self) -> &[(f64, f64)] {
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
        assert!(so.iter().all(|&(s, o)| s == 1.0 && o == 0.0));
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
        CogReader::open_with_reader_and_config(
            reader,
            ReaderConfig {
                source_id: source_id.to_string(),
                block_cache: cache,
                ..ReaderConfig::default()
            },
        )
        .await
        .unwrap()
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
