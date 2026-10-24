//! Immutable Raster Style input and render-plan access.
use super::{raster::RasterPlan, TileFormat};
use crate::error::AgResult;
use crate::render::pipeline::CompiledTileStyle;
use crate::warp::resampler::Resampling;
use std::sync::{Arc, OnceLock};

/// Supported square tile widths/heights in pixels.
pub const ALLOWED_TILE_SIZES: [u32; 5] = [64, 128, 256, 512, 1024];

/// Validated Raster Style 3.0. Change the JSON/Q3 document and parse again to edit.
///
/// ```compile_fail
/// let mut style = async_geotiff::style::TileStyle::default();
/// style.rescale = vec![(0.0, 100.0)];
/// ```
///
/// ```compile_fail
/// async_geotiff::style::TileStyle::from_query("stretch=stddev").unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct TileStyle {
    pub(crate) raster: Arc<RasterPlan>,
    pub(crate) nodata: Option<f64>,
    pub(crate) resampling: Resampling,
    pub(crate) reproject: Resampling,
    pub(crate) tilesize: u32,
    pub(crate) format: TileFormat,
}

impl Default for TileStyle {
    /// Gray, source band 1, stretch none (0–255), PNG, 256 pixels.
    fn default() -> Self {
        Self::from_json(r#"{"version":"3.0","renderer":{"type":"gray","bidx":[1]}}"#)
            .expect("built-in Raster Style is supported")
    }
}

impl TileStyle {
    /// Requested output encoding, PNG by default.
    pub fn format(&self) -> TileFormat {
        self.format
    }
    /// Square tile width/height in pixels, 256 by default.
    pub fn tilesize(&self) -> u32 {
        self.tilesize
    }
    /// Kernel for the read/resize stage; reprojection can use a separate kernel.
    pub fn resampling(&self) -> Resampling {
        self.resampling
    }
}

/// Share a validated style's immutable plan with a standalone renderer.
///
/// Compilation performs no image reads or dataset statistics. The tile pipeline
/// resolves dataset-dependent stretch settings when rendering through a Tiler.
pub fn compile(style: &TileStyle) -> AgResult<CompiledTileStyle> {
    Ok(CompiledTileStyle {
        raster: style.raster.clone(),
        nodata: style.nodata,
        resampling: style.resampling,
        format: style.format,
    })
}

/// Cache immutable plans by their normalized Raster Style document.
pub async fn compile_cached(style: &TileStyle) -> AgResult<Arc<CompiledTileStyle>> {
    static CACHE: OnceLock<moka::future::Cache<String, Arc<CompiledTileStyle>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| moka::future::Cache::new(256));
    let key = style.raster.key.clone();
    if let Some(hit) = cache.get(&key).await {
        return Ok(hit);
    }
    let compiled = Arc::new(compile(style)?);
    cache.insert(key, compiled.clone()).await;
    Ok(compiled)
}
