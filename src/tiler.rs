//! Top-level orchestrator: HTTP request → tile bytes.

use std::sync::Arc;
use std::time::Duration;
#[cfg(feature = "perf-tracing")]
use std::time::Instant;

use bytes::Bytes;

use crate::blocking::{default_blocking_permits, CpuLimiter};
use crate::bounds::Bbox;
use crate::error::{AgError, AgResult};
use crate::io::CogReader;
#[cfg(feature = "perf-tracing")]
use crate::perf::duration_ms;
use crate::pixel::ImageData;
use crate::request::{
    RequestContext, RequestLimiter, RequestOptions, DEFAULT_MAX_IN_FLIGHT_REQUESTS,
    DEFAULT_REQUEST_TIMEOUT,
};
use crate::style::params::compile_cached;
use crate::style::TileStyle;
use crate::tms::{TileCoord, Tms};

const PASSTHROUGH_PIXEL_TOLERANCE: f64 = 1e-3;

#[derive(Clone)]
pub struct Tiler {
    reader: Arc<CogReader>,
    cpu: CpuLimiter,
    requests: RequestLimiter,
    request_timeout: Duration,
    work_budget: crate::resource::WorkBudget,
}

/// Runtime limits for a [`Tiler`].
///
/// `max_blocking_tasks` caps concurrent warp/render jobs. The permit wait is
/// async, while the CPU-heavy work still runs on Tokio's blocking pool.
#[derive(Clone, Copy, Debug)]
pub struct TilerConfig {
    pub max_blocking_tasks: usize,
    /// Maximum admitted public requests; excess requests fail immediately.
    pub max_in_flight_requests: usize,
    /// Includes IO and waits for CPU permits. Blocking work stops at stage boundaries.
    pub request_timeout: Duration,
}

