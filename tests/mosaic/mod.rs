//! Public integration tests for src/mosaic/mod.rs.
#[path = "../support/fixtures.rs"]
mod fixtures;

use async_geotiff::{
    bounds::Bbox,
    resource::WorkBudget,
    style::TileStyle,
    tms::{TileCoord, TileMatrixSet},
};
use fixtures::{coord, fixture_store, local_coord, world_fixture};
use object_store::{memory::InMemory, path::Path as ObjectPath, ObjectStoreExt};
use std::sync::Arc;

fn mosaic_style(selection: PixelSelection, style: &TileStyle) -> TileStyle {
    let mut document: serde_json::Value = serde_json::from_str(style.raster_style_json()).unwrap();
    document["raster:mosaic"] = serde_json::json!({"pixel_selection": format!("{selection:?}").to_lowercase(), "stage": "after_channels"});
    let declarations = document
        .as_object_mut()
        .unwrap()
        .entry("extensions")
        .or_insert_with(|| serde_json::json!([]));
    declarations
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!(raster_style::MOSAIC_SCHEMA_URL));
    TileStyle::from_json(&document.to_string()).unwrap()
}

use async_geotiff::mosaic::{AssetRef, Backend, MosaicSource, MosaicTiler, PixelSelection};
struct Fixed(Vec<AssetRef>);
#[async_trait::async_trait]
impl MosaicSource for Fixed {
    async fn statistics_assets(&self) -> async_geotiff::error::AgResult<Vec<AssetRef>> {
        Ok(self.0.clone())
    }
    async fn assets_for_tile(&self, _: TileCoord) -> async_geotiff::error::AgResult<Vec<AssetRef>> {
        Ok(self.0.clone())
    }
}
async fn mosaic_with_nodata(shift: f64, invalid_unselected: bool) -> MosaicTiler<Fixed> {
    let store = Arc::new(InMemory::new());
    let mut assets = Vec::new();
    for (name, offset, samples) in [
        (
            "a",
            if invalid_unselected { shift } else { 0.0 },
            if invalid_unselected {
                [0, 200, 30]
            } else {
                [10, 200, 30]
            },
        ),
        ("b", shift, [20, 100, 40]),
    ] {
        store
            .put(
                &ObjectPath::from(name),
                world_fixture(offset, Some(samples)).into(),
            )
            .await
            .unwrap();
        assets.push(AssetRef {
            url: name.into(),
            source_id: name.into(),
            bbox: Bbox::new(-180.0, -90.0, 0.0, 90.0),
        });
    }
    MosaicTiler::builder(Fixed(assets))
        .backend(Backend::Store(store))
        .build()
}

#[tokio::test]
async fn statistical_stretch_uses_complete_mosaic_after_both_stages() {
    let mosaic = mosaic_with_nodata(0.0, false).await;
    for stage in ["before_channels", "after_channels"] {
        for selection in ["first", "mean", "median"] {
            let style=TileStyle::from_raster_query(&format!("version=3.0&type=rgb&bidx=2&bidx=3&bidx=1&method=minmax&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%2C%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Fmosaic%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%2C%22raster%3Amosaic%22%3A%7B%22pixel_selection%22%3A%22{selection}%22%2C%22stage%22%3A%22{stage}%22%7D%7D")).unwrap();
            let bytes = mosaic.tile(coord(), &style).await.unwrap().unwrap();
            assert!(
                image::load_from_memory(&bytes)
                    .unwrap()
                    .to_rgba8()
                    .pixels()
                    .all(|p| p.0 == [128, 128, 128, 255]),
                "{stage} {selection}"
            );
        }
    }
}

