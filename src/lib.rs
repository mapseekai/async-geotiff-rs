//! async-geotiff: pure-Rust async COG → XYZ tile renderer.
//!
//! Style inputs use Raster Style 3.0 JSON/Q3; see [`style::TileStyle`].

#![warn(missing_docs)]

mod blocking;
pub use blocking::CpuLimiter;
/// Spatial envelopes and pixel-to-world affine transforms.
pub mod bounds;
/// Coordinate reference system identifiers.
pub mod crs;
/// Errors shared by readers, renderers and request controls.
pub mod error;
/// Asynchronous COG access, decoded block caching and source metadata.
pub mod io;
#[cfg(feature = "mosaic")]
pub mod mosaic;
mod perf;
/// Typed pixel arrays, validity masks and georeferenced images.
pub mod pixel;
/// Raster Style render plans, expressions, color maps and image encoding.
pub mod render;
pub mod request;
pub mod resource;
/// Validated Raster Style 3.0 JSON/Q3 documents and compilation.
pub mod style;
pub mod tiler;
/// Supported tile matrix sets and checked tile coordinates.
pub mod tms;
/// Coordinate reprojection and spatial resampling of raster images.
pub mod warp;
