//! Public integration tests for src/tiler.rs.
#[path = "support/fixtures.rs"]
mod fixtures;

use async_geotiff::{
    error::AgError,
    io::{CogReader, ReaderConfig},
    resource::WorkBudget,
    style::TileStyle,
    tiler::Tiler,
    tms::TileCoord,
};
use fixtures::{coord, fixture_store, local_coord, world_fixture};
use object_store::{memory::InMemory, path::Path as ObjectPath, ObjectStoreExt};
use std::sync::Arc;

#[tokio::test]
async fn mercator_source_preserves_actual_polar_coverage() {
    let radius = 6_378_137.0;
    let extent = std::f64::consts::PI * radius;
    let polar_extent = radius
        * (std::f64::consts::FRAC_PI_4 + 89.9_f64.to_radians() * 0.5)
            .tan()
            .ln();
    for (north, polar_alpha) in [(extent, 0), (polar_extent, 255)] {
        let store = Arc::new(InMemory::new());
        store
            .put(
                &ObjectPath::from("world"),
                fixtures::georeferenced_fixture(
                    3857,
                    [-extent, -north, extent, north],
                    Some([200; 3]),
                )
                .into(),
            )
            .await
            .unwrap();
        let reader = Arc::new(CogReader::open_object_store(store, "world").await.unwrap());
        for kernel in [
            "nearest",
            "bilinear",
            "cubic",
            "cubic_spline",
            "lanczos",
            "average",
        ] {
            let style = TileStyle::from_json(&format!(
                r#"{{"version":"3.0","renderer":{{"type":"gray","bidx":[1]}},"resampling":{{"read":"{kernel}","reproject":"{kernel}"}}}}"#
            )).unwrap();
            assert_eq!(
                style.resampling(),
                async_geotiff::warp::Resampling::parse(kernel).unwrap()
            );
            let png = Tiler::new(reader.clone())
                .tile(coord(), &style)
                .await
                .unwrap()
                .unwrap();
            let image = image::load_from_memory(&png).unwrap().into_rgba8();
            for row in [0, 255] {
                for col in 0..256 {
                    assert_eq!(
                        image.get_pixel(col, row).0[3],
                        polar_alpha,
                        "polar pixel {col},{row}, extent {north}, kernel {kernel}"
                    );
                }
            }
            for row in [7, 128, 248] {
                assert_eq!(
                    image.get_pixel(128, row).0,
                    [200, 200, 200, 255],
                    "covered row {row}, extent {north}, kernel {kernel}"
                );
            }
        }
    }
}

#[tokio::test]
async fn tiny_pixel_scale_returns_a_range_error_without_a_worker_panic() {
    let store = Arc::new(InMemory::new());
    store
        .put(
            &ObjectPath::from("tiny"),
            fixtures::georeferenced_fixture(4326, [0.0, -64e-18, 64e-18, 0.0], Some([200; 3]))
                .into(),
        )
        .await
        .unwrap();
    let reader = Arc::new(CogReader::open_object_store(store, "tiny").await.unwrap());
    let style = TileStyle::from_raster_query("version=3.0&type=gray&bidx=1").unwrap();
    let error = Tiler::new(reader).tile(coord(), &style).await.unwrap_err();
    assert!(matches!(error, AgError::Warp(ref message)
        if message.contains("source pixel window") && message.contains("out of range")));
}

#[tokio::test]
async fn invalid_tile_coordinates_are_rejected_by_public_tiler() {
    let reader = Arc::new(
        CogReader::open_object_store(fixture_store().await, "a")
            .await
            .unwrap(),
    );
    let tiler = Tiler::new(reader);
    let style = TileStyle::from_raster_query("version=3.0&type=gray&bidx=1").unwrap();
    for tms in [
        async_geotiff::tms::TileMatrixSet::WebMercatorQuad,
        async_geotiff::tms::TileMatrixSet::WorldCrs84Quad,
    ] {
        for (z, x, y) in [(0, 2, 0), (0, 0, 1), (33, 17, 19)] {
            let coord = TileCoord { tms, z, x, y };
            assert!(matches!(tiler.tile(coord, &style).await,
                Err(AgError::TileOutOfBounds { z: ez, x: ex, y: ey }) if (ez, ex, ey) == (z, x, y)));
            assert!(matches!(tiler.warp_tile(coord, &style).await,
                Err(AgError::TileOutOfBounds { z: ez, x: ex, y: ey }) if (ez, ex, ey) == (z, x, y)));
        }
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
        .warp_tile(
            coord(),
            &TileStyle::from_raster_query("version=3.0&type=gray&bidx=1&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%7D").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(image.transform.forward(0.0, 0.0), (-180.0, 90.0));
    assert_eq!(image.bbox.xmin, -180.0);
    assert_eq!(image.bbox.ymax, 90.0);
    assert!(image.bbox.xmax.abs() < 1e-12);
    assert!((image.bbox.ymin + 90.0).abs() < 1e-12);
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
                        &TileStyle::from_raster_query("version=3.0&type=gray&bidx=1&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%7D")
                            .unwrap(),
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

#[tokio::test]
async fn incompatible_statistics_scope_errors_even_outside_coverage() {
    let store = Arc::new(InMemory::new());
    store
        .put(&ObjectPath::from("scope"), world_fixture(0., None).into())
        .await
        .unwrap();
    let reader = Arc::new(CogReader::open_object_store(store, "scope").await.unwrap());
    let style = TileStyle::from_json(r#"{"version":"3.0","renderer":{"type":"gray","bidx":[1]},"stretch":{"method":"minmax"},"statistics":{"scope":"mosaic","accuracy":"exact"}}"#).unwrap();
    let outside = TileCoord { x: 1, ..coord() };
    let error = Tiler::new(reader).tile(outside, &style).await.unwrap_err();
    assert!(
        error.to_string().contains("requires dataset statistics"),
        "{error}"
    );
}

#[tokio::test]
async fn statistics_respects_tiler_work_budget() {
    use async_geotiff::resource::WorkBudget;
    let store = Arc::new(InMemory::new());
    store
        .put(
            &ObjectPath::from("audit-stats"),
            world_fixture(0., Some([10, 20, 30])).into(),
        )
        .await
        .unwrap();
    let mut cfg = async_geotiff::io::ReaderConfig::default();
    let reader_budget = WorkBudget::new(512 * 1024 * 1024);
    cfg.work_budget = reader_budget.clone();
    let reader = Arc::new(
        CogReader::builder()
            .reader_config(cfg)
            .open_object_store(store, "audit-stats")
            .await
            .unwrap(),
    );
    let tile_budget = WorkBudget::new(32 * 1024 * 1024);
    let tiler = Tiler::new(reader).with_work_budget(tile_budget.clone());
    let style =
        TileStyle::from_raster_query("version=3.0&type=gray&bidx=2&method=minmax&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A64%7D%7D")
            .unwrap();
    let result = tiler.tile(coord(), &style).await;
    assert!(
        matches!(result, Err(async_geotiff::error::AgError::ResourceLimit(_))),
        "statistics grid exceeds configured 32 MiB tiler budget"
    );
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
            futures_util::future::join_all((0..16).map(|_| tiler.tile(local_coord(), &style)))
                .await;
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
        futures_util::future::join_all((0..16).map(|_| tiler.warp_tile(local_coord(), &style)))
            .await;
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
