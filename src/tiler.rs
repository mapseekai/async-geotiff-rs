//! Top-level orchestrator: HTTP request → tile bytes.

use std::sync::Arc;
#[cfg(feature = "perf-tracing")]
use std::time::Instant;

use bytes::Bytes;

use crate::blocking::{default_blocking_permits, CpuLimiter};
use crate::bounds::Bbox;
use crate::error::{AgError, AgResult};
use crate::io::stats::{load_dataset_stats, DatasetStats};
use crate::io::{CogReader, ScaleOffset};
#[cfg(feature = "perf-tracing")]
use crate::perf::duration_ms;
use crate::pixel::{ImageData, PixelData};
use crate::render::pipeline::{render, CompiledTileStyle};
use crate::style::params::compile_cached;
use crate::style::stretch::{fallback_rescale, resolve_rescale, Stretch};
use crate::style::TileStyle;
use crate::tms::{TileCoord, Tms};
use crate::warp;
use tokio::sync::OnceCell;

const PASSTHROUGH_PIXEL_TOLERANCE: f64 = 1e-3;

/// Bounded memo of stats-derived effective styles, keyed by
/// `"{style:?}|{dtype}"`. Only deterministic (dataset-stats based) results are
/// inserted — per-tile fallback/expression stretches never enter it.
type EffectiveMemo = moka::sync::Cache<String, Arc<CompiledTileStyle>>;

pub struct Tiler {
    reader: Arc<CogReader>,
    cpu: CpuLimiter,
    stats: OnceCell<Option<Arc<DatasetStats>>>,
    /// See [`EffectiveMemo`]. Shared into the fused CPU job per tile; reused
    /// across tiles so per-style work (rescale resolution, render LUT builds
    /// hanging off the compiled style) amortizes instead of repeating per tile.
    effective_memo: Arc<EffectiveMemo>,
}

/// Runtime limits for a [`Tiler`].
///
/// `max_blocking_tasks` caps concurrent warp/render jobs. The permit wait is
/// async, while the CPU-heavy work still runs on Tokio's blocking pool.
#[derive(Clone, Copy, Debug)]
pub struct TilerConfig {
    pub max_blocking_tasks: usize,
}

