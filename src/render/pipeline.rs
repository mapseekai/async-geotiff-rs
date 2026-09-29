//! Render an immutable Raster Style plan after reading/warping its source.
use crate::{
    error::AgResult, pixel::ImageData, render::encode::TileFormat, style::raster::RasterPlan,
    warp::resampler::Resampling,
};
use bytes::Bytes;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CompiledTileStyle {
    pub(crate) raster: Arc<RasterPlan>,
    pub(crate) nodata: Option<f64>,
    pub(crate) format: TileFormat,
    pub(crate) resampling: Resampling,
}

impl CompiledTileStyle {
    pub fn resampling(&self) -> Resampling {
        self.resampling
    }
}

pub fn render(image: &ImageData, style: &CompiledTileStyle) -> AgResult<Bytes> {
    style
        .raster
        .render(image, style.nodata.or(image.nodata), style.format)
}