#[tokio::test]
async fn raster_mosaic_real_tiffs_support_all_methods_and_both_stages() {
    let mosaic = mosaic_with_nodata(0.0, false).await;
    for stage in ["before_channels", "after_channels"] {
        for (method, expected) in [
            ("first", [200, 30, 10, 128]),
            (
                "highest",
                if stage == "before_channels" {
                    [100, 40, 20, 128]
                } else {
                    [200, 30, 10, 128]
                },
            ),
            (
                "lowest",
                if stage == "before_channels" {
                    [200, 30, 10, 128]
                } else {
                    [100, 40, 20, 128]
                },
            ),
            ("mean", [150, 35, 15, 128]),
            ("median", [150, 35, 15, 128]),
        ] {
            let style = TileStyle::from_raster_query(&format!("version=3.0&type=rgb&bidx=2&bidx=3&bidx=1&opacity=0.5&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%2C%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Fmosaic%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%2C%22raster%3Amosaic%22%3A%7B%22pixel_selection%22%3A%22{method}%22%2C%22stage%22%3A%22{stage}%22%7D%7D")).unwrap();
            let bytes = mosaic.tile(coord(), &style).await.unwrap().unwrap();
            let image = image::load_from_memory(&bytes).unwrap().to_rgba8();
            assert!(image.pixels().all(|p| p.0 == expected), "{stage} {method}");
        }
    }
    let style = TileStyle::from_raster_query(
        "version=3.0&type=gray&bidx=2&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%2C%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Fmosaic%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%2C%22raster%3Amosaic%22%3A%7B%22pixel_selection%22%3A%22mean%22%2C%22stage%22%3A%22before_channels%22%7D%7D",
    )
    .unwrap();
    let mosaic = mosaic_with_nodata(0.0, true).await;
    let bytes = mosaic.tile(coord(), &style).await.unwrap().unwrap();
    assert!(image::load_from_memory(&bytes)
        .unwrap()
        .to_rgba8()
        .pixels()
        .all(|p| p.0 == [150, 150, 150, 255]));
}

