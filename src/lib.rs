//! async-geotiff: pure-Rust async COG → XYZ tile renderer.
//!
//! See `docs/superpowers/specs/2026-05-14-async-geotiff-rs-design.md`.

mod blocking;
pub use blocking::CpuLimiter;
pub mod bounds;
pub mod crs;
pub mod error;
pub mod io;
#[cfg(feature = "mosaic")]
pub mod mosaic;
pub mod pixel;
pub mod render;
pub mod style;
pub mod tiler;
pub mod tms;
pub mod warp;
