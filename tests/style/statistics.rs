//! Public integration tests for src/style/statistics.rs through Tiler.
#[path = "../support/fixtures.rs"]
mod fixtures;

use async_geotiff::{
    io::CogReader,
    style::TileStyle,
    tiler::Tiler,
    tms::{TileCoord, TileMatrixSet},
};
use fixtures::{coord, world_fixture};
use object_store::{memory::InMemory, path::Path as ObjectPath, ObjectStoreExt};
use std::sync::Arc;

#[tokio::test]
async fn statistical_stretch_is_shared_across_dataset_tiles() {
    let store = Arc::new(InMemory::new());
    store
        .put(
            &ObjectPath::from("stats"),
            world_fixture(0., Some([10, 20, 30])).into(),
        )
        .await
        .unwrap();
    let reader = Arc::new(CogReader::open_object_store(store, "stats").await.unwrap());
    let tiler = Tiler::new(reader);
    for method in [
        "minmax",
        "percentile&percentiles=2,98",
        "stddev&stddev=2",
        "histogram_equalization",
    ] {
        let style = TileStyle::from_raster_query(&format!(
            "version=3.0&type=gray&bidx=2&method={method}&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%7D"
        ))
        .unwrap();
        for coord in [
            coord(),
            TileCoord {
                tms: TileMatrixSet::WorldCrs84Quad,
                z: 1,
                x: 0,
                y: 0,
            },
        ] {
            let bytes = tiler.tile(coord, &style).await.unwrap().unwrap();
            assert!(
                image::load_from_memory(&bytes)
                    .unwrap()
                    .to_rgba8()
                    .pixels()
                    .all(|p| p.0 == [128, 128, 128, 255]),
                "{method}"
            );
        }
    }
}

#[tokio::test]
async fn statistics_cache_isolates_private_readers() {
    use async_tiff::reader::ObjectReader;
    let style =
        TileStyle::from_raster_query("version=3.0&type=gray&bidx=2&method=minmax&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%7D")
            .unwrap();
    let mut outputs = Vec::new();
    for value in [20, 100] {
        let store = Arc::new(InMemory::new());
        store
            .put(
                &ObjectPath::from("audit-cache"),
                world_fixture(0., Some([10, value, 30])).into(),
            )
            .await
            .unwrap();
        // Explicit immutable opaque readers, independent default block caches.
        let reader = CogReader::builder()
            .source_id("local-dataset")
            .snapshot_policy(async_geotiff::io::SnapshotPolicy::AssumeImmutable)
            .open(ObjectReader::new(store, ObjectPath::from("audit-cache")))
            .await
            .unwrap();
        let bytes = Tiler::new(Arc::new(reader))
            .tile(coord(), &style)
            .await
            .unwrap()
            .unwrap();
        let rgba = image::load_from_memory(&bytes).unwrap().to_rgba8();
        outputs.push(rgba.get_pixel(32, 32).0);
    }
    assert_eq!(
        outputs,
        vec![[128, 128, 128, 255]; 2],
        "each constant dataset needs its own range"
    );
}

#[tokio::test]
async fn sample_size_integer_valued_float_is_not_ignored() {
    let store = Arc::new(InMemory::new());
    store
        .put(
            &ObjectPath::from("audit-float-sample"),
            world_fixture(0., None).into(),
        )
        .await
        .unwrap();
    let reader = Arc::new(
        CogReader::open_object_store(store, "audit-float-sample")
            .await
            .unwrap(),
    );
    let tiler = Tiler::new(reader);
    let mut outputs = Vec::new();
    for sample in ["1", "1.0", "q3"] {
        let query_input = sample == "q3";
        let sample = if query_input { "1" } else { sample };
        let style=TileStyle::from_json(&format!(r#"{{"version":"3.0","extensions":["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"],"renderer":{{"type":"gray","bidx":[2]}},"stretch":{{"method":"minmax"}},"statistics":{{"scope":"dataset","accuracy":"sample","sample_size":{sample}}},"raster:tile":{{"tilesize":64}}}}"#)).unwrap();
        let style = if query_input {
            let sdk =
                async_geotiff::style::RasterStyle::from_json(style.raster_style_json()).unwrap();
            TileStyle::from_raster_query(&raster_style::encode_query(&sdk).unwrap()).unwrap()
        } else {
            style
        };
        let bytes = tiler.tile(coord(), &style).await.unwrap().unwrap();
        let rgba = image::load_from_memory(&bytes).unwrap().to_rgba8();
        let colors: std::collections::HashSet<_> =
            rgba.pixels().filter(|p| p.0[3] > 0).map(|p| p.0).collect();
        assert!(
            colors.len() <= 3,
            "one sampled value makes a degenerate minmax range with only 0,128,255"
        );
        assert!(!colors.is_empty());
        assert!(colors.iter().all(|p| [0, 128, 255].contains(&p[0])));
        outputs.push(rgba.into_raw());
    }
    assert!(
        outputs.windows(2).all(|pair| pair[0] == pair[1]),
        "JSON Schema integer-valued 1.0 must have same sampling as 1"
    );
}
