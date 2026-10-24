//! Render an immutable Raster Style plan after reading/warping its source.
use crate::{
    error::AgResult, pixel::ImageData, render::encode::TileFormat, style::raster::RasterPlan,
    warp::resampler::Resampling,
};
use bytes::Bytes;
use std::sync::Arc;

/// Immutable, cheaply cloneable render plan produced by [`crate::style::compile`].
#[derive(Debug, Clone)]
pub struct CompiledTileStyle {
    pub(crate) raster: Arc<RasterPlan>,
    pub(crate) nodata: Option<f64>,
    pub(crate) format: TileFormat,
    pub(crate) resampling: Resampling,
}

impl CompiledTileStyle {
    /// Resampling kernel requested for reading/resizing source pixels.
    pub fn resampling(&self) -> Resampling {
        self.resampling
    }
}

/// Apply a compiled style and encode pixels in its requested image format.
///
/// This function does not reproject the input. Image bands and mask dimensions
/// must match the style and pixel grid; incompatible inputs return an error.
/// Use [`crate::tiler::Tiler`] to read, reproject and render a tile together.
pub fn render(image: &ImageData, style: &CompiledTileStyle) -> AgResult<Bytes> {
    style
        .raster
        .render(image, style.nodata.or(image.nodata), style.format)
}
