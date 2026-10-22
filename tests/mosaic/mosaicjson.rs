//! Public integration tests for src/mosaic/mosaicjson.rs.
use async_geotiff::{
    mosaic::{MosaicJsonSource, MosaicSource},
    tms::{TileCoord, TileMatrixSet},
};

#[tokio::test]
async fn mosaicjson_preserves_asset_priority() {
    let source = MosaicJsonSource::from_str(r#"{"mosaicjson":"0.0.3","minzoom":1,"maxzoom":1,"quadkey_zoom":1,"tiles":{"0":["z_new.tif","a_old.tif","z_new.tif"]}}"#).unwrap();
    for z in [1, 0] {
        let assets = source
            .assets_for_tile(TileCoord {
                tms: TileMatrixSet::WebMercatorQuad,
                z,
                x: 0,
                y: 0,
            })
            .await
            .unwrap();
        assert_eq!(
            assets.iter().map(|a| a.url.as_str()).collect::<Vec<_>>(),
            vec!["z_new.tif", "a_old.tif"]
        );
    }
}