impl Default for TilerConfig {
    fn default() -> Self {
        Self {
            max_blocking_tasks: default_blocking_permits(),
            max_in_flight_requests: DEFAULT_MAX_IN_FLIGHT_REQUESTS,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

impl TilerConfig {
    fn blocking_task_limit(self) -> usize {
        self.max_blocking_tasks.max(1)
    }
}

/// Resolved read/warp plan for one output tile (see [`Tiler::plan_tile`]).
struct ReadyPlan {
    level: usize,
    col: i64,
    row: i64,
    w: u64,
    h: u64,
    /// The window is an exact integer-aligned identity crop — skip the warp.
    passthrough: bool,
    dst_crs: crate::crs::Crs,
    dst_bbox: Bbox,
}

impl Tiler {
    /// Build a `Tiler` that shares the reader's [`CpuLimiter`], so decode,
    /// stitch, warp and render all draw from one CPU budget. In a mosaic, where
    /// every asset's reader shares one limiter (via `ReaderPool`), this bounds
    /// the whole mosaic's CPU work — preventing oversubscription. Use
    /// [`Self::with_config`] to give a single-source tiler its own bound.
    pub fn new(reader: Arc<CogReader>) -> Self {
        let cpu = reader.cpu_limiter();
        let work_budget = reader.work_budget();
        Self {
            reader,
            cpu,
            requests: RequestLimiter::default(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            work_budget,
        }
    }

    /// Build a `Tiler` with an independent CPU bound sized by
    /// `config.max_blocking_tasks` (does not share the reader's limiter). Prefer
    /// [`Self::new`] when you want one shared process/mosaic-wide budget.
    pub fn with_config(reader: Arc<CogReader>, config: TilerConfig) -> Self {
        let work_budget = reader.work_budget();
        Self {
            reader,
            cpu: CpuLimiter::new(config.blocking_task_limit()),
            requests: RequestLimiter::new(config.max_in_flight_requests),
            request_timeout: config.request_timeout,
            work_budget,
        }
    }

    /// Share admission across multiple datasets or a serving application.
    pub fn with_request_limiter(mut self, requests: RequestLimiter) -> Self {
        self.requests = requests;
        self
    }

    /// Share a byte budget for active source, warp, and render buffers.
    pub fn with_work_budget(mut self, work_budget: crate::resource::WorkBudget) -> Self {
        self.work_budget = work_budget;
        self
    }

    /// Whether every source sample is an unsigned 8-bit integer.
    pub fn is_u8(&self) -> AgResult<bool> {
        let ifd = self.reader.data_ifd(0)?;
        Ok(ifd.bits_per_sample().iter().all(|&bits| bits == 8)
            && ifd
                .sample_format()
                .iter()
                .all(|format| *format == async_tiff::tags::SampleFormat::Uint))
    }

    /// Number of renderable bands after an ExtraSamples alpha channel is removed.
    pub fn band_count(&self) -> AgResult<usize> {
        use async_tiff::tags::ExtraSamples;
        let ifd = self.reader.data_ifd(0)?;
        let samples = usize::from(ifd.samples_per_pixel());
        let alpha = ifd.extra_samples().is_some_and(|extras| {
            extras.len() <= samples
                && extras.iter().any(|sample| {
                    matches!(
                        sample,
                        ExtraSamples::AssociatedAlpha | ExtraSamples::UnassociatedAlpha
                    )
                })
        });
        Ok(samples - usize::from(alpha))
    }

    /// WGS84 `[west, south, east, north]` extent of the dataset.
    pub fn wgs84_bounds(&self) -> AgResult<[f64; 4]> {
        self.reader.wgs84_bounds()
    }

    /// Pure planning: destination grid → overview level + clamped pixel window.
    /// Returns `None` when the tile is fully outside the COG or outside the
    /// projection's valid domain. Shared by [`Self::warp_tile`] (mosaic
    /// per-asset path) and the fused [`Self::tile`] path.
    fn plan_tile(&self, coord: TileCoord, style: &TileStyle) -> AgResult<Option<ReadyPlan>> {
        let tms = Tms::cached(coord.tms)?;
        let dst_bbox = tms.tile_bbox(coord.z, coord.x, coord.y)?;
        let dst_crs = tms.crs();

        let bbox_in_src = if self.reader.src_crs() == &dst_crs {
            dst_bbox
        } else {
            match Self::reproject_bbox_sync(&dst_bbox, &dst_crs, self.reader.src_crs()) {
                Ok(b) => b,
                Err(AgError::ProjectionDomain(_)) => return Ok(None),
                Err(e) => return Err(e),
            }
        };

        let target_gsd =
            bbox_in_src.width().abs().max(bbox_in_src.height().abs()) / f64::from(style.tilesize);
        let level = if style.raster.categorical {
            // Source overviews do not carry a category-preservation proof.
            0
        } else {
            self.reader.pick_overview(target_gsd)
        };

        let level_tr = self.reader.level_transform(level)?;
        let (rc, rr, rw, rh) = level_tr
            .window_for_bbox(&bbox_in_src)
            .ok_or_else(|| AgError::Warp("source GeoTransform not invertible".into()))?;
        let same_crs = self.reader.src_crs() == &dst_crs;
        let level_gsd = self.reader.level_gsd(level).unwrap_or(target_gsd);
        let res_matches = level_gsd > 0.0 && ((target_gsd - level_gsd).abs() / level_gsd) < 0.01;
        let aligned = same_crs && res_matches;
        let ts = style.tilesize as usize;

        // L2 passthrough: exact integer-aligned ts×ts window → skip warp.
        if aligned {
            if let Some((pc, pr)) = Self::passthrough_window(&level_tr, &dst_bbox, ts) {
                if let Ok((iw, ih)) = self.reader.level_dimensions(level) {
                    let in_bounds = pc >= 0
                        && pr >= 0
                        && (pc as u64 + ts as u64) <= iw as u64
                        && (pr as u64 + ts as u64) <= ih as u64;
                    if in_bounds {
                        return Ok(Some(ReadyPlan {
                            level,
                            col: pc,
                            row: pr,
                            w: ts as u64,
                            h: ts as u64,
                            passthrough: true,
                            dst_crs,
                            dst_bbox,
                        }));
                    }
                }
            }
        }

        let kernel = if same_crs {
            style.resampling
        } else {
            style.reproject
        };
        let halo = match kernel {
            crate::warp::resampler::Resampling::Lanczos => 3,
            crate::warp::resampler::Resampling::Cubic
            | crate::warp::resampler::Resampling::CubicSpline => 2,
            crate::warp::resampler::Resampling::Nearest => 0,
            _ => 1,
        };
        let raw = (
            rc.checked_sub(halo)
                .ok_or_else(|| AgError::Warp("column halo overflow".into()))?,
            rr.checked_sub(halo)
                .ok_or_else(|| AgError::Warp("row halo overflow".into()))?,
            rw.checked_add(2 * halo as u64)
                .ok_or_else(|| AgError::Warp("width halo overflow".into()))?,
            rh.checked_add(2 * halo as u64)
                .ok_or_else(|| AgError::Warp("height halo overflow".into()))?,
        );
        let (col, row, w, h) = self.clamp_window(level, raw.0, raw.1, raw.2, raw.3)?;
        if w == 0 || h == 0 {
            return Ok(None);
        }
        Ok(Some(ReadyPlan {
            level,
            col,
            row,
            w,
            h,
            passthrough: false,
            dst_crs,
            dst_bbox,
        }))
    }

    /// Read + warp the source COG onto the destination tile grid. Returns
    /// `None` when the tile is fully outside the COG or outside the
    /// projection's valid domain — [`Tiler::tile`] turns that into "no tile"
    /// (HTTP 204). Returns sorted, unique source dependencies; with
    /// mosaic.stage=after_channels it returns expression outputs instead.
    /// NoData is consumed into the mask. This is the per-asset mosaic primitive.
    pub async fn warp_tile(
        &self,
        coord: TileCoord,
        style: &TileStyle,
    ) -> AgResult<Option<Arc<ImageData>>> {
        self.warp_tile_with_options(coord, style, &RequestOptions::default())
            .await
    }

    /// Warp with a per-call deadline override and explicit cancellation handle.
    pub async fn warp_tile_with_options(
        &self,
        coord: TileCoord,
        style: &TileStyle,
        options: &RequestOptions,
    ) -> AgResult<Option<Arc<ImageData>>> {
        self.requests
            .run(self.request_timeout, options, |context| async move {
                self.work_budget
                    .run_request(&context, |attempt| async move {
                        self.warp_tile_in_request(coord, style, &attempt).await
                    })
                    .await
            })
            .await
    }

    pub(crate) async fn warp_tile_in_request(
        &self,
        coord: TileCoord,
        style: &TileStyle,
        context: &RequestContext,
    ) -> AgResult<Option<Arc<ImageData>>> {
        context.check()?;
        compile_cached(style).await?;
        self.check_raster_source()?;
        let Some(plan) = self.plan_tile_async(coord, style, context).await? else {
            return Ok(None);
        };
        let bands = u64::from(self.reader.data_ifd(plan.level)?.samples_per_pixel())
            .max(style.raster.mosaic_band_count() as u64);
        let bytes = tile_work_bytes(plan.w, plan.h, bands, u64::from(style.tilesize))?;
        let context = &context.with_work(self.work_budget.try_reserve(bytes)?);
        let mut src: Arc<ImageData> = self
            .reader
            .read_window(plan.level, plan.col, plan.row, plan.w, plan.h)
            .await?;
        context.check()?;
        if let Some(nodata) = style.nodata {
            Arc::make_mut(&mut src).nodata = Some(nodata);
        }
        let raster = style.raster.clone();
        let size = style.tilesize as usize;
        let kernel = if src.crs == plan.dst_crs {
            style.resampling
        } else {
            style.reproject
        };
        let cancellation = context.clone();
        self.run_tile_cpu(context, "raster mosaic channels", move || {
            let warped =
                match raster.warp_dependencies(&src, &plan.dst_crs, plan.dst_bbox, size, kernel) {
                    Ok(image) => image,
                    Err(AgError::ProjectionDomain(_)) => return Ok(None),
                    Err(error) => return Err(error),
                };
            cancellation.check()?;
            Ok(Some(Arc::new(raster.prepare_mosaic(warped))))
        })
        .await
    }

    /// Render one tile. Returns `Ok(None)` when the tile lies fully outside
    /// the dataset (or the projection's valid domain) — callers should treat
    /// that as "no tile" (e.g. HTTP 204) rather than synthesising a blank one,
    /// so out-of-range areas stay empty for every output format (a black JPEG
    /// would otherwise cover the map where PNG/WebP stay transparent).
    /// In-range tiles that happen to be all-nodata still render normally
    /// (transparent for PNG/WebP).
    #[cfg_attr(feature = "perf-tracing", tracing::instrument(
        skip(self, style),
        fields(tms = coord.tms.name(), z = coord.z, x = coord.x, y = coord.y, format = ?style.format),
        err
    ))]
    pub async fn tile(&self, coord: TileCoord, style: &TileStyle) -> AgResult<Option<Bytes>> {
        self.tile_with_options(coord, style, &RequestOptions::default())
            .await
    }

    /// Render with a per-call deadline override and explicit cancellation handle.
    pub async fn tile_with_options(
        &self,
        coord: TileCoord,
        style: &TileStyle,
        options: &RequestOptions,
    ) -> AgResult<Option<Bytes>> {
        self.requests
            .run(self.request_timeout, options, |context| async move {
                self.work_budget
                    .run_request(&context, |attempt| async move {
                        self.tile_in_request(coord, style, &attempt).await
                    })
                    .await
            })
            .await
    }

    pub(crate) fn check_raster_source(&self) -> AgResult<()> {
        {
            use async_tiff::tags::ExtraSamples;
            for level in 0..self.reader.level_count() {
                if self
                    .reader
                    .data_ifd(level)?
                    .extra_samples()
                    .is_some_and(|extras| {
                        extras.iter().any(|sample| {
                            matches!(
                                sample,
                                ExtraSamples::AssociatedAlpha | ExtraSamples::UnassociatedAlpha
                            )
                        })
                    })
                {
                    return Err(AgError::query("source_alpha", "Raster Style requires preserved fractional Alpha; this reader currently exposes a boolean mask"));
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn tile_in_request(
        &self,
        coord: TileCoord,
        style: &TileStyle,
        context: &RequestContext,
    ) -> AgResult<Option<Bytes>> {
        context.check()?;
        self.check_raster_source()?;
        style.raster.statistics.check_scope(false)?;
        #[cfg(feature = "perf-tracing")]
        let started = Instant::now();
        let compiled = compile_cached(style).await?;
        let Some(plan) = self.plan_tile_async(coord, style, context).await? else {
            crate::perf_trace! {
                tracing::debug!(
                    total_ms = duration_ms(started.elapsed()),
                    "tile internals timing: no tile"
                );
            }
            return Ok(None);
        };
        let raster = if style.raster.stretch.needs_statistics() {
            crate::style::statistics::resolve(
                style,
                std::slice::from_ref(&self.reader),
                false,
                &self.cpu,
                &self.work_budget,
                context,
            )
            .await?
        } else {
            compiled.raster.clone()
        };
        let bands = u64::from(self.reader.data_ifd(plan.level)?.samples_per_pixel());
        let bytes = tile_work_bytes(plan.w, plan.h, bands, u64::from(style.tilesize))?;
        let context = &context.with_work(self.work_budget.try_reserve(bytes)?);
        #[cfg(feature = "perf-tracing")]
        let planned = started.elapsed();
        let mut src: Arc<ImageData> = self
            .reader
            .read_window(plan.level, plan.col, plan.row, plan.w, plan.h)
            .await?;
        context.check()?;
        if let Some(nodata) = style.nodata {
            Arc::make_mut(&mut src).nodata = Some(nodata);
        }
        #[cfg(feature = "perf-tracing")]
        let read_done = started.elapsed();

        let ts = style.tilesize as usize;
        let kernel = if src.crs == plan.dst_crs {
            style.resampling
        } else {
            style.reproject
        };
        let cancellation = context.clone();
        let bytes = self
            .run_tile_cpu(context, "tile", move || {
                let result = if plan.passthrough {
                    raster.render(&src, src.nodata, compiled.format)
                } else {
                    raster.render_warped(
                        &src,
                        &plan.dst_crs,
                        plan.dst_bbox,
                        ts,
                        kernel,
                        compiled.format,
                    )
                };
                cancellation.check()?;
                match result {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(AgError::ProjectionDomain(_)) => Ok(None),
                    Err(error) => Err(error),
                }
            })
            .await?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        #[cfg(feature = "perf-tracing")]
        let done = started.elapsed();
        crate::perf_trace! {
            tracing::debug!(
                plan_ms = duration_ms(planned),
                read_window_ms = duration_ms(read_done - planned),
                fused_cpu_ms = duration_ms(done - read_done),
                total_ms = duration_ms(done),
                bytes = bytes.len(),
                "tile internals timing"
            );
        }
        Ok(Some(bytes))
    }

    async fn plan_tile_async(
        &self,
        coord: TileCoord,
        style: &TileStyle,
        context: &RequestContext,
    ) -> AgResult<Option<ReadyPlan>> {
        let this = self.clone();
        let style = style.clone();
        self.run_tile_cpu(context, "plan", move || this.plan_tile(coord, &style))
            .await
    }

    /// Run a pure-CPU closure `f` under one [`CpuLimiter`] permit.
    ///
    /// INVARIANT (deadlock-freedom): `f` must be **pure synchronous CPU** and
    /// must NOT acquire another permit from the same limiter (directly, or by
    /// calling back into a reader read). The limiter may be shared across decode,
    /// stitch, warp and render; the safety of sharing rests on each job holding
    /// at most ONE permit at a time. Reader reads (`read_window`) acquire+release
    /// their decode permits in the async layer *before* `run_tile_cpu` is called,
    /// so warp/render here never nest acquisition. Were `f` to await a second
    /// permit, N permit-holding jobs could starve all remaining acquisitions.
    async fn run_tile_cpu<T, F>(
        &self,
        context: &RequestContext,
        label: &'static str,
        f: F,
    ) -> AgResult<T>
    where
        T: Send + 'static,
        F: FnOnce() -> AgResult<T> + Send + 'static,
    {
        context.check()?;
        let permit = self
            .cpu
            .acquire()
            .await
            .map_err(|_| AgError::Warp(format!("{label} cpu limiter closed")))?;
        let context = context.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            context.check()?;
            let result = f();
            context.check()?;
            result
        })
        .await
        .map_err(|e| AgError::Warp(format!("{label} join: {e}")))?
    }

    /// Returns the integer `(col, row)` of an exact `ts×ts` passthrough
    /// window when the destination tile maps onto source pixels with
    /// negligible sub-pixel offset and unit scale. `None` means the read
    /// is not a clean identity crop and must go through the warp path.
    ///
    /// Tolerance: corners must be within 1e-3 px of integers and the span
    /// must round to exactly `ts` — anything looser would misregister the
    /// tile against the basemap.
    fn passthrough_window(
        level_tr: &crate::bounds::GeoTransform,
        dst_bbox: &Bbox,
        ts: usize,
    ) -> Option<(i64, i64)> {
        let (x0, y0) = level_tr.forward(0.0, 0.0);
        let (x1, y1) = level_tr.forward(1.0, 0.0);
        let (x2, y2) = level_tr.forward(0.0, 1.0);
        if !(x1 > x0 && y2 < y0 && y1 == y0 && x2 == x0) {
            return None;
        }
        let inv = level_tr.inverse()?;
        // Top-left and bottom-right corners → source pixel coords.
        let (c0, r0) = inv.forward(dst_bbox.xmin, dst_bbox.ymax);
        let (c1, r1) = inv.forward(dst_bbox.xmax, dst_bbox.ymin);
        let frac = |v: f64| (v - v.round()).abs();
        if frac(c0) > PASSTHROUGH_PIXEL_TOLERANCE || frac(r0) > PASSTHROUGH_PIXEL_TOLERANCE {
            return None;
        }
        let span_c = (c1 - c0).abs();
        let span_r = (r1 - r0).abs();
        if (span_c - ts as f64).abs() > PASSTHROUGH_PIXEL_TOLERANCE
            || (span_r - ts as f64).abs() > PASSTHROUGH_PIXEL_TOLERANCE
        {
            return None;
        }
        Some((c0.round() as i64, r0.round() as i64))
    }

    /// Reproject a bbox boundary synchronously, sampling curved edges and
    /// retaining valid portions of partially out-of-domain requests.
    /// Used only on the cross-CRS path; same-CRS callers don't reach here.
    fn reproject_bbox_sync(
        bbox: &crate::bounds::Bbox,
        from_crs: &crate::crs::Crs,
        src_crs: &crate::crs::Crs,
    ) -> AgResult<Bbox> {
        crate::warp::projection::reproject_boundary(
            [
                (bbox.xmin, bbox.ymin),
                (bbox.xmax, bbox.ymin),
                (bbox.xmax, bbox.ymax),
                (bbox.xmin, bbox.ymax),
            ],
            from_crs,
            src_crs,
        )
    }

    /// Clamp pixel window to the IFD extent; returns (0, 0, 0, 0)-equivalent
    /// when the window is fully outside.
    fn clamp_window(
        &self,
        level: usize,
        col: i64,
        row: i64,
        w: u64,
        h: u64,
    ) -> AgResult<(i64, i64, u64, u64)> {
        let ifd = self.reader.data_ifd(level)?;
        let iw = ifd.image_width() as i64;
        let ih = ifd.image_height() as i64;
        let col_end = (col + w as i64).min(iw);
        let row_end = (row + h as i64).min(ih);
        let col = col.max(0);
        let row = row.max(0);
        let w = (col_end - col).max(0) as u64;
        let h = (row_end - row).max(0) as u64;
        Ok((col, row, w, h))
    }
}

/// Conservative live working-set estimate: source/promotion and overlapping
/// warp rows, output, render, encoding and coordinate/mask buffers.
fn tile_work_bytes(width: u64, height: u64, bands: u64, tile_size: u64) -> AgResult<u64> {
    width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(bands))
        .and_then(|n| n.checked_mul(16))
        .and_then(|source| {
            tile_size
                .checked_mul(tile_size)
                .and_then(|pixels| {
                    bands
                        .checked_mul(48)
                        .and_then(|per_band| per_band.checked_add(48))
                        .and_then(|per_pixel| pixels.checked_mul(per_pixel))
                })
                .and_then(|tile| source.checked_add(tile))
        })
        .ok_or_else(|| AgError::ResourceLimit("tile working-set estimate overflow".into()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn passthrough_rejects_flipped_and_rotated_grids() {
        let bbox = crate::bounds::Bbox::new(0.0, 0.0, 256.0, 256.0);
        for gt in [
            [256.0, -1.0, 0.0, 256.0, 0.0, -1.0],
            [0.0, 0.0, 1.0, 256.0, -1.0, 0.0],
        ] {
            assert!(super::Tiler::passthrough_window(
                &crate::bounds::GeoTransform::from_gdal(&gt),
                &bbox,
                256
            )
            .is_none());
        }
    }
    use super::{Tiler, TilerConfig};
    use crate::bounds::{Bbox, GeoTransform};

    #[test]
    fn tiler_type_compiles() {
        let _ = std::mem::size_of::<Tiler>();
    }

    /// Standard slippy-map lon/lat -> (x, y) at zoom `z` (WebMercator).
    fn lonlat_to_tile(lon: f64, lat: f64, z: u8) -> (u32, u32) {
        let n = 2f64.powi(z as i32);
        let x = ((lon + 180.0) / 360.0 * n).floor().clamp(0.0, n - 1.0) as u32;
        let lat_rad = lat.to_radians();
        let y = ((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0
            * n)
            .floor()
            .clamp(0.0, n - 1.0) as u32;
        (x, y)
    }

    #[tokio::test]
    async fn tile_returns_none_outside_data_and_some_inside() {
        use crate::io::CogReader;
        use crate::style::TileStyle;
        use crate::tms::{TileCoord, TileMatrixSet};
        use async_tiff::reader::ObjectReader;
        use object_store::local::LocalFileSystem;
        use object_store::path::Path as OsPath;
        use std::sync::Arc;

        let tif = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/rgb_nodata.tif");

        let os_path = OsPath::from_filesystem_path(&tif).unwrap();
        let reader = CogReader::builder()
            .snapshot_policy(crate::io::SnapshotPolicy::AssumeImmutable)
            .open(ObjectReader::new(Arc::new(LocalFileSystem::new()), os_path))
            .await
            .unwrap();
        let tiler = Tiler::new(Arc::new(reader));
        let style = TileStyle::from_raster_query("version=2.0&type=gray&bidx=1").unwrap();
        let [w, s, e, n] = tiler.wgs84_bounds().unwrap();

        // A tile over the dataset centre must render actual bytes.
        let z = 12u8;
        let (cx, cy) = lonlat_to_tile((w + e) / 2.0, (s + n) / 2.0, z);
        let inside = tiler
            .tile(
                TileCoord {
                    tms: TileMatrixSet::WebMercatorQuad,
                    z,
                    x: cx,
                    y: cy,
                },
                &style,
            )
            .await
            .unwrap();
        assert!(inside.is_some(), "in-bounds tile must produce bytes");

        // The antipode is guaranteed outside any single real dataset → no tile.
        let anti_lon = {
            let l = (w + e) / 2.0 + 180.0;
            if l > 180.0 {
                l - 360.0
            } else {
                l
            }
        };
        let (ax, ay) = lonlat_to_tile(anti_lon, -(s + n) / 2.0, z);
        let outside = tiler
            .tile(
                TileCoord {
                    tms: TileMatrixSet::WebMercatorQuad,
                    z,
                    x: ax,
                    y: ay,
                },
                &style,
            )
            .await
            .unwrap();
        assert!(
            outside.is_none(),
            "fully out-of-bounds tile must return None, got bytes"
        );
    }

    #[tokio::test]
    #[ignore = "requires optional 904 MiB local JL1 fixture"]
    async fn local_jl1_cog_center_tile_is_not_opaque_black() {
        use crate::io::CogReader;
        use crate::style::TileStyle;
        use crate::tms::{TileCoord, TileMatrixSet};
        use async_tiff::reader::ObjectReader;
        use object_store::local::LocalFileSystem;
        use object_store::path::Path as OsPath;
        use std::sync::Arc;

        let tif = std::env::current_dir()
            .unwrap()
            .join("data/JL1GF04A_PMS_20230301124138_3857_cog.tif");

        let os_path = OsPath::from_filesystem_path(&tif).unwrap();
        let reader = CogReader::builder()
            .snapshot_policy(crate::io::SnapshotPolicy::AssumeImmutable)
            .open(ObjectReader::new(Arc::new(LocalFileSystem::new()), os_path))
            .await
            .unwrap();
        let tiler = Tiler::new(Arc::new(reader));
        let style = TileStyle::from_raster_query(
            "version=2.0&type=gray&bidx=1&resampling=bilinear&reproject=bilinear&tilesize=256",
        )
        .unwrap();
        let png = tiler
            .tile(
                TileCoord {
                    tms: TileMatrixSet::WebMercatorQuad,
                    z: 14,
                    x: 12866,
                    y: 7017,
                },
                &style,
            )
            .await
            .unwrap()
            .expect("JL1 center tile must intersect the COG");
        let rgba = image::load_from_memory(&png).unwrap().to_rgba8();
        assert!(
            rgba.pixels()
                .any(|pixel| pixel[3] > 0 && (pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0)),
            "JL1 center tile must retain source imagery instead of becoming opaque black"
        );
    }

    #[test]
    fn tiler_config_clamps_blocking_limit_to_one() {
        let cfg = TilerConfig {
            max_blocking_tasks: 0,
            ..Default::default()
        };
        assert_eq!(cfg.blocking_task_limit(), 1);
    }

    #[test]
    fn passthrough_hits_on_exact_aligned_tile() {
        // GT: origin (0, 256), 1 m/px, north-up. A 256×256 tile covering
        // world [0,0]..[256,256] maps exactly to pixel window (0,0)+256.
        let gt = GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 256.0, 0.0, -1.0]);
        let bbox = Bbox::new(0.0, 0.0, 256.0, 256.0);
        assert_eq!(Tiler::passthrough_window(&gt, &bbox, 256), Some((0, 0)));

        // Offset tile at world (512, 0)..(768, 256) → pixel col 512.
        let bbox2 = Bbox::new(512.0, 0.0, 768.0, 256.0);
        assert_eq!(Tiler::passthrough_window(&gt, &bbox2, 256), Some((512, 0)));
    }

    #[test]
    fn passthrough_rejects_subpixel_offset() {
        let gt = GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 256.0, 0.0, -1.0]);
        // Origin shifted half a pixel — must NOT passthrough (would
        // misregister against the basemap).
        let bbox = Bbox::new(0.5, 0.0, 256.5, 256.0);
        assert_eq!(Tiler::passthrough_window(&gt, &bbox, 256), None);
    }

    #[test]
    fn passthrough_rejects_scale_mismatch() {
        let gt = GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 256.0, 0.0, -1.0]);
        // World span 512 but ts=256 → 2 m/px request, not unit scale.
        let bbox = Bbox::new(0.0, -256.0, 512.0, 256.0);
        assert_eq!(Tiler::passthrough_window(&gt, &bbox, 256), None);
    }

    #[tokio::test]
    async fn cancelled_cpu_stage_keeps_all_permits_until_it_exits() {
        use crate::request::{RequestLimiter, RequestOptions};
        let reader = crate::io::CogReader::builder()
            .snapshot_policy(crate::io::SnapshotPolicy::AssumeImmutable)
            .open_object_store(
                std::sync::Arc::new(
                    object_store::local::LocalFileSystem::new_with_prefix(".").unwrap(),
                ),
                "tests/fixtures/rgb_nodata.tif",
            )
            .await
            .unwrap();
        let tiler = Tiler::with_config(
            std::sync::Arc::new(reader),
            TilerConfig {
                max_blocking_tasks: 1,
                ..Default::default()
            },
        );
        let cpu = tiler.cpu.clone();
        let admission = RequestLimiter::new(1);
        let task_admission = admission.clone();
        let budget = crate::resource::WorkBudget::new(100);
        let task_budget = budget.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            task_admission
                .run(
                    std::time::Duration::from_secs(30),
                    &RequestOptions::default(),
                    |context| async move {
                        let context = context.with_work(task_budget.try_reserve(70).unwrap());
                        tiler
                            .run_tile_cpu(&context, "held test stage", move || {
                                entered_tx.send(()).unwrap();
                                release_rx.recv().unwrap();
                                Ok(())
                            })
                            .await
                    },
                )
                .await
        });
        entered_rx.await.unwrap();
        task.abort();
        let _ = task.await;
        assert_eq!(cpu.available_permits(), 0);
        assert_eq!(admission.available_permits(), 0);
        assert_eq!(budget.used_bytes(), 70);
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while cpu.available_permits() != 1
                || admission.available_permits() != 1
                || budget.used_bytes() != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn tile_rejects_work_budget_before_reading_pixels() {
        let reader = crate::io::CogReader::builder()
            .snapshot_policy(crate::io::SnapshotPolicy::AssumeImmutable)
            .open_object_store(
                std::sync::Arc::new(
                    object_store::local::LocalFileSystem::new_with_prefix(".").unwrap(),
                ),
                "tests/fixtures/rgb_nodata.tif",
            )
            .await
            .unwrap();
        let budget = crate::resource::WorkBudget::new(1);
        let tiler = Tiler::new(std::sync::Arc::new(reader)).with_work_budget(budget.clone());
        let (x, y) = lonlat_to_tile(10.032, 20.032, 18);
        let coord = crate::tms::TileCoord {
            tms: crate::tms::TileMatrixSet::WebMercatorQuad,
            z: 18,
            x,
            y,
        };
        assert!(matches!(
            tiler.tile(coord, &crate::style::TileStyle::default()).await,
            Err(crate::error::AgError::ResourceLimit(_))
        ));
        assert_eq!(budget.used_bytes(), 0);
    }

    #[tokio::test]
    async fn lanczos_plan_reads_full_kernel_halo() {
        let reader = crate::io::CogReader::builder()
            .snapshot_policy(crate::io::SnapshotPolicy::AssumeImmutable)
            .open_object_store(
                std::sync::Arc::new(
                    object_store::local::LocalFileSystem::new_with_prefix(".").unwrap(),
                ),
                "tests/fixtures/rgb_nodata.tif",
            )
            .await
            .unwrap();
        let tiler = Tiler::new(std::sync::Arc::new(reader));
        let (x, y) = lonlat_to_tile(10.032, 20.032, 18);
        let coord = crate::tms::TileCoord {
            tms: crate::tms::TileMatrixSet::WebMercatorQuad,
            z: 18,
            x,
            y,
        };
        let nearest = tiler
            .plan_tile(coord, &crate::style::TileStyle::default())
            .unwrap()
            .unwrap();
        let lanczos = tiler
            .plan_tile(
                coord,
                &crate::style::TileStyle::from_raster_query(
                    "version=2.0&type=gray&reproject=lanczos&bidx=1",
                )
                .unwrap(),
            )
            .unwrap()
            .unwrap();
        assert!(lanczos.col <= nearest.col - 3 && lanczos.row <= nearest.row - 3);
        assert!(lanczos.w >= nearest.w + 6 && lanczos.h >= nearest.h + 6);
    }

    #[tokio::test]
    async fn request_nodata_is_applied_before_interpolation() {
        let store = std::sync::Arc::new(
            object_store::local::LocalFileSystem::new_with_prefix(".").unwrap(),
        );
        let reader = crate::io::CogReader::builder()
            .snapshot_policy(crate::io::SnapshotPolicy::AssumeImmutable)
            .open_object_store(store, "tests/fixtures/rgb_nodata.tif")
            .await
            .unwrap();
        let tiler = Tiler::new(std::sync::Arc::new(reader));
        let [w, s, e, n] = tiler.wgs84_bounds().unwrap();
        let (x, y) = lonlat_to_tile((w + e) / 2.0, (s + n) / 2.0, 12);
        let coord = crate::tms::TileCoord {
            tms: crate::tms::TileMatrixSet::WebMercatorQuad,
            z: 12,
            x,
            y,
        };
        let style = crate::style::TileStyle::from_raster_query(
            "version=2.0&type=gray&nodata=100&reproject=bilinear&bidx=1",
        )
        .unwrap();
        let plan = tiler.plan_tile(coord, &style).unwrap().unwrap();
        let mut source = (*tiler
            .reader
            .read_window(plan.level, plan.col, plan.row, plan.w, plan.h)
            .await
            .unwrap())
        .clone();
        source.nodata = Some(100.0);
        // Raster Style warps only referenced bands, in floating point.
        source.pixels = crate::pixel::PixelData::F64(
            source.pixels.band_to_f64(0).insert_axis(ndarray::Axis(0)),
        );
        let expected = crate::warp::warp(
            &source,
            &plan.dst_crs,
            plan.dst_bbox,
            256,
            256,
            style.reproject,
        )
        .unwrap();
        let actual = tiler.warp_tile(coord, &style).await.unwrap().unwrap();
        assert_eq!(actual.mask, expected.mask);
        for (a, b) in actual
            .pixels
            .to_f64()
            .iter()
            .zip(expected.pixels.to_f64().iter())
        {
            assert!(a == b || (a.is_nan() && b.is_nan()));
        }
    }
}
