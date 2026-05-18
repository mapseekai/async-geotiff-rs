//! Top-level orchestrator: HTTP request → tile bytes.

use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::Semaphore;

use crate::blocking::default_blocking_permits;
use crate::bounds::Bbox;
use crate::error::{AgError, AgResult};
use crate::io::stats::{load_dataset_stats, DatasetStats};
use crate::io::CogReader;
use crate::pixel::{ImageData, PixelData};
use crate::render::pipeline::{render, CompiledTileStyle};
use crate::style::params::compile_cached;
use crate::style::stretch::{fallback_rescale, resolve_rescale, Stretch};
use crate::style::TileStyle;
use crate::tms::{TileMatrixSet, Tms};
use crate::warp;
use tokio::sync::OnceCell;

const PASSTHROUGH_PIXEL_TOLERANCE: f64 = 1e-3;

pub struct Tiler {
    reader: Arc<CogReader>,
    tile_cpu_semaphore: Arc<Semaphore>,
    stats: OnceCell<Option<Arc<DatasetStats>>>,
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

impl Tiler {
    pub fn new(reader: Arc<CogReader>) -> Self {
        Self::with_config(reader, TilerConfig::default())
    }

    pub fn with_config(reader: Arc<CogReader>, config: TilerConfig) -> Self {
        Self {
            reader,
            tile_cpu_semaphore: Arc::new(Semaphore::new(config.blocking_task_limit())),
            stats: OnceCell::new(),
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

    /// Resolve the effective rescale and inject it into a cloned
    /// `CompiledTileStyle`. Priority: explicit `rescale` (Custom) →
    /// `?stretch=` mode → default StdDev k=2.0 (u8 keeps its fast path).
    /// Sets BOTH `rescale` (RGB path / fast-path identity) and
    /// `composite.vmin/vmax` (single-band Gradient path).
    async fn effective_compiled(
        &self,
        style: &TileStyle,
        compiled: &Arc<CompiledTileStyle>,
        image: &ImageData,
    ) -> AgResult<Arc<CompiledTileStyle>> {
        if !style.rescale.is_empty() {
            return Ok(Arc::clone(compiled));
        }
        let spec: Stretch = match style.resolved_stretch()? {
            Some(s) => s,
            None => {
                if matches!(image.pixels, PixelData::U8(_)) {
                    return Ok(Arc::clone(compiled));
                }
                Stretch::DEFAULT
            }
        };

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

        let ranges = match self.dataset_stats().await? {
            Some(stats) => {
                let r = resolve_rescale(&spec, &stats, &bidx);
                // Empty → malformed stats (no bands); degrade gracefully.
                if r.is_empty() {
                    fallback_rescale(&image.pixels, image.nodata, &bidx)
                } else {
                    r
                }
            }
            None => fallback_rescale(&image.pixels, image.nodata, &bidx),
        };

        let mut next = (**compiled).clone();
        next.rescale = ranges.clone();
        next.composite.vmin = Some(ranges.iter().map(|r| r.0).collect());
        next.composite.vmax = Some(ranges.iter().map(|r| r.1).collect());
        next.auto_rgb = false;
        Ok(Arc::new(next))
    }

    /// Read + warp the source COG onto the destination tile grid. Returns
    /// `None` when the tile is fully outside the COG or outside the
    /// projection's valid domain (caller renders a transparent tile).
    /// This is the per-asset primitive the mosaic layer reuses.
    pub async fn warp_tile(
        &self,
        tile_matrix_set: TileMatrixSet,
        z: u8,
        x: u32,
        y: u32,
        style: &TileStyle,
    ) -> AgResult<Option<ImageData>> {
        let tms = Tms::new(tile_matrix_set)?;
        let dst_bbox = tms.tile_bbox(z, x, y)?;
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
        if same_crs && res_matches {
            if let Some((pc, pr)) = Self::passthrough_window(&level_tr, &dst_bbox, ts) {
                if let Ok((iw, ih)) = self.reader.level_dimensions(level) {
                    let in_bounds = pc >= 0
                        && pr >= 0
                        && (pc as u64 + ts as u64) <= iw as u64
                        && (pr as u64 + ts as u64) <= ih as u64;
                    if in_bounds {
                        let src: Arc<ImageData> = self
                            .reader
                            .read_window(level, pc, pr, ts as u64, ts as u64)
                            .await?;
                        return Ok(Some((*src).clone()));
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
        let src: Arc<ImageData> = self.reader.read_window(level, col, row, w, h).await?;

        let resampling = if src.crs == dst_crs {
            style.resampling
        } else {
            style.reproject
        };
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
        Ok(Some(warped))
    }

    pub async fn tile(
        &self,
        tile_matrix_set: TileMatrixSet,
        z: u8,
        x: u32,
        y: u32,
        style: &TileStyle,
    ) -> AgResult<Bytes> {
        let warped = match self.warp_tile(tile_matrix_set, z, x, y, style).await? {
            Some(img) => img,
            None => return empty_tile(style.tilesize as usize),
        };
        let compiled = compile_cached(style).await?;
        let compiled = self.effective_compiled(style, &compiled, &warped).await?;
        let img = warped;
        self.run_tile_cpu("render", move || render(&img, &compiled))
            .await
    }

    async fn run_tile_cpu<T, F>(&self, label: &'static str, f: F) -> AgResult<T>
    where
        T: Send + 'static,
        F: FnOnce() -> AgResult<T> + Send + 'static,
    {
        let permit = Arc::clone(&self.tile_cpu_semaphore)
            .acquire_owned()
            .await
            .map_err(|_| AgError::Warp(format!("{label} semaphore closed")))?;
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

/// Fully-transparent tile PNG. The bytes are constant for a given size,
/// so encode once and memoize: out-of-bounds / out-of-domain tiles are
/// common (whole viewport at COG edges), and PNG deflate on the async
/// worker would otherwise stall the reactor under load.
pub(crate) fn empty_tile(size: usize) -> AgResult<Bytes> {
    use std::collections::HashMap;
    use std::sync::{OnceLock, RwLock};

    static CACHE: OnceLock<RwLock<HashMap<usize, Bytes>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| RwLock::new(HashMap::new()));

    if let Some(bytes) = cache
        .read()
        .map_err(|_| AgError::Tiff("empty tile cache read lock poisoned".into()))?
        .get(&size)
    {
        return Ok(bytes.clone());
    }
    let rgba = ndarray::Array3::<u8>::zeros((size, size, 4));
    let png = crate::render::encode::encode_png(&rgba)?;
    cache
        .write()
        .map_err(|_| AgError::Tiff("empty tile cache write lock poisoned".into()))?
        .insert(size, png.clone());
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::{Tiler, TilerConfig};
    use crate::bounds::{Bbox, GeoTransform};

    #[test]
    fn tiler_type_compiles() {
        let _ = std::mem::size_of::<Tiler>();
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
