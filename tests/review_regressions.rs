//! Regressions for MAP-209 through MAP-215; all fixtures are deterministic.
use async_geotiff::{
    bounds::{Bbox, GeoTransform},
    crs::Crs,
    io::CogReader,
    pixel::{ImageData, PixelData},
    render::pipeline::render,
    style::{params::compile, TileStyle},
    tiler::Tiler,
    tms::{TileCoord, TileMatrixSet},
};
use ndarray::{s, Array2, Array3};
use object_store::{memory::InMemory, path::Path as ObjectPath, ObjectStoreExt};
use std::sync::Arc;

fn image(pixels: PixelData, mask: Option<Array2<bool>>) -> ImageData {
    let (h, w) = (pixels.height(), pixels.width());
    ImageData {
        pixels,
        mask,
        bbox: Bbox::new(0.0, 0.0, w as f64, h as f64),
        crs: Crs::epsg(4326),
        transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, h as f64, 0.0, -1.0]),
        nodata: None,
    }
}

fn alphas(image: &ImageData, query: &str) -> Vec<u8> {
    let style = compile(&TileStyle::from_query(query).unwrap()).unwrap();
    let bytes = render(image, &style).unwrap();
    image::load_from_memory(&bytes)
        .unwrap()
        .to_rgba8()
        .pixels()
        .map(|p| p[3])
        .collect()
}

#[test]
fn fractional_nodata_does_not_mask_integer_rgb() {
    let img = image(
        PixelData::U8(Array3::from_shape_vec((3, 1, 1), vec![1, 100, 200]).unwrap()),
        None,
    );
    for query in [
        "nodata=1.5",
        "nodata=1.5&bidx=1&bidx=2&bidx=3&color_formula=gamma%20RGB%201",
    ] {
        assert_eq!(alphas(&img, query), vec![255], "{query}");
    }
    assert_eq!(alphas(&img, "nodata=1"), vec![0]);
}

#[test]
fn rgb_render_honors_strided_masks() {
    let base = Array2::from_shape_vec((2, 2), vec![true, false, true, true]).unwrap();
    for (mask, expected) in [
        (base.clone().reversed_axes(), vec![255, 255, 0, 255]),
        (base.slice_move(s![.., ..;-1]), vec![0, 255, 255, 255]),
    ] {
        assert!(!mask.is_standard_layout());
        for pixels in [
            PixelData::U8(Array3::from_elem((3, 2, 2), 100)),
            PixelData::U16(Array3::from_elem((3, 2, 2), 100)),
        ] {
            assert_eq!(alphas(&image(pixels, Some(mask.clone())), ""), expected);
        }
    }
}

