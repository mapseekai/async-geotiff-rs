//! Loads a MosaicJSON document and renders one mosaicked tile.
//!
//! `MosaicJsonSource` is the crate's built-in read-only consumer of a
//! pre-built cogeo-mosaic MosaicJSON (a `quadkey -> [cog_url]` index).
//! "Loading a mosaic" is just: get the JSON text (file, HTTP, embedded),
//! hand it to `MosaicJsonSource::from_str`, wrap it in a `MosaicTiler`.
//!
//! Run with the bundled sample:
//!   cargo run --example mosaic_json --features mosaic
//! Or load your own document:
//!   cargo run --example mosaic_json --features mosaic -- path/to/mosaic.json

use async_geotiff::error::AgResult;
use async_geotiff::mosaic::{MosaicJsonSource, MosaicTiler, PixelSelection};
use async_geotiff::style::TileStyle;
use async_geotiff::tms::{TileCoord, TileMatrixSet};

/// A minimal valid MosaicJSON. `quadkey_zoom` is 3, so the keys are
/// length-3 quadkeys; each maps to the COG URLs covering that quadkey.
/// The URLs here are placeholders — opening them fails, so the mosaic
/// skips them and returns a transparent tile (still an `Ok`).
const SAMPLE: &str = r#"{
  "mosaicjson": "0.0.3",
  "minzoom": 3,
  "maxzoom": 12,
  "quadkey_zoom": 3,
  "bounds": [-180.0, -85.05, 180.0, 85.05],
  "tiles": {
    "021": ["https://example.com/scene_a.tif", "https://example.com/scene_b.tif"],
    "022": ["https://example.com/scene_c.tif"]
  }
}"#;

/// Decode a quadkey string back to its `(z, x, y)` tile coordinates so we
/// can request a tile we know the sample indexes.
fn quadkey_to_tile(qk: &str) -> (u8, u32, u32) {
    let (mut x, mut y) = (0u32, 0u32);
    for ch in qk.chars() {
        x <<= 1;
        y <<= 1;
        match ch {
            '0' => {}
            '1' => x |= 1,
            '2' => y |= 1,
            '3' => {
                x |= 1;
                y |= 1;
            }
            _ => panic!("invalid quadkey digit {ch:?}"),
        }
    }
    (qk.len() as u8, x, y)
}

#[tokio::main]
async fn main() -> AgResult<()> {
    // Loading step: from a file argument if given, else the embedded sample.
    let json = match std::env::args().nth(1) {
        Some(path) => {
            println!("loading MosaicJSON from {path}");
            std::fs::read_to_string(&path)
                .map_err(|e| async_geotiff::error::AgError::Mosaic(format!("read {path}: {e}")))?
        }
        None => {
            println!("loading the embedded sample MosaicJSON");
            SAMPLE.to_string()
        }
    };

    let source = MosaicJsonSource::from_str(&json)?;
    let b = source.bounds();
    println!(
        "mosaic loaded — bounds [w={}, s={}, e={}, n={}]",
        b.xmin, b.ymin, b.xmax, b.ymax
    );

    let mosaic = MosaicTiler::new(source);
    let style = TileStyle::from_query("")?;

    // Request a tile that the sample's quadkey index covers ("021").
    let (z, x, y) = quadkey_to_tile("021");
    println!("requesting tile z={z} x={x} y={y} (quadkey 021)");

    match mosaic
        .tile(
            TileCoord {
                tms: TileMatrixSet::WebMercatorQuad,
                z,
                x,
                y,
            },
            PixelSelection::First,
            &style,
        )
        .await
    {
        Ok(Some(png)) => println!(
            "rendered {} bytes (placeholder URLs are skipped → transparent tile)",
            png.len()
        ),
        Ok(None) => println!("no tile (outside all assets)"),
        Err(e) => println!("error: {e}"),
    }
    Ok(())
}
