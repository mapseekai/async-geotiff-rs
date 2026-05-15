//! Top-level orchestrator: HTTP request → tile bytes.

use std::sync::Arc;

use bytes::Bytes;

use crate::bounds::Bbox;
use crate::error::{AgError, AgResult};
use crate::io::CogReader;
use crate::pixel::ImageData;
use crate::render::pipeline::{render, CompiledRenderSpec};
use crate::style::query::{compile, RenderSpec};
use crate::tms::{Tms, TmsKind};
use crate::warp;

pub struct Tiler {
    reader: Arc<CogReader>,
}

impl Tiler {
    pub fn new(reader: Arc<CogReader>) -> Self {
        Self { reader }
    }

    pub async fn tile(
        &self,
        tms_kind: TmsKind,
        z: u8,
        x: u32,
        y: u32,
        spec: &RenderSpec,
    ) -> AgResult<Bytes> {
        let tms = Tms::new(tms_kind)?;
        let dst_bbox = tms.tile_bbox(z, x, y)?;
        let dst_crs = tms.crs();

        tracing::debug!(
            ?dst_crs,
            src_crs = %self.reader.src_crs(),
            ?dst_bbox,
            "tile request"
        );

        // 1. Project dst_bbox into the source CRS so we know which pixel window to read.
        //    Bbox projection is 4 points — sub-microsecond when the cached
        //    Projector hits or when the (4326, 3857) direct-formula fast
        //    path triggers — so we run it synchronously on the async worker
        //    instead of paying ~10-50 us in spawn_blocking task scheduling.
        let bbox_in_src = if self.reader.src_crs() == &dst_crs {
            dst_bbox
        } else {
            Self::reproject_bbox_sync(&dst_bbox, &dst_crs, self.reader.src_crs())?
        };

        // 2. Pick overview in source CRS units. Each overview has its own transform.
        let target_gsd =
            bbox_in_src.width().abs().max(bbox_in_src.height().abs()) / f64::from(spec.tilesize);
        let level = self.reader.pick_overview(target_gsd);

        tracing::debug!(?bbox_in_src, target_gsd, level, "picked overview");

        // 3. Pixel window in source-pixel space.
        //    Pad by 1 px on every side so the bilinear resampler always has
        //    valid neighbours at tile edges — otherwise adjacent XYZ tiles
        //    show white seams where samples fall just outside the read buffer.
        //
        //    Skip the halo when the read is "aligned": same CRS AND the
        //    requested resolution matches this overview's native resolution
        //    (scale ≈ 1). In that case the dst→src map is an integer
        //    translation with unit scale — no fractional sampling, so the
        //    resampler never reaches outside the window and the halo is pure
        //    waste. Mirrors rio-tiler's `_requested_tile_aligned_with_
        //    internal_tile` optimisation (reader.py:453-459); we use the
        //    resolution-match condition because it is the actual cause of
        //    edge-resampling artifacts, not internal-tile-grid alignment.
        let (rc, rr, rw, rh) = self
            .reader
            .level_transform(level)?
            .window_for_bbox(&bbox_in_src)
            .ok_or_else(|| AgError::Warp("source GeoTransform not invertible".into()))?;
        let same_crs = self.reader.src_crs() == &dst_crs;
        let level_gsd = self.reader.level_gsd(level).unwrap_or(target_gsd);
        let res_matches = level_gsd > 0.0
            && ((target_gsd - level_gsd).abs() / level_gsd) < 0.01;
        let aligned = same_crs && res_matches;
        let raw = if aligned {
            tracing::debug!(target_gsd, level_gsd, "aligned read — skipping halo");
            (rc, rr, rw, rh)
        } else {
            (rc - 1, rr - 1, rw + 2, rh + 2)
        };
        let (col, row, w, h) = self.clamp_window(level, raw.0, raw.1, raw.2, raw.3)?;
        if w == 0 || h == 0 {
            let level_dims = self.reader.level_dimensions(level).ok();
            tracing::warn!(
                ?bbox_in_src, raw_window = ?raw, level, ?level_dims,
                "tile entirely outside COG; returning transparent"
            );
            return empty_tile(spec.tilesize as usize);
        }
        tracing::debug!(col, row, w, h, "read window (clamped)");
        let src: ImageData = self.reader.read_window(level, col, row, w, h).await?;

        // 4. Project the read window into the exact destination tile grid. This is
        // still needed for same-CRS reads because the requested COG window may not
        // be the same size or placement as the output XYZ tile.
        let resampling = if src.crs == dst_crs {
            spec.resampling
        } else {
            spec.reproject
        };
        let ts = spec.tilesize as usize;
        let warped: ImageData = tokio::task::spawn_blocking(move || {
            warp::warp(&src, &dst_crs, dst_bbox, ts, ts, resampling)
        })
        .await
        .map_err(|e| AgError::Warp(format!("join: {e}")))??;

        let compiled: CompiledRenderSpec = compile(spec)?;
        let png = tokio::task::spawn_blocking(move || render(&warped, &compiled))
            .await
            .map_err(|e| AgError::Warp(format!("join: {e}")))??;
        Ok(png)
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

fn empty_tile(size: usize) -> AgResult<Bytes> {
    use ndarray::Array3;
    let rgba = Array3::<u8>::zeros((size, size, 4));
    crate::render::encode::encode_png(&rgba)
}

#[cfg(test)]
mod tests {
    #[test]
    fn tiler_type_compiles() {
        let _ = std::mem::size_of::<super::Tiler>();
    }
}