#[test]
fn generic_render_masks_infinite_nodata() {
    for (sentinel, query) in [
        (f32::INFINITY, "nodata=inf"),
        (f32::NEG_INFINITY, "nodata=-inf"),
        (f32::NAN, "nodata=nan"),
    ] {
        let img = image(
            PixelData::F32(Array3::from_shape_vec((1, 1, 2), vec![sentinel, 10.0]).unwrap()),
            None,
        );
        assert_eq!(alphas(&img, query), vec![0, 255], "{query}");
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

// Adapt the checked-in tiny RGB TIFF to exactly cover WorldCRS84Quad 0/0/0.
// Optional constant samples make expected reducer outputs independent of render helpers.
fn world_fixture(shift: f64, samples: Option<[u8; 3]>) -> Vec<u8> {
    let mut bytes = include_bytes!("fixtures/rgb_nodata.tif").to_vec();
    let mut ifd = 8;
    while ifd != 0 {
        let entries = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
        let mut size = 0;
        let mut offsets = Vec::new();
        let mut lengths = Vec::new();
        for index in 0..entries {
            let entry = ifd + 2 + 12 * index;
            let tag = u16::from_le_bytes(bytes[entry..entry + 2].try_into().unwrap());
            let count = u32_at(&bytes, entry + 4) as usize;
            let value = u32_at(&bytes, entry + 8) as usize;
            match tag {
                256 => size = value,
                324 | 325 => {
                    let location = if count == 1 { entry + 8 } else { value };
                    let values = (0..count)
                        .map(|i| u32_at(&bytes, location + 4 * i) as usize)
                        .collect();
                    if tag == 324 {
                        offsets = values;
                    } else {
                        lengths = values;
                    }
                }
                33550 => {
                    for (i, v) in [180.0 / size as f64, 180.0 / size as f64, 0.0]
                        .iter()
                        .enumerate()
                    {
                        bytes[value + 8 * i..value + 8 * (i + 1)].copy_from_slice(&v.to_le_bytes());
                    }
                }
                33922 => {
                    for (i, v) in [0.0, 0.0, 0.0, -180.0 + shift, 90.0, 0.0]
                        .iter()
                        .enumerate()
                    {
                        bytes[value + 8 * i..value + 8 * (i + 1)].copy_from_slice(&v.to_le_bytes());
                    }
                }
                _ => {}
            }
        }
        if let Some(samples) = samples {
            for (offset, len) in offsets.into_iter().zip(lengths) {
                for pixel in bytes[offset..offset + len].chunks_exact_mut(3) {
                    pixel.copy_from_slice(&samples);
                }
            }
        }
        ifd = u32_at(&bytes, ifd + 2 + 12 * entries) as usize;
    }
    bytes
}

fn coord() -> TileCoord {
    TileCoord {
        tms: TileMatrixSet::WorldCrs84Quad,
        z: 0,
        x: 0,
        y: 0,
    }
}

#[tokio::test]
async fn passthrough_uses_the_destination_grid() {
    let store = Arc::new(InMemory::new());
    store
        .put(&ObjectPath::from("a"), world_fixture(1e-7, None).into())
        .await
        .unwrap();
    let reader = Arc::new(CogReader::open_object_store(store, "a").await.unwrap());
    let image = Tiler::new(reader)
        .warp_tile(coord(), &TileStyle::from_query("tilesize=64").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(image.transform.forward(0.0, 0.0), (-180.0, 90.0));
    assert_eq!(image.bbox.xmin, -180.0);
    assert_eq!(image.bbox.ymax, 90.0);
    assert!(image.bbox.xmax.abs() < 1e-12);
    assert!((image.bbox.ymin + 90.0).abs() < 1e-12);
}

#[cfg(feature = "mosaic")]
mod mosaic {
    use super::*;
    use async_geotiff::mosaic::{
        AssetRef, Backend, MosaicJsonSource, MosaicSource, MosaicTiler, PixelSelection,
    };
    struct Fixed(Vec<AssetRef>);
    #[async_trait::async_trait]
    impl MosaicSource for Fixed {
        async fn assets_for_tile(
            &self,
            _: TileCoord,
        ) -> async_geotiff::error::AgResult<Vec<AssetRef>> {
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
        let style =
            TileStyle::from_query("tilesize=64&bidx=1&bidx=2&bidx=3&rescale=0,255").unwrap();
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
                .tile(coord(), PixelSelection::First, &style)
                .await
                .expect("covered First tile must not open missing lower-priority assets")
                .unwrap();
            let rgba = image::load_from_memory(&bytes).unwrap().to_rgba8();
            for pixel in rgba.pixels() {
                assert_eq!(pixel.0, [10, 200, 30, 255]);
            }
            // Other reducers still require every asset and must propagate its failure.
            assert!(mosaic
                .tile(coord(), PixelSelection::Mean, &style)
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
                PixelSelection::First,
                &TileStyle::from_query("tilesize=64&rescale=0,255").unwrap()
            )
            .await
            .is_err());
    }

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

    #[tokio::test]
    async fn extrema_compare_selected_bands() {
        let mosaic = mosaic_with_nodata(0.0, false).await;
        for (selection, expected_rgb, expected_grey) in [
            (
                PixelSelection::Highest,
                [200, 30, 10, 255],
                [67, 67, 67, 255],
            ),
            (
                PixelSelection::Lowest,
                [100, 40, 20, 255],
                [182, 182, 182, 255],
            ),
        ] {
            for (query, expected) in [
                (
                    "tilesize=64&bidx=2&bidx=3&bidx=1&rescale=0,255&rescale=0,255&rescale=0,255",
                    expected_rgb,
                ),
                ("tilesize=64&bidx=2&rescale=0,255", expected_grey),
            ] {
                let output = mosaic
                    .tile(coord(), selection, &TileStyle::from_query(query).unwrap())
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
                "tilesize=64&bidx=2&rescale=0,255",
                "tilesize=64&bidx=2&rescale=0,255&unscale=true",
            ] {
                let output = mosaic
                    .tile(
                        coord(),
                        PixelSelection::Highest,
                        &TileStyle::from_query(query).unwrap(),
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
                    [67, 67, 67, 255],
                    "{shift} {query}"
                );
            }
        }
    }

    #[tokio::test]
    async fn mosaic_accepts_nearly_aligned_assets() {
        let output = mosaic_with_nodata(1e-7, false)
            .await
            .tile(
                coord(),
                PixelSelection::Mean,
                &TileStyle::from_query(
                    "tilesize=64&bidx=1&bidx=2&bidx=3&rescale=0,255&rescale=0,255&rescale=0,255",
                )
                .unwrap(),
            )
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
}

#[test]
fn cancelled_decode_keeps_admission_until_job_exits() {
    use async_geotiff::{
        io::ReaderConfig,
        request::{RequestCancellation, RequestLimiter, RequestOptions},
        resource::WorkBudget,
        CpuLimiter,
    };
    use axum::{
        http::{HeaderMap, StatusCode},
        routing::get,
        Router,
    };
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let gate = Arc::new(AtomicBool::new(false));
            let received = Arc::new(tokio::sync::Notify::new());
            let respond = Arc::new(tokio::sync::Notify::new());
            let (g, rx, tx) = (gate.clone(), received.clone(), respond.clone());
            let fixture = Arc::new(world_fixture(0.0, None));
            let app = Router::new().route(
                "/a.tif",
                get(move |headers: HeaderMap| {
                    let (g, rx, tx, fixture) = (g.clone(), rx.clone(), tx.clone(), fixture.clone());
                    async move {
                        let (a, b) = headers["range"]
                            .to_str()
                            .unwrap()
                            .strip_prefix("bytes=")
                            .unwrap()
                            .split_once('-')
                            .unwrap();
                        let (a, b) = (
                            a.parse::<usize>().unwrap(),
                            b.parse::<usize>().unwrap().min(fixture.len() - 1),
                        );
                        if g.swap(false, Ordering::SeqCst) {
                            rx.notify_one();
                            tx.notified().await;
                        }
                        (
                            StatusCode::PARTIAL_CONTENT,
                            [
                                ("etag", "\"v1\"".to_owned()),
                                ("content-range", format!("bytes {a}-{b}/{}", fixture.len())),
                            ],
                            fixture[a..=b].to_vec(),
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let cpu = CpuLimiter::new(1);
            let admission = RequestLimiter::new(1);
            let budget = WorkBudget::new(64 * 1024 * 1024);
            let mut config = ReaderConfig::default();
            config.work_budget = budget.clone();
            config.cpu_limiter = Some(cpu.clone());
            let reader = Arc::new(
                CogReader::builder()
                    .reader_config(config)
                    .open_http(&format!("http://{addr}/a.tif"))
                    .await
                    .unwrap(),
            );
            let tiler = Tiler::new(reader).with_request_limiter(admission.clone());
            let cancellation = RequestCancellation::default();
            let options = RequestOptions {
                cancellation: cancellation.clone(),
                timeout: Some(Duration::from_secs(10)),
            };
            gate.store(true, Ordering::SeqCst);
            let task = tokio::spawn(async move {
                tiler
                    .tile_with_options(
                        coord(),
                        &TileStyle::from_query("tilesize=64").unwrap(),
                        &options,
                    )
                    .await
            });
            tokio::time::timeout(Duration::from_secs(5), received.notified())
                .await
                .unwrap();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(10));
            });
            started_rx.await.unwrap();
            respond.notify_one();
            tokio::time::timeout(Duration::from_secs(5), async {
                while cpu.available_permits() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            cancellation.cancel();
            let result = task.await.unwrap();
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            let admission_while_queued = admission.available_permits();
            let cpu_while_queued = cpu.available_permits();
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                while cpu.available_permits() != 1 || budget.used_bytes() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            server.abort();
            assert!(matches!(
                result,
                Err(async_geotiff::error::AgError::Cancelled)
            ));
            assert_eq!(
                cpu_while_queued, 0,
                "decode was queued behind the controlled worker"
            );
            assert_eq!(
                admission_while_queued, 0,
                "the queued decode must retain request admission"
            );
            assert_eq!(admission.available_permits(), 1);
        });
}

#[test]
fn composite_get_masks_infinite_nodata() {
    use async_geotiff::render::composite::{ColourDefinition, Composite, HandleGet};
    let gradient =
        Composite::from_definition(&ColourDefinition::Named("viridis".into()), None, None).unwrap();
    for nd in [f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(gradient.get(&[nd], Some(&[nd]))[3], 0);
    }
}

#[tokio::test]
async fn point_masks_infinite_nodata() {
    // Append F32 tile payloads, preserving all existing directory positions.
    let mut bytes = world_fixture(0.0, Some([0, 10, 20]));
    let mut ifd = 8;
    while ifd != 0 {
        let entries = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
        let mut offsets = (0, 0);
        let mut counts = 0;
        for index in 0..entries {
            let entry = ifd + 2 + 12 * index;
            let tag = u16::from_le_bytes(bytes[entry..entry + 2].try_into().unwrap());
            let count = u32_at(&bytes, entry + 4) as usize;
            let value = u32_at(&bytes, entry + 8) as usize;
            match tag {
                258 | 339 => {
                    let field: u16 = if tag == 258 { 32 } else { 3 };
                    for b in 0..count {
                        bytes[value + 2 * b..value + 2 * b + 2]
                            .copy_from_slice(&field.to_le_bytes());
                    }
                }
                324 => offsets = (if count == 1 { entry + 8 } else { value }, count),
                325 => counts = if count == 1 { entry + 8 } else { value },
                42113 => {
                    bytes[entry + 4..entry + 8].copy_from_slice(&4u32.to_le_bytes());
                    bytes[entry + 8..entry + 12].copy_from_slice(b"inf\0");
                }
                _ => {}
            }
        }
        for tile in 0..offsets.1 {
            let offset = u32_at(&bytes, offsets.0 + 4 * tile) as usize;
            let len = u32_at(&bytes, counts + 4 * tile) as usize;
            let data: Vec<u8> = bytes[offset..offset + len]
                .iter()
                .flat_map(|&v| if v == 0 { f32::INFINITY } else { f32::from(v) }.to_le_bytes())
                .collect();
            let new_offset = bytes.len() as u32;
            bytes[offsets.0 + 4 * tile..offsets.0 + 4 * tile + 4]
                .copy_from_slice(&new_offset.to_le_bytes());
            bytes[counts + 4 * tile..counts + 4 * tile + 4]
                .copy_from_slice(&(data.len() as u32).to_le_bytes());
            bytes.extend(data);
        }
        ifd = u32_at(&bytes, ifd + 2 + 12 * entries) as usize;
    }
    let store = Arc::new(InMemory::new());
    store
        .put(&ObjectPath::from("float"), bytes.into())
        .await
        .unwrap();
    let reader = CogReader::open_object_store(store, "float").await.unwrap();
    let value = reader
        .point(-179.0, 89.0, &Crs::epsg(4326), &[1])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(value.values, vec![f64::INFINITY]);
    assert!(!value.valid);
    assert!(
        reader
            .point(-179.0, 89.0, &Crs::epsg(4326), &[2])
            .await
            .unwrap()
            .unwrap()
            .valid
    );
}
