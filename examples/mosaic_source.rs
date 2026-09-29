//! Demonstrates the `MosaicSource` extension point: a business-owned
//! URL+bbox list. This is intentionally NOT crate API — the crate ships
//! only standard sources (MosaicJSON/STAC); attaching your own asset
//! discovery is what the `MosaicSource` trait is for.
//!
//! Run: `cargo run --example mosaic_source --features mosaic`

use async_geotiff::bounds::Bbox;
use async_geotiff::error::AgResult;
use async_geotiff::mosaic::{AssetRef, MosaicSource, MosaicTiler};
use async_geotiff::style::TileStyle;
use async_geotiff::tms::{TileCoord, TileMatrixSet};

struct UrlList(Vec<AssetRef>);

#[async_trait::async_trait]
impl MosaicSource for UrlList {
    async fn assets_for_tile(&self, _coord: TileCoord) -> AgResult<Vec<AssetRef>> {
        // Real code would filter by tile bbox intersection here.
        Ok(self.0.clone())
    }
}

#[tokio::main]
async fn main() -> AgResult<()> {
    let source = UrlList(vec![AssetRef {
        url: "https://example.com/scene_a.tif".into(),
        source_id: "https://example.com/scene_a.tif".into(),
        bbox: Bbox::new(-10.0, -10.0, 10.0, 10.0),
    }]);
    let mosaic = MosaicTiler::new(source);
    let style = TileStyle::from_json(
        r#"{
        "version": "2.0",
        "renderer": {"type": "rgb", "bidx": [1, 2, 3]},
        "mosaic": {"pixel_selection": "first", "stage": "before_channels"}
    }"#,
    )?;
    match mosaic
        .tile(
            TileCoord {
                tms: TileMatrixSet::WebMercatorQuad,
                z: 4,
                x: 8,
                y: 8,
            },
            &style,
        )
        .await
    {
        Ok(Some(png)) => println!("rendered {} bytes", png.len()),
        Ok(None) => println!("no tile (outside all assets)"),
        Err(e) => println!("expected (demo URL is fake): {e}"),
    }
    Ok(())
}
