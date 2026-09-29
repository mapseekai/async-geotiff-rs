//! async-geotiff: pure-Rust async COG → XYZ tile renderer.
//!
//! Style inputs use Raster Style 2.0 JSON/Q2; see [`style::TileStyle`].

mod blocking;
pub use blocking::CpuLimiter;
pub mod bounds;
pub mod crs;
pub mod error;
pub mod io;
#[cfg(feature = "mosaic")]
pub mod mosaic;
mod perf;
pub mod pixel;
pub mod render;
pub mod request;
pub mod resource;
pub mod style;
pub mod tiler;
pub mod tms;
pub mod warp;