#[tokio::test]
async fn raster_mosaic_short_circuit_failures_limits_and_selection_conflicts() {
    let store = Arc::new(InMemory::new());
    store
        .put(
            &ObjectPath::from("full"),
            world_fixture(0.0, Some([10, 200, 30])).into(),
        )
        .await
        .unwrap();
    let assets: Vec<_> = ["full", "missing"]
        .into_iter()
        .map(|key| AssetRef {
            url: key.into(),
            source_id: key.into(),
            bbox: Bbox::new(-180., -90., 0., 90.),
        })
        .collect();
    let limiter = async_geotiff::request::RequestLimiter::new(1);
    let budget = async_geotiff::resource::WorkBudget::new(64 * 1024 * 1024);
    let mosaic = MosaicTiler::builder(Fixed(assets.clone()))
        .backend(Backend::Store(store.clone()))
        .request_limiter(limiter.clone())
        .work_budget(budget.clone())
        .build();
    for stage in ["before_channels", "after_channels"] {
        let style = TileStyle::from_raster_query(&format!("version=3.0&type=rgb&bidx=1&bidx=2&bidx=3&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%2C%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Fmosaic%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%2C%22raster%3Amosaic%22%3A%7B%22pixel_selection%22%3A%22first%22%2C%22stage%22%3A%22{stage}%22%7D%7D")).unwrap();
        let bytes = mosaic.tile(coord(), &style).await.unwrap().unwrap();
        assert!(image::load_from_memory(&bytes)
            .unwrap()
            .to_rgba8()
            .pixels()
            .all(|p| p.0 == [10, 200, 30, 255]));

        let required = TileStyle::from_raster_query(&format!(
            "version=3.0&type=gray&bidx=1&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%2C%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Fmosaic%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%2C%22raster%3Amosaic%22%3A%7B%22pixel_selection%22%3A%22mean%22%2C%22stage%22%3A%22{stage}%22%7D%7D"
        ))
        .unwrap();
        assert!(mosaic.tile(coord(), &required).await.is_err());
        // A failed sibling may leave a blocking job draining; its admission
        // stays held until completion, including on the next loop iteration.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while limiter.available_permits() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    assert_eq!(limiter.available_permits(), 1);
    assert!(budget.peak_bytes() <= budget.limit_bytes());
    drop(mosaic);
    assert_eq!(budget.used_bytes(), 0);
    let capped = MosaicTiler::builder(Fixed(assets))
        .backend(Backend::Store(store))
        .config(async_geotiff::mosaic::MosaicConfig {
            max_assets_per_tile: 1,
            ..Default::default()
        })
        .build();
    let missing = TileStyle::from_raster_query("version=3.0&type=gray&bidx=1").unwrap();
    let error = capped.tile(coord(), &missing).await.unwrap_err();
    assert!(
        matches!(&error, async_geotiff::error::AgError::Query { name, .. } if name == "/raster:mosaic")
    );
    assert!(error.to_string().contains("require"));
    let style = TileStyle::from_raster_query(
        "version=3.0&type=gray&bidx=1&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Fmosaic%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Amosaic%22%3A%7B%22pixel_selection%22%3A%22first%22%2C%22stage%22%3A%22before_channels%22%7D%7D",
    )
    .unwrap();
    assert!(capped
        .tile(coord(), &style)
        .await
        .unwrap_err()
        .to_string()
        .contains("exceeding limit"));
}

#[tokio::test]
async fn first_stops_before_unneeded_assets() {
    let store = Arc::new(InMemory::new());
    for (name, samples) in [("empty", [0, 0, 0]), ("full", [10, 200, 30])] {
        store
            .put(
                &ObjectPath::from(name),
                world_fixture(0.0, Some(samples)).into(),
            )
            .await
            .unwrap();
    }
    let style = TileStyle::from_raster_query(
        "version=3.0&type=rgb&bidx=1&bidx=2&bidx=3&rescale=0,255&method=linear&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%7D",
    )
    .unwrap();
    for keys in [vec!["full", "missing"], vec!["empty", "full", "missing"]] {
        let assets = keys
            .into_iter()
            .map(|key| AssetRef {
                url: key.into(),
                source_id: key.into(),
                bbox: Bbox::new(-180.0, -90.0, 0.0, 90.0),
            })
            .collect();
        let mosaic = MosaicTiler::builder(Fixed(assets))
            .backend(Backend::Store(store.clone()))
            .build();
        let bytes = mosaic
            .tile(coord(), &mosaic_style(PixelSelection::First, &style))
            .await
            .expect("covered First tile must not open missing lower-priority assets")
            .unwrap();
        let rgba = image::load_from_memory(&bytes).unwrap().to_rgba8();
        for pixel in rgba.pixels() {
            assert_eq!(pixel.0, [10, 200, 30, 255]);
        }
        // Other reducers still require every asset and must propagate its failure.
        assert!(mosaic
            .tile(coord(), &mosaic_style(PixelSelection::Mean, &style))
            .await
            .is_err());
    }
}

#[tokio::test]
async fn first_propagates_failure_when_coverage_is_incomplete() {
    let store = Arc::new(InMemory::new());
    store
        .put(
            &ObjectPath::from("partial"),
            world_fixture(90.0, Some([10, 200, 30])).into(),
        )
        .await
        .unwrap();
    let assets = ["partial", "missing"]
        .into_iter()
        .map(|key| AssetRef {
            url: key.into(),
            source_id: key.into(),
            bbox: Bbox::new(-180.0, -90.0, 180.0, 90.0),
        })
        .collect();
    let mosaic = MosaicTiler::builder(Fixed(assets))
        .backend(Backend::Store(store))
        .build();
    assert!(mosaic
        .tile(
            coord(),
            &mosaic_style(
                PixelSelection::First,
                &TileStyle::from_raster_query(
                    "version=3.0&type=gray&rescale=0,255&bidx=1&method=linear&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%7D"
                )
                .unwrap()
            )
        )
        .await
        .is_err());
}

#[tokio::test]
async fn extrema_compare_selected_bands() {
    let mosaic = mosaic_with_nodata(0.0, false).await;
    for (selection, expected_rgb, expected_grey) in [
        (
            PixelSelection::Highest,
            [200, 30, 10, 255],
            [200, 200, 200, 255],
        ),
        (
            PixelSelection::Lowest,
            [100, 40, 20, 255],
            [100, 100, 100, 255],
        ),
    ] {
        for (query, expected) in [
            (
                "version=3.0&type=rgb&bidx=2&bidx=3&bidx=1&rescale=0,255&rescale=0,255&rescale=0,255&method=linear&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%7D",
                expected_rgb,
            ),
            ("version=3.0&type=gray&bidx=2&rescale=0,255&method=linear&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%7D", expected_grey),
        ] {
            let output = mosaic
                .tile(coord(), &mosaic_style(selection, &TileStyle::from_raster_query(query).unwrap()))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                image::load_from_memory(&output)
                    .unwrap()
                    .to_rgba8()
                    .get_pixel(0, 0)
                    .0,
                expected,
                "{selection:?} {query}"
            );
        }
    }
}