impl Default for TilerConfig {
    fn default() -> Self {
        Self {
            max_blocking_tasks: default_blocking_permits(),
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

/// Result of the fused warp+style+render CPU job in [`Tiler::tile`].
enum FusedOut {
    /// Encoded tile bytes — the common case.
    Bytes(Bytes),
    /// Tile fell outside the projection's valid domain during warp.
    NoTile,
    /// The effective style needs dataset stats that were not prefetched
    /// (the warp changed the pixel dtype, e.g. a boundary tile upcast to
    /// f64). Carries the warped image so the caller re-renders without
    /// repeating the warp.
    NeedStats(Arc<ImageData>),
}

/// Outcome of the synchronous effective-style resolution.
enum EffectiveOutcome {
    Ready(Arc<CompiledTileStyle>),
    /// Dataset stats are required but were not provided by the caller.
    NeedStats,
}

impl Tiler {
    /// Build a `Tiler` that shares the reader's [`CpuLimiter`], so decode,
    /// stitch, warp and render all draw from one CPU budget. In a mosaic, where
    /// every asset's reader shares one limiter (via `ReaderPool`), this bounds
    /// the whole mosaic's CPU work — preventing oversubscription. Use
    /// [`Self::with_config`] to give a single-source tiler its own bound.
    pub fn new(reader: Arc<CogReader>) -> Self {
        let cpu = reader.cpu_limiter();
        Self {
            reader,
            cpu,
            stats: OnceCell::new(),
            effective_memo: Arc::new(EffectiveMemo::new(32)),
        }
    }

    /// Build a `Tiler` with an independent CPU bound sized by
    /// `config.max_blocking_tasks` (does not share the reader's limiter). Prefer
    /// [`Self::new`] when you want one shared process/mosaic-wide budget.
    pub fn with_config(reader: Arc<CogReader>, config: TilerConfig) -> Self {
        Self {
            reader,
            cpu: CpuLimiter::new(config.blocking_task_limit()),
            stats: OnceCell::new(),
            effective_memo: Arc::new(EffectiveMemo::new(32)),
        }
    }

    /// WGS84 `[west, south, east, north]` extent of the dataset.
    pub fn wgs84_bounds(&self) -> AgResult<[f64; 4]> {
        self.reader.wgs84_bounds()
    }

    /// Lazily computed once per dataset, then memoized for the life of
    /// this `Tiler` (which `AppState.tilers` keeps alive across all tile
    /// requests). `?rescale=` requests never call this.
    async fn dataset_stats(&self) -> AgResult<Option<Arc<DatasetStats>>> {
        self.stats
            .get_or_try_init(|| async {
                Ok::<_, AgError>(load_dataset_stats(&self.reader).await?.map(Arc::new))
            })
            .await
            .cloned()
    }

    // `effective_compiled` was an async method here; it is now the free
    // function [`effective_compiled_sync`] so the fused warp+render CPU job
    // can resolve the style without leaving the blocking context. Dataset
    // stats are awaited by the caller and passed in.

    /// Pure planning: destination grid → overview level + clamped pixel window.
    /// Returns `None` when the tile is fully outside the COG or outside the
    /// projection's valid domain. Shared by [`Self::warp_tile`] (mosaic
    /// per-asset path) and the fused [`Self::tile`] path.
    fn plan_tile(&self, coord: TileCoord, style: &TileStyle) -> AgResult<Option<ReadyPlan>> {
        let tms = Tms::new(coord.tms)?;
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
        let level = self.reader.pick_overview(target_gsd);

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

        let raw = if aligned {
            (rc, rr, rw, rh)
        } else {
            (rc - 1, rr - 1, rw + 2, rh + 2)
        };
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
    /// (HTTP 204). This is the per-asset primitive the mosaic layer reuses.
    pub async fn warp_tile(
        &self,
        coord: TileCoord,
        style: &TileStyle,
    ) -> AgResult<Option<Arc<ImageData>>> {
        #[cfg(feature = "perf-tracing")]
        let started = Instant::now();
        let Some(plan) = self.plan_tile(coord, style)? else {
            return Ok(None);
        };
        #[cfg(feature = "perf-tracing")]
        let planned = started.elapsed();
        let src: Arc<ImageData> = self
            .reader
            .read_window(plan.level, plan.col, plan.row, plan.w, plan.h)
            .await?;
        #[cfg(feature = "perf-tracing")]
        let read_done = started.elapsed();
        if plan.passthrough {
            crate::perf_trace! {
                tracing::debug!(
                    level = plan.level,
                    col = plan.col,
                    row = plan.row,
                    width = plan.w,
                    height = plan.h,
                    plan_ms = duration_ms(planned),
                    read_window_ms = duration_ms(read_done - planned),
                    warp_cpu_ms = 0.0,
                    total_ms = duration_ms(read_done),
                    "warp_tile timing"
                );
            }
            return Ok(Some(src));
        }

        let ts = style.tilesize as usize;
        let resampling = if src.crs == plan.dst_crs {
            style.resampling
        } else {
            style.reproject
        };
        let dst_crs = plan.dst_crs.clone();
        let dst_bbox = plan.dst_bbox;
        let warped: ImageData = match self
            .run_tile_cpu("warp", move || {
                warp::warp(&src, &dst_crs, dst_bbox, ts, ts, resampling)
            })
            .await
        {
            Ok(w) => w,
            Err(AgError::ProjectionDomain(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        #[cfg(feature = "perf-tracing")]
        let warp_done = started.elapsed();
        crate::perf_trace! {
            tracing::debug!(
                level = plan.level,
                col = plan.col,
                row = plan.row,
                width = plan.w,
                height = plan.h,
                plan_ms = duration_ms(planned),
                read_window_ms = duration_ms(read_done - planned),
                warp_cpu_ms = duration_ms(warp_done - read_done),
                total_ms = duration_ms(warp_done),
                "warp_tile timing"
            );
        }
        Ok(Some(Arc::new(warped)))
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
        #[cfg(feature = "perf-tracing")]
        let started = Instant::now();
        let Some(plan) = self.plan_tile(coord, style)? else {
            crate::perf_trace! {
                tracing::debug!(
                    total_ms = duration_ms(started.elapsed()),
                    "tile internals timing: no tile"
                );
            }
            return Ok(None);
        };
        #[cfg(feature = "perf-tracing")]
        let planned = started.elapsed();
        let src: Arc<ImageData> = self
            .reader
            .read_window(plan.level, plan.col, plan.row, plan.w, plan.h)
            .await?;
        #[cfg(feature = "perf-tracing")]
        let read_done = started.elapsed();

        let compiled = compile_cached(style).await?;
        // Prefetch dataset stats (memoized after the first tile) whenever the
        // effective style may need them, so the fused CPU job below resolves
        // the style synchronously. The u8-no-stretch default never loads them
        // — exactly as before the fusion.
        let want_stats = style.rescale.is_empty()
            && style.expression.is_none()
            && (style.resolved_stretch()?.is_some()
                || style.unscale
                || !matches!(src.pixels, PixelData::U8(_)));
        let stats: Option<Option<Arc<DatasetStats>>> = if want_stats {
            Some(self.dataset_stats().await?)
        } else {
            None
        };

        // ONE blocking hop (one CPU permit) for warp + unscale + effective
        // style + render + encode. The intermediate tile buffer stays hot in
        // cache instead of crossing two spawn_blocking boundaries.
        let ts = style.tilesize as usize;
        let style_owned = style.clone();
        let compiled2 = Arc::clone(&compiled);
        let scale_offset = self.reader.band_scale_offset().to_vec();
        let memo = Arc::clone(&self.effective_memo);
        let fused = self
            .run_tile_cpu("tile", move || {
                let img: Arc<ImageData> = if plan.passthrough {
                    src
                } else {
                    let resampling = if src.crs == plan.dst_crs {
                        style_owned.resampling
                    } else {
                        style_owned.reproject
                    };
                    match warp::warp(&src, &plan.dst_crs, plan.dst_bbox, ts, ts, resampling) {
                        Ok(w) => Arc::new(w),
                        Err(AgError::ProjectionDomain(_)) => return Ok(FusedOut::NoTile),
                        Err(e) => return Err(e),
                    }
                };
                let img = if style_owned.unscale {
                    Arc::new(unscale_image(&img, &scale_offset))
                } else {
                    img
                };
                let effective = match effective_compiled_sync(
                    &style_owned,
                    &compiled2,
                    &img,
                    stats.as_ref(),
                    &scale_offset,
                    &memo,
                )? {
                    EffectiveOutcome::Ready(c) => c,
                    EffectiveOutcome::NeedStats => return Ok(FusedOut::NeedStats(img)),
                };
                Ok(FusedOut::Bytes(render(&img, &effective)?))
            })
            .await?;
        let bytes = match fused {
            FusedOut::Bytes(bytes) => bytes,
            FusedOut::NoTile => return Ok(None),
            FusedOut::NeedStats(img) => {
                // Rare: the warp changed the pixel dtype (boundary tile upcast
                // to f64) so stats weren't prefetched. Load them and re-render
                // — the warp is NOT repeated, only the style+render hop.
                let stats = self.dataset_stats().await?;
                let effective = match effective_compiled_sync(
                    style,
                    &compiled,
                    &img,
                    Some(&stats),
                    self.reader.band_scale_offset(),
                    &self.effective_memo,
                )? {
                    EffectiveOutcome::Ready(c) => c,
                    EffectiveOutcome::NeedStats => {
                        return Err(AgError::Warp("effective style demanded stats twice".into()))
                    }
                };
                self.run_tile_cpu("render", move || render(&img, &effective))
                    .await?
            }
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
    async fn run_tile_cpu<T, F>(&self, label: &'static str, f: F) -> AgResult<T>
    where
        T: Send + 'static,
        F: FnOnce() -> AgResult<T> + Send + 'static,
    {
        let permit = self
            .cpu
            .acquire()
            .await
            .map_err(|_| AgError::Warp(format!("{label} cpu limiter closed")))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            f()
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

    /// Reproject a bbox synchronously — 4 corner points through PROJ (or the
    /// 4326↔3857 direct formula). At a microsecond or two of work it's far
    /// cheaper to run inline than to pay tokio's spawn_blocking dispatch.
    /// Used only on the cross-CRS path; same-CRS callers don't reach here.
    fn reproject_bbox_sync(
        bbox: &crate::bounds::Bbox,
        from_crs: &crate::crs::Crs,
        src_crs: &crate::crs::Crs,
    ) -> AgResult<Bbox> {
        let mut pts = [
            (bbox.xmin, bbox.ymin),
            (bbox.xmax, bbox.ymin),
            (bbox.xmax, bbox.ymax),
            (bbox.xmin, bbox.ymax),
        ];
        crate::warp::projection::convert_batch(from_crs, src_crs, &mut pts)?;
        Ok(Bbox::from_corners(&pts))
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

/// Resolve the effective rescale and inject it into a cloned
/// `CompiledTileStyle`. Priority: explicit `rescale` (Custom) →
/// `?stretch=` mode → default StdDev k=2.0 (u8 keeps its fast path).
/// Sets BOTH `rescale` (RGB path / fast-path identity) and
/// `composite.vmin/vmax` (single-band Gradient path).
///
/// Pure CPU: dataset stats are passed in (`stats`), not awaited. `None` means
/// the caller did not prefetch them; if this style+image actually needs them
/// the function returns [`EffectiveOutcome::NeedStats`] and the caller falls
/// back to the unfused path. Stats-derived results (deterministic per
/// style+dtype) are memoized in `memo`; per-tile results (expression stretch,
/// pixel-fallback stretch) are computed fresh each call by design.
fn effective_compiled_sync(
    style: &TileStyle,
    compiled: &Arc<CompiledTileStyle>,
    image: &ImageData,
    stats: Option<&Option<Arc<DatasetStats>>>,
    scale_offset: &[ScaleOffset],
    memo: &EffectiveMemo,
) -> AgResult<EffectiveOutcome> {
    use EffectiveOutcome::{NeedStats, Ready};
    if !style.rescale.is_empty() {
        return Ok(Ready(Arc::clone(compiled)));
    }
    // Band-math expression: stretch from the synthetic index's own value
    // range, NOT dataset_stats (which are per-source-band raw DN and
    // meaningless for an index like NDVI). Explicit `rescale` already
    // returned above and wins.
    if style.expression.is_some() {
        // Match render's nodata source (`style.nodata.or(image.nodata)`) so
        // the stretch is computed over the same pixel set that renders.
        let nodata = compiled.nodata.or(image.nodata);
        let (lo, hi) =
            expression_rescale(image, &compiled.expression, nodata).unwrap_or((-1.0, 1.0));
        let mut next = (**compiled).clone();
        next.rescale = vec![(lo, hi)];
        next.composite.vmin = Some(vec![lo]);
        next.composite.vmax = Some(vec![hi]);
        next.auto_rgb = false;
        // Rescale changed → the clone must not share the parent's render LUTs.
        next.luts = Default::default();
        return Ok(Ready(Arc::new(next)));
    }
    let spec: Stretch = match style.resolved_stretch()? {
        Some(s) => s,
        None => {
            if matches!(image.pixels, PixelData::U8(_)) {
                return Ok(Ready(Arc::clone(compiled)));
            }
            Stretch::DEFAULT
        }
    };

    // Stats-derived results are deterministic per (style, pixel dtype): the
    // memo turns the per-tile clone + range resolution into a pointer fetch
    // and lets render-side LUTs hanging off the compiled style amortize.
    let memo_key = format!("{style:?}|{}", dtype_code(&image.pixels));
    if let Some(hit) = memo.get(&memo_key) {
        return Ok(Ready(hit));
    }

    let bands = image.pixels.bands();
    let bidx: Vec<usize> = if compiled.bidx.is_empty() {
        if bands >= 3 && compiled.composite.bands >= 3 {
            vec![0, 1, 2]
        } else {
            vec![0]
        }
    } else {
        compiled.bidx.clone()
    };

    let Some(stats) = stats else {
        return Ok(NeedStats);
    };
    let mut from_stats = false;
    let ranges = match stats {
        Some(stats) => {
            let r = resolve_rescale(&spec, stats, &bidx);
            // Empty → malformed stats (no bands); degrade gracefully.
            if r.is_empty() {
                fallback_rescale(&image.pixels, image.nodata, &bidx)
            } else {
                from_stats = true;
                r
            }
        }
        None => fallback_rescale(&image.pixels, image.nodata, &bidx),
    };

    let ranges = if style.unscale {
        unscale_ranges(&ranges, &bidx, scale_offset)
    } else {
        ranges
    };
    let mut next = (**compiled).clone();
    next.rescale = ranges.clone();
    next.composite.vmin = Some(ranges.iter().map(|r| r.0).collect());
    next.composite.vmax = Some(ranges.iter().map(|r| r.1).collect());
    next.auto_rgb = false;
    // Rescale changed → the clone must not share the parent's render LUTs.
    next.luts = Default::default();
    let next = Arc::new(next);
    if from_stats {
        // Only deterministic results enter the memo — the pixel-fallback
        // branch above depends on this tile's pixels and must stay per-tile.
        memo.insert(memo_key, Arc::clone(&next));
    }
    Ok(Ready(next))
}

/// Stable per-variant tag for memo keys.
fn dtype_code(p: &PixelData) -> &'static str {
    match p {
        PixelData::U8(_) => "u8",
        PixelData::U16(_) => "u16",
        PixelData::I16(_) => "i16",
        PixelData::U32(_) => "u32",
        PixelData::I32(_) => "i32",
        PixelData::F32(_) => "f32",
        PixelData::F64(_) => "f64",
    }
}

/// Finite-value (min, max) of the synthetic band an `expression` produces over
/// `image`. Returns `None` if no pixel yields a finite value (e.g. all-nodata
/// tile) — caller falls back to a default range. Nodata in a referenced band
/// or a non-finite result skips that pixel (same masking as render).
fn expression_rescale(
    image: &ImageData,
    rpn: &[crate::render::expression::Token],
    nodata: Option<f64>,
) -> Option<(f64, f64)> {
    use crate::render::expression::{eval_rpn_with, referenced_nodata_hit, Token};
    let src = image.pixels.to_f64();
    let bands = src.shape()[0];
    let (h, w) = (src.shape()[1], src.shape()[2]);
    let refs: Vec<usize> = {
        let mut v: Vec<usize> = rpn
            .iter()
            .filter_map(|t| match t {
                Token::Band(i) => Some(*i),
                _ => None,
            })
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let mut px = vec![0.0f64; bands];
    let mut stack: Vec<f64> = Vec::with_capacity(rpn.len().max(8));
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for r in 0..h {
        for c in 0..w {
            for b in 0..bands {
                px[b] = src[[b, r, c]];
            }
            if referenced_nodata_hit(&px, &refs, nodata) {
                continue;
            }
            let v = eval_rpn_with(rpn, &px, &mut stack);
            if v.is_finite() {
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
    }
    if lo.is_finite() && hi.is_finite() {
        Some((lo, hi))
    } else {
        None
    }
}

/// Apply per-band `physical = raw * scale + offset`, returning a new F64
/// `ImageData`. Bands beyond `scale_offset.len()` use identity.
fn unscale_image(img: &ImageData, scale_offset: &[ScaleOffset]) -> ImageData {
    use ndarray::Axis;
    let mut data = img.pixels.to_f64();
    for (b, mut band) in data.axis_iter_mut(Axis(0)).enumerate() {
        let so = scale_offset
            .get(b)
            .copied()
            .unwrap_or(ScaleOffset::IDENTITY);
        if !so.is_identity() {
            band.mapv_inplace(|v| so.apply(v));
        }
    }
    // The nodata sentinel is compared by VALUE in render's compute_valid_mask.
    // Pixels are unscaled, so the sentinel must move to physical space too, or
    // interior nodata pixels (for COGs without a mask IFD) render opaque.
    // GDAL stores one nodata value for all bands → use band 0's affine.
    let nodata_physical = img.nodata.map(|nd| {
        scale_offset
            .first()
            .copied()
            .unwrap_or(ScaleOffset::IDENTITY)
            .apply(nd)
    });
    ImageData {
        pixels: PixelData::F64(data),
        mask: img.mask.clone(),
        crs: img.crs.clone(),
        transform: img.transform,
        nodata: nodata_physical,
        bbox: img.bbox,
    }
}

/// Apply per-source-band scale/offset to auto-stretch ranges so they match
/// unscaled pixels. `bidx[i]` is the source band index for `ranges[i]`.
fn unscale_ranges(
    ranges: &[(f64, f64)],
    bidx: &[usize],
    scale_offset: &[ScaleOffset],
) -> Vec<(f64, f64)> {
    ranges
        .iter()
        .enumerate()
        .map(|(i, &(lo, hi))| {
            let b = bidx.get(i).copied().unwrap_or(i);
            let so = scale_offset
                .get(b)
                .copied()
                .unwrap_or(ScaleOffset::IDENTITY);
            (so.apply(lo), so.apply(hi))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{expression_rescale, unscale_image, unscale_ranges, Tiler, TilerConfig};
    use crate::bounds::{Bbox, GeoTransform};
    use crate::io::ScaleOffset;

    fn so(scale: f64, offset: f64) -> ScaleOffset {
        ScaleOffset { scale, offset }
    }

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
            .join("data/cog_uint8_rgb_nodata.tif");
        if !tif.exists() {
            eprintln!("fixture missing, skipping");
            return;
        }
        let os_path = OsPath::from_filesystem_path(&tif).unwrap();
        let reader = CogReader::open_with_reader(ObjectReader::new(
            Arc::new(LocalFileSystem::new()),
            os_path,
        ))
        .await
        .unwrap();
        let tiler = Tiler::new(Arc::new(reader));
        let style = TileStyle::from_query("").unwrap();
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

    #[test]
    fn tiler_config_clamps_blocking_limit_to_one() {
        let cfg = TilerConfig {
            max_blocking_tasks: 0,
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

    #[test]
    fn unscale_image_applies_per_band_affine() {
        use crate::pixel::{ImageData, PixelData};
        use ndarray::array;
        let pixels = PixelData::F64(array![[[10.0, 20.0]], [[1.0, 2.0]]]); // 2 bands,1x2
        let img = ImageData {
            pixels,
            mask: None,
            crs: crate::crs::Crs::epsg(3857),
            transform: crate::bounds::GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 0.0, 0.0, -1.0]),
            nodata: Some(-9999.0),
            bbox: crate::bounds::Bbox::new(0.0, 0.0, 2.0, 1.0),
        };
        let out = unscale_image(&img, &[so(0.1, 1.0), so(2.0, 0.0)]);
        let f = out.pixels.to_f64();
        assert_eq!(f[[0, 0, 0]], 10.0 * 0.1 + 1.0);
        assert_eq!(f[[0, 0, 1]], 20.0 * 0.1 + 1.0);
        assert_eq!(f[[1, 0, 0]], 1.0 * 2.0);
        assert_eq!(out.nodata, Some(-9999.0 * 0.1 + 1.0)); // sentinel moved to physical space
    }

    #[test]
    fn unscale_moves_nodata_sentinel_with_pixels() {
        use crate::pixel::{ImageData, PixelData};
        use ndarray::array;
        // single band, 1x2: one valid pixel (50) and one nodata pixel (-9999).
        let img = ImageData {
            pixels: PixelData::F64(array![[[50.0, -9999.0]]]),
            mask: None,
            crs: crate::crs::Crs::epsg(3857),
            transform: crate::bounds::GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 0.0, 0.0, -1.0]),
            nodata: Some(-9999.0),
            bbox: crate::bounds::Bbox::new(0.0, 0.0, 2.0, 1.0),
        };
        let out = unscale_image(&img, &[so(0.1, 1.0)]);
        let f = out.pixels.to_f64();
        // the transformed nodata pixel must still equal the transformed sentinel,
        // so render's value-based nodata masking keeps working.
        assert_eq!(f[[0, 0, 1]], out.nodata.unwrap());
        // and a valid pixel is unaffected by the sentinel.
        assert_eq!(f[[0, 0, 0]], 50.0 * 0.1 + 1.0);
    }

    #[test]
    fn unscale_ranges_applies_source_band_affine() {
        let ranges = vec![(0.0, 1000.0), (10.0, 20.0)];
        let bidx = vec![0usize, 1];
        let out = unscale_ranges(&ranges, &bidx, &[so(0.01, 5.0), so(2.0, 0.0)]);
        assert_eq!(out[0], (0.0 * 0.01 + 5.0, 1000.0 * 0.01 + 5.0));
        assert_eq!(out[1], (10.0 * 2.0, 20.0 * 2.0));
    }

    #[test]
    fn expression_rescale_all_nonfinite_returns_none() {
        use crate::pixel::{ImageData, PixelData};
        use crate::render::expression::parse_expression;
        // every pixel 0/0 → NaN → no finite value → None.
        let pixels = PixelData::F64(
            ndarray::Array3::from_shape_vec((2, 1, 2), vec![0.0, 0.0, 0.0, 0.0]).unwrap(),
        );
        let img = ImageData {
            pixels,
            mask: None,
            crs: crate::crs::Crs::epsg(3857),
            transform: crate::bounds::GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 0.0, 0.0, -1.0]),
            nodata: None,
            bbox: crate::bounds::Bbox::new(0.0, 0.0, 2.0, 1.0),
        };
        let rpn = parse_expression("(b2-b1)/(b2+b1)").unwrap();
        assert!(expression_rescale(&img, &rpn, None).is_none());
    }

    #[test]
    fn expression_rescale_uses_synthetic_band_range_not_band_stats() {
        use crate::pixel::{ImageData, PixelData};
        use crate::render::expression::parse_expression;
        // 2 bands, 1x3. NDVI=(b2-b1)/(b2+b1):
        //   px0: b1=1000,b2=3000 → 2000/4000 = 0.5
        //   px1: b1=2000,b2=2000 → 0/4000 = 0.0
        //   px2: b1=0,b2=0       → 0/0 = NaN (skipped)
        let pixels = PixelData::F64(
            ndarray::Array3::from_shape_vec(
                (2, 1, 3),
                vec![1000.0, 2000.0, 0.0, 3000.0, 2000.0, 0.0],
            )
            .unwrap(),
        ); // (2,1,3): band0=b1=[1000,2000,0], band1=b2=[3000,2000,0]
        let img = ImageData {
            pixels,
            mask: None,
            crs: crate::crs::Crs::epsg(3857),
            transform: crate::bounds::GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 0.0, 0.0, -1.0]),
            nodata: None,
            bbox: crate::bounds::Bbox::new(0.0, 0.0, 3.0, 1.0),
        };
        let rpn = parse_expression("(b2-b1)/(b2+b1)").unwrap();
        let (lo, hi) = expression_rescale(&img, &rpn, None).expect("finite range");
        assert!((lo - 0.0).abs() < 1e-9, "lo={lo}");
        assert!((hi - 0.5).abs() < 1e-9, "hi={hi}");
        assert!(hi < 1.0, "range must be NDVI units, not DN");
    }

    #[test]
    fn u8_default_request_keeps_rescale_empty() {
        // effective_compiled's u8 short-circuit is reached when:
        //   rescale empty  &&  stretch_kind None  &&  PixelData::U8
        // We assert the decision predicate here (pure, no IO):
        use crate::style::TileStyle;
        let style = TileStyle::from_query("").unwrap();
        assert!(style.rescale.is_empty());
        assert!(style.resolved_stretch().unwrap().is_none());
        // The combination above + U8 → effective_compiled returns the
        // input Arc unchanged (verified by the full-suite run, which
        // exercises real u8 fixtures through render()).
    }
}
