//! Top-level orchestrator: HTTP request → tile bytes.

use std::sync::Arc;

use bytes::Bytes;

use crate::error::{AgError, AgResult};
use crate::io::overview::pick_overview;
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

        let target_gsd = tms.resolution(z)?;

        // Overview pick using the full-resolution GSD from the source transform.
        let full_gsd = self
            .reader
            .transform()
            .0
            .a
            .abs()
            .max(self.reader.transform().0.e.abs());
        let full_w = if full_gsd > 0.0 {
            (dst_bbox.width() / full_gsd) as u64
        } else {
            1
        };
        let level = pick_overview(&self.reader.tiff, full_w.max(1), full_gsd, target_gsd);

        // Read window in source CRS pixel coords.
        let (col, row, w, h) = self
            .reader
            .transform()
            .window_for_bbox(&dst_bbox)
            .ok_or_else(|| AgError::Warp("inverse transform failed".into()))?;
        let src: ImageData = self.reader.read_window(level, col, row, w, h).await?;

        // Warp if needed; runs on a blocking pool (PROJ is !Send + !Sync, decode is sync).
        let warped: ImageData = if src.crs == dst_crs {
            src
        } else {
            let resampling = spec.reproject;
            let ts = spec.tilesize as usize;
            tokio::task::spawn_blocking(move || {
                warp::warp(&src, &dst_crs, dst_bbox, ts, ts, resampling)
            })
            .await
            .map_err(|e| AgError::Warp(format!("join: {e}")))??
        };

        let compiled: CompiledRenderSpec = compile(spec)?;
        let png = tokio::task::spawn_blocking(move || render(&warped, &compiled))
            .await
            .map_err(|e| AgError::Warp(format!("join: {e}")))??;
        Ok(png)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tiler_type_compiles() {
        let _ = std::mem::size_of::<super::Tiler>();
    }
}
