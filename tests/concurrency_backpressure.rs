//! Public API regressions: small working budget, overlapping requests, identical output.
use async_geotiff::{
    io::{CogReader, ReaderConfig},
    resource::WorkBudget,
    style::TileStyle,
    tiler::Tiler,
    tms::{TileCoord, TileMatrixSet},
};
use object_store::{memory::InMemory, path::Path, ObjectStoreExt};
use std::sync::Arc;

async fn fixture_store() -> Arc<InMemory> {
    let store = Arc::new(InMemory::new());
    for name in ["a", "b"] {
        store
            .put(
                &Path::from(name),
                bytes::Bytes::from_static(include_bytes!("fixtures/rgb_nodata.tif")).into(),
            )
            .await
            .unwrap();
    }
    store
}
fn coord() -> TileCoord {
    TileCoord {
        tms: TileMatrixSet::WorldCrs84Quad,
        z: 10,
        x: 1081,
        y: 398,
    }
}
async fn tiler(budget: &WorkBudget) -> Tiler {
    let mut config = ReaderConfig::default();
    config.work_budget = budget.clone();
    let reader = CogReader::builder()
        .reader_config(config)
        .open_object_store(fixture_store().await, "a")
        .await
        .unwrap();
    Tiler::new(Arc::new(reader))
}

#[tokio::test]
async fn tile_bursts_succeed_with_bounded_memory_and_stable_pixels() {
    let budget = WorkBudget::new(24 * 1024 * 1024);
    let tiler = tiler(&budget).await;
    let style = TileStyle::default();
    let mut expected = None;
    for _ in 0..2 {
        let results =
            futures_util::future::join_all((0..16).map(|_| tiler.tile(coord(), &style))).await;
        for result in results {
            let bytes = result.unwrap().expect("fixture tile must exist");
            let image = image::load_from_memory(&bytes).unwrap().to_rgba8();
            assert!(image.pixels().any(|p| p[3] > 0));
            if let Some(expected) = &expected {
                assert_eq!(&bytes, expected);
            } else {
                expected = Some(bytes);
            }
        }
        assert!(budget.peak_bytes() <= budget.limit_bytes());
    }
    drop(tiler);
    assert_eq!(budget.used_bytes(), 0);
}

#[tokio::test]
async fn warp_bursts_share_the_same_backpressure_boundary() {
    let budget = WorkBudget::new(24 * 1024 * 1024);
    let tiler = tiler(&budget).await;
    let style = TileStyle::default();
    let results =
        futures_util::future::join_all((0..16).map(|_| tiler.warp_tile(coord(), &style))).await;
    let mut expected = None;
    for result in results {
        let image = result.unwrap().expect("fixture tile must exist");
        let pixels = image.pixels.to_f64();
        if let Some(expected) = &expected {
            assert_eq!(&pixels, expected);
        } else {
            expected = Some(pixels);
        }
    }
    assert!(budget.peak_bytes() <= budget.limit_bytes());
    drop(tiler);
    assert_eq!(budget.used_bytes(), 0);
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
                .tile(coord(), &mosaic_style(PixelSelection::Mean, &style))
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

#[cfg(feature = "mosaic")]
fn mosaic_style(selection: async_geotiff::mosaic::PixelSelection, style: &TileStyle) -> TileStyle {
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
