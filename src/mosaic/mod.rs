//! Multi-source pixel-level mosaic. See
//! `docs/superpowers/specs/2026-05-18-mosaic-design.md`.

pub mod mosaicjson;
pub mod quadkey;
pub mod selection;
pub mod source;

pub use mosaicjson::MosaicJsonSource;
pub use selection::PixelSelection;
pub use source::{AssetRef, MosaicSource};