#[tokio::test]
async fn selected_band_validity_ignores_other_source_bands() {
    for shift in [0.0, 0.01] {
        let mosaic = mosaic_with_nodata(shift, true).await;
        for query in [
            "version=3.0&type=gray&bidx=2&rescale=0,255&method=linear&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%7D",
            "version=3.0&type=gray&bidx=2&rescale=0,255&method=linear&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%7D",
        ] {
            let output = mosaic
                .tile(
                    coord(),
                    &mosaic_style(
                        PixelSelection::Highest,
                        &TileStyle::from_raster_query(query).unwrap(),
                    ),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                image::load_from_memory(&output)
                    .unwrap()
                    .to_rgba8()
                    .get_pixel(32, 32)
                    .0,
                [200, 200, 200, 255],
                "{shift} {query}"
            );
        }
    }
}

#[tokio::test]
async fn mosaic_accepts_nearly_aligned_assets() {
    let output = mosaic_with_nodata(1e-7, false)
        .await
        .tile(coord(), &mosaic_style(PixelSelection::Mean, &TileStyle::from_raster_query("version=3.0&type=rgb&bidx=1&bidx=2&bidx=3&rescale=0,255&rescale=0,255&rescale=0,255&method=linear&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%7D")
            .unwrap()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        image::load_from_memory(&output)
            .unwrap()
            .to_rgba8()
            .get_pixel(0, 0)
            .0,
        [15, 150, 35, 255]
    );
}

#[cfg(feature = "mosaic")]
#[tokio::test]
async fn statistical_mosaic_preserves_index_coverage() {
    use async_geotiff::mosaic::{Backend, MosaicJsonSource, MosaicTiler};
    let store = Arc::new(InMemory::new());
    store
        .put(
            &ObjectPath::from("audit-mosaic-coverage"),
            world_fixture(0., Some([10, 20, 30])).into(),
        )
        .await
        .unwrap();
    let source=MosaicJsonSource::from_str(r#"{"mosaicjson":"0.0.3","minzoom":1,"maxzoom":10,"quadkey_zoom":1,"bounds":[-180,-90,0,90],"tiles":{"0":["audit-mosaic-coverage"]}}"#).unwrap();
    let tiler = MosaicTiler::builder(source)
        .backend(Backend::Store(store))
        .build();
    // Tile 2 is absent from index, but falls inside the raw TIFF footprint.
    let coord = TileCoord {
        tms: TileMatrixSet::WebMercatorQuad,
        z: 1,
        x: 0,
        y: 1,
    };
    for method in ["none", "minmax"] {
        let style=TileStyle::from_raster_query(&format!("version=3.0&type=gray&bidx=2&method={method}&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%2C%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Fmosaic%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%2C%22raster%3Amosaic%22%3A%7B%22pixel_selection%22%3A%22first%22%2C%22stage%22%3A%22before_channels%22%7D%7D")).unwrap();
        let output = tiler.tile(coord, &style).await.unwrap();
        assert!(
            output.is_none(),
            "stretch must not create coverage absent from MosaicJSON index"
        );
    }
}

#[cfg(feature = "mosaic")]
#[tokio::test]
async fn statistical_mosaic_rejects_partitioned_collection() {
    use async_geotiff::mosaic::{Backend, MosaicJsonSource, MosaicTiler};
    let store = Arc::new(InMemory::new());
    store
        .put(
            &ObjectPath::from("constant"),
            world_fixture(0., Some([10, 20, 30])).into(),
        )
        .await
        .unwrap();
    let style = TileStyle::from_raster_query("version=3.0&type=gray&bidx=2&method=minmax&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%2C%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Fmosaic%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%2C%22raster%3Amosaic%22%3A%7B%22pixel_selection%22%3A%22first%22%2C%22stage%22%3A%22before_channels%22%7D%7D").unwrap();
    let coord = TileCoord {
        tms: TileMatrixSet::WebMercatorQuad,
        z: 1,
        x: 0,
        y: 0,
    };
    for (zoom, key) in [(0, ""), (1, "0")] {
        let document = serde_json::json!({"mosaicjson":"0.0.3","minzoom":0,"maxzoom":10,"quadkey_zoom":zoom,"bounds":[-180,-90,0,90],"tiles":{key:["constant"]}});
        let source = MosaicJsonSource::from_str(&document.to_string()).unwrap();
        let tiler = MosaicTiler::builder(source)
            .backend(Backend::Store(store.clone()))
            .build();
        let result = tiler.tile(coord, &style).await;
        if zoom == 0 {
            let rgba = image::load_from_memory(&result.unwrap().unwrap())
                .unwrap()
                .to_rgba8();
            assert_eq!(rgba.get_pixel(32, 32).0, [128, 128, 128, 255]);
        } else {
            assert!(result.unwrap_err().to_string().contains("quadkey_zoom=0"));
        }
    }
}

#[cfg(feature = "mosaic")]
#[tokio::test]
async fn multi_asset_mosaic_retries_whole_request_not_nested_stages() {
    use async_geotiff::{
        bounds::Bbox,
        error::AgResult,
        mosaic::{AssetRef, Backend, MosaicConfig, MosaicSource, MosaicTiler, PixelSelection},
    };
    struct Sources;
    #[async_trait::async_trait]
    impl MosaicSource for Sources {
        async fn assets_for_tile(&self, _: TileCoord) -> AgResult<Vec<AssetRef>> {
            Ok(["a", "b"]
                .into_iter()
                .map(|name| AssetRef {
                    url: name.into(),
                    source_id: name.into(),
                    bbox: Bbox::new(-180., -90., 180., 90.),
                })
                .collect())
        }
    }
    let budget = WorkBudget::new(64 * 1024 * 1024);
    let mosaic = MosaicTiler::builder(Sources)
        .backend(Backend::Store(fixture_store().await))
        .work_budget(budget.clone())
        .config(MosaicConfig {
            chunk_size: 1,
            ..MosaicConfig::default()
        })
        .build();
    let style = TileStyle::default();
    let mut expected = None;
    for _ in 0..2 {
        let results = futures_util::future::join_all((0..8).map(|_| async {
            mosaic
                .tile(local_coord(), &mosaic_style(PixelSelection::Mean, &style))
                .await
        }))
        .await;
        for result in results {
            let bytes = result.unwrap().expect("mosaic must exist");
            if let Some(expected) = &expected {
                assert_eq!(&bytes, expected);
            } else {
                expected = Some(bytes);
            }
        }
    }
    assert!(budget.peak_bytes() <= budget.limit_bytes());
    drop(mosaic);
    assert_eq!(budget.used_bytes(), 0);
}
