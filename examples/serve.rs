//! Example tile server + MapLibre viewer.
//!
//! The HTTP layer lives entirely in this example — the library crate
//! exposes only the rendering pipeline (`Tiler`, `CogReader`, …). Run:
//!
//! ```text
//! cargo run --release --example serve -- <optional-cog-url>
//! ```
//!
//! With no argument it serves every `data/*.tif` over its own
//! `/data/<name>` route (HTTP, range-capable) and offers a dataset
//! switcher in the viewer.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use async_geotiff::crs::Crs;
use async_geotiff::io::{BlockCache, CacheConfig, CogReader, ReaderConfig};
#[cfg(feature = "stac")]
use async_geotiff::mosaic::StacSource;
use async_geotiff::mosaic::{MosaicJsonSource, MosaicTiler};
use async_geotiff::tiler::{Tiler, TilerConfig};
use async_geotiff::warp::Projector;
use axum::{response::Html, routing::get};

#[path = "serve/server.rs"]
mod server;

struct ServerRuntimeConfig {
    block_cache: CacheConfig,
    reader: ReaderConfig,
    tiler: TilerConfig,
    slow_tile_ms: Option<u64>,
}

#[cfg(feature = "stac")]
struct StacDemoConfig {
    api_url: String,
    collection: String,
    asset_key: String,
}

#[cfg(feature = "stac")]
impl StacDemoConfig {
    fn from_env() -> Result<Option<Self>, String> {
        Self::from_env_values(|key| std::env::var(key).ok())
    }

    fn from_env_values(
        mut get: impl FnMut(&str) -> Option<String>,
    ) -> Result<Option<Self>, String> {
        let api_url = get("ASYNC_GEOTIFF_STAC_URL");
        let collection = get("ASYNC_GEOTIFF_STAC_COLLECTION");
        let asset_key = get("ASYNC_GEOTIFF_STAC_ASSET_KEY");
        if api_url.is_none() && collection.is_none() && asset_key.is_none() {
            return Ok(None);
        }
        let required = |name: &str, value: Option<String>| {
            value
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| format!("{name} is required when enabling the STAC demo"))
        };
        Ok(Some(Self {
            api_url: required("ASYNC_GEOTIFF_STAC_URL", api_url)?,
            collection: required("ASYNC_GEOTIFF_STAC_COLLECTION", collection)?,
            asset_key: required("ASYNC_GEOTIFF_STAC_ASSET_KEY", asset_key)?,
        }))
    }
}

impl ServerRuntimeConfig {
    fn from_env() -> Result<Self, String> {
        Self::from_env_values(|key| std::env::var(key).ok())
    }

    fn from_env_values(mut get: impl FnMut(&str) -> Option<String>) -> Result<Self, String> {
        let mut block_cache = CacheConfig::default();
        let mut reader = ReaderConfig::default();
        let mut tiler = TilerConfig::default();

        if let Some(mb) = parse_positive_u64_env(
            "ASYNC_GEOTIFF_BLOCK_CACHE_MB",
            get("ASYNC_GEOTIFF_BLOCK_CACHE_MB"),
        )? {
            block_cache.max_bytes = mb
                .checked_mul(1 << 20)
                .ok_or_else(|| "ASYNC_GEOTIFF_BLOCK_CACHE_MB is too large".to_string())?;
        }
        if let Some(tasks) = parse_positive_usize_env(
            "ASYNC_GEOTIFF_MAX_DECODE_TASKS",
            get("ASYNC_GEOTIFF_MAX_DECODE_TASKS"),
        )? {
            reader.max_decode_tasks = tasks;
        }
        if let Some(tasks) = parse_positive_usize_env(
            "ASYNC_GEOTIFF_MAX_TILE_TASKS",
            get("ASYNC_GEOTIFF_MAX_TILE_TASKS"),
        )? {
            tiler.max_blocking_tasks = tasks;
        }
        if let Some(requests) = parse_positive_usize_env(
            "ASYNC_GEOTIFF_MAX_IN_FLIGHT_REQUESTS",
            get("ASYNC_GEOTIFF_MAX_IN_FLIGHT_REQUESTS"),
        )? {
            tiler.max_in_flight_requests = requests;
        }
        if let Some(milliseconds) = parse_positive_u64_env(
            "ASYNC_GEOTIFF_REQUEST_TIMEOUT_MS",
            get("ASYNC_GEOTIFF_REQUEST_TIMEOUT_MS"),
        )? {
            tiler.request_timeout = std::time::Duration::from_millis(milliseconds);
        }
        // Speculative neighbour-block prefetch radius. Defaults to 1 in the
        // example server: browsing patterns make the surrounding ring exactly
        // what the next tile requests need. Set 0 to disable (e.g. when object
        // storage GETs are billed per request).
        match get("ASYNC_GEOTIFF_PREFETCH_RING") {
            Some(raw) => {
                reader.prefetch_ring = raw
                    .parse::<u8>()
                    .map_err(|_| "ASYNC_GEOTIFF_PREFETCH_RING must be 0-255".to_string())?;
            }
            None => reader.prefetch_ring = 1,
        }
        let slow_tile_ms = parse_positive_u64_env(
            "ASYNC_GEOTIFF_SLOW_TILE_MS",
            get("ASYNC_GEOTIFF_SLOW_TILE_MS"),
        )?;

        Ok(Self {
            block_cache,
            reader,
            tiler,
            slow_tile_ms,
        })
    }

    fn reader_config(&self, source_id: String, block_cache: Arc<BlockCache>) -> ReaderConfig {
        let mut cfg = ReaderConfig::default();
        cfg.source_id = source_id;
        cfg.block_cache = block_cache;
        cfg.max_decode_tasks = self.reader.max_decode_tasks;
        cfg.prefetch_ring = self.reader.prefetch_ring;
        cfg
    }

    fn tiler_config(&self) -> TilerConfig {
        self.tiler
    }
}

fn parse_positive_usize_env(name: &str, value: Option<String>) -> Result<Option<usize>, String> {
    value
        .map(|raw| {
            let parsed = raw
                .parse::<usize>()
                .map_err(|_| format!("{name} must be a positive integer"))?;
            if parsed == 0 {
                return Err(format!("{name} must be greater than zero"));
            }
            Ok(parsed)
        })
        .transpose()
}

fn parse_positive_u64_env(name: &str, value: Option<String>) -> Result<Option<u64>, String> {
    value
        .map(|raw| {
            let parsed = raw
                .parse::<u64>()
                .map_err(|_| format!("{name} must be a positive integer"))?;
            if parsed == 0 {
                return Err(format!("{name} must be greater than zero"));
            }
            Ok(parsed)
        })
        .transpose()
}

#[cfg(any(feature = "tokio-console", test))]
fn tokio_console_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|raw| {
        matches!(
            raw.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("async_geotiff=debug,warn"))
}

fn init_tracing() {
    use tracing_subscriber::prelude::*;

    #[cfg(feature = "tokio-console")]
    {
        if tokio_console_enabled(std::env::var("ASYNC_GEOTIFF_TOKIO_CONSOLE").ok().as_deref()) {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_filter(env_filter());
            tracing_subscriber::registry()
                .with(console_subscriber::spawn())
                .with(fmt_layer)
                .init();
            return;
        }
    }

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_filter(env_filter());
    tracing_subscriber::registry().with(fmt_layer).init();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    // Optional CLI arg: a single COG URL served when no ?dataset= is given.
    let url = std::env::args().nth(1);

    // Discover local data/*.tif for the dataset switcher.
    let data_dir = std::path::PathBuf::from("data");
    let mut datasets: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&data_dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let lower = name.to_ascii_lowercase();
            if lower.ends_with(".tif") || lower.ends_with(".tiff") {
                datasets.push(name);
            }
        }
    }
    datasets.sort();
    println!("Datasets discovered in ./data: {}", datasets.len());

    // One decoded-block cache shared by the default reader, every per-dataset
    // reader, and (indirectly) the mosaic — so the whole server runs under a
    // single bounded decode-memory budget instead of one cache per COG.
    let runtime = ServerRuntimeConfig::from_env()?;
    #[cfg(feature = "stac")]
    let stac_demo = StacDemoConfig::from_env()?;
    let block_cache = Arc::new(BlockCache::new(runtime.block_cache));

    let (default_tiler, view) = if let Some(u) = url.as_deref() {
        let cfg = runtime.reader_config(u.to_string(), Arc::clone(&block_cache));
        let reader = CogReader::builder().reader_config(cfg).open_http(u).await?;
        let v = default_view(&reader).unwrap_or(DefaultView {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 2.0,
        });
        (
            Some(Arc::new(Tiler::with_config(
                Arc::new(reader),
                runtime.tiler_config(),
            ))),
            v,
        )
    } else {
        (
            None,
            DefaultView {
                center_lon: 0.0,
                center_lat: 0.0,
                zoom: 2.0,
            },
        )
    };

    let bind_addr = bind_addr()?;
    let http_base = local_http_base(bind_addr);
    // Optional mosaic: ASYNC_GEOTIFF_MOSAIC=<path to a MosaicJSON file>.
    let mut state = server::AppState::new(
        default_tiler,
        Some(data_dir),
        datasets,
        http_base.clone(),
        Arc::clone(&block_cache),
    )
    .with_runtime_tuning(
        runtime.reader.max_decode_tasks,
        runtime.tiler.max_blocking_tasks,
        runtime.slow_tile_ms,
    )
    .with_request_tuning(
        runtime.tiler.max_in_flight_requests,
        runtime.tiler.request_timeout,
    );
    let mut mosaic_loaded = false;
    if let Ok(path) = std::env::var("ASYNC_GEOTIFF_MOSAIC") {
        let json = std::fs::read_to_string(&path)?;
        let source = MosaicJsonSource::from_str(&json)?;
        // Share the one process-wide block cache with the mosaic too, so the
        // whole server (single datasets + mosaic) runs under a single budget.
        let mosaic = MosaicTiler::builder(source)
            .block_cache(Arc::clone(&block_cache))
            .build();
        state = state.with_mosaic(Arc::new(mosaic));
        mosaic_loaded = true;
        println!("Mosaic loaded from {path}");
    }
    #[cfg(feature = "stac")]
    if let Some(stac) = stac_demo {
        if mosaic_loaded {
            return Err(
                "ASYNC_GEOTIFF_MOSAIC and ASYNC_GEOTIFF_STAC_* are mutually exclusive".into(),
            );
        }
        let source = StacSource::new(&stac.api_url, vec![stac.collection], stac.asset_key)?;
        let mosaic = MosaicTiler::builder(source)
            .block_cache(Arc::clone(&block_cache))
            .build();
        state = state.with_stac_mosaic(Arc::new(mosaic));
        mosaic_loaded = true;
        println!("STAC mosaic loaded from {}", stac.api_url);
    }
    let viewer_html = Arc::new(maplibre_viewer_html(view, mosaic_loaded));
    let app = server::router(state)
        .route(
            "/",
            get({
                let viewer_html = viewer_html.clone();
                move || {
                    let viewer_html = viewer_html.clone();
                    async move { Html((*viewer_html).clone()) }
                }
            }),
        )
        .route(
            "/maplibre",
            get({
                let viewer_html = viewer_html.clone();
                move || {
                    let viewer_html = viewer_html.clone();
                    async move { Html((*viewer_html).clone()) }
                }
            }),
        )
        .route(
            "/openlayers",
            get(|| async { Html(include_str!("openlayers.html")) }),
        )
        .route(
            "/leaflet",
            get(|| async { Html(include_str!("leaflet.html")) }),
        );
    // Compile the SDK's lazy schema and Q2 bindings before accepting requests.
    let style_query = "version=2.0&type=gray&bidx=1";
    async_geotiff::style::TileStyle::from_raster_query(style_query)?;
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    println!("Listening on: http://{bind_addr}");
    println!("Loopback COG base: {http_base}");
    println!("MapLibre viewer: {http_base}/maplibre");
    println!("OpenLayers viewer: {http_base}/openlayers");
    println!("Leaflet viewer: {http_base}/leaflet");
    let tile_base = format!("{http_base}/tiles/WebMercatorQuad/{{z}}/{{x}}/{{y}}");
    println!("Tile endpoint: {tile_base}.png?{style_query}");
    #[cfg(feature = "webp")]
    println!("  WebP tiles: {tile_base}.webp?{style_query}&format=webp&lossless=true");
    println!("Formats endpoint: {http_base}/formats");
    if mosaic_loaded {
        println!(
            "Mosaic endpoint: {http_base}/mosaic/tiles/WebMercatorQuad/{{z}}/{{x}}/{{y}}.png?{style_query}&pixel_selection=first&stage=before_channels"
        );
    } else {
        println!("Mosaic endpoint: disabled (set ASYNC_GEOTIFF_MOSAIC=<mosaic.json> to enable)");
    }
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct DefaultView {
    center_lon: f64,
    center_lat: f64,
    zoom: f64,
}

fn maplibre_viewer_html(view: DefaultView, default_mosaic: bool) -> String {
    include_str!("maplibre.html")
        .replace(
            "__ASYNC_GEOTIFF_DEFAULT_CENTER__",
            &format!("{:.6},{:.6}", view.center_lon, view.center_lat),
        )
        .replace(
            "__ASYNC_GEOTIFF_DEFAULT_ZOOM__",
            &format!("{:.2}", view.zoom),
        )
        .replace(
            "__ASYNC_GEOTIFF_DEFAULT_MODE__",
            if default_mosaic { "mosaic" } else { "single" },
        )
}

fn default_view(reader: &CogReader) -> Result<DefaultView, Box<dyn std::error::Error>> {
    let (width, height) = reader.level_dimensions(0)?;
    let transform = reader.transform();
    let mut corners = [
        transform.forward(0.0, 0.0),
        transform.forward(width as f64, 0.0),
        transform.forward(width as f64, height as f64),
        transform.forward(0.0, height as f64),
    ];

    if reader.src_crs() != &Crs::epsg(4326) {
        let projector = Projector::new(reader.src_crs(), &Crs::epsg(4326))?;
        projector.convert(&mut corners)?;
    }

    let mut lon_min = f64::INFINITY;
    let mut lon_max = f64::NEG_INFINITY;
    let mut lat_min = f64::INFINITY;
    let mut lat_max = f64::NEG_INFINITY;
    for (lon, lat) in corners {
        lon_min = lon_min.min(lon);
        lon_max = lon_max.max(lon);
        lat_min = lat_min.min(lat);
        lat_max = lat_max.max(lat);
    }
    let lon_span = (lon_max - lon_min).abs();
    let lat_span = (lat_max - lat_min).abs();
    let zoom = (360.0 / lon_span.max(lat_span).max(1e-9))
        .log2()
        .clamp(1.0, 14.0);

    Ok(DefaultView {
        center_lon: (lon_min + lon_max) / 2.0,
        center_lat: (lat_min + lat_max) / 2.0,
        zoom,
    })
}

fn bind_addr() -> Result<SocketAddr, Box<dyn std::error::Error>> {
    Ok(std::env::var("ASYNC_GEOTIFF_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "stac")]
    use async_geotiff::mosaic::{HttpAccessPolicy, MosaicSource, StacSource};
    #[cfg(feature = "stac")]
    use async_geotiff::tms::{TileCoord, TileMatrixSet};
    #[cfg(feature = "stac")]
    use axum::extract::{Json, State};
    #[cfg(feature = "stac")]
    use axum::routing::post;
    #[cfg(feature = "stac")]
    use axum::Router;
    #[cfg(feature = "stac")]
    use object_store::local::LocalFileSystem;
    #[cfg(feature = "stac")]
    use serde_json::{json, Value};
    #[cfg(feature = "stac")]
    use tokio::task::JoinHandle;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl FnMut(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find_map(|(k, v)| (*k == key).then(|| (*v).to_string()))
        }
    }

    #[test]
    fn runtime_config_preserves_request_limits() {
        let cfg = ServerRuntimeConfig::from_env_values(env(&[
            ("ASYNC_GEOTIFF_MAX_IN_FLIGHT_REQUESTS", "7"),
            ("ASYNC_GEOTIFF_REQUEST_TIMEOUT_MS", "250"),
        ]))
        .unwrap();
        assert_eq!(cfg.tiler_config().max_in_flight_requests, 7);
        assert_eq!(
            cfg.tiler_config().request_timeout,
            std::time::Duration::from_millis(250)
        );
        for key in [
            "ASYNC_GEOTIFF_MAX_IN_FLIGHT_REQUESTS",
            "ASYNC_GEOTIFF_REQUEST_TIMEOUT_MS",
        ] {
            assert!(ServerRuntimeConfig::from_env_values(env(&[(key, "0")])).is_err());
        }
    }

    #[test]
    fn runtime_config_rejects_cache_megabyte_overflow() {
        let overflow = (u64::MAX / (1 << 20) + 1).to_string();
        assert!(ServerRuntimeConfig::from_env_values(env(&[(
            "ASYNC_GEOTIFF_BLOCK_CACHE_MB",
            &overflow
        )]))
        .is_err());
    }

    #[test]
    fn runtime_config_overrides_cache_and_cpu_tuning_from_env() {
        let cfg = ServerRuntimeConfig::from_env_values(env(&[
            ("ASYNC_GEOTIFF_BLOCK_CACHE_MB", "1024"),
            ("ASYNC_GEOTIFF_MAX_DECODE_TASKS", "3"),
            ("ASYNC_GEOTIFF_MAX_TILE_TASKS", "5"),
            ("ASYNC_GEOTIFF_SLOW_TILE_MS", "150"),
        ]))
        .unwrap();

        assert_eq!(cfg.block_cache.max_bytes, 1024 << 20);
        assert_eq!(cfg.reader.max_decode_tasks, 3);
        assert_eq!(cfg.tiler.max_blocking_tasks, 5);
        assert_eq!(cfg.slow_tile_ms, Some(150));
    }

    #[test]
    fn runtime_config_rejects_zero_tuning_values() {
        let err = match ServerRuntimeConfig::from_env_values(env(&[(
            "ASYNC_GEOTIFF_MAX_DECODE_TASKS",
            "0",
        )])) {
            Ok(_) => panic!("zero decode task count should be rejected"),
            Err(err) => err,
        };

        assert!(err.contains("ASYNC_GEOTIFF_MAX_DECODE_TASKS"));
    }

    #[cfg(feature = "stac")]
    #[test]
    fn stac_demo_requires_all_startup_configuration() {
        let cfg = StacDemoConfig::from_env_values(env(&[
            ("ASYNC_GEOTIFF_STAC_URL", "https://stac.example.test"),
            ("ASYNC_GEOTIFF_STAC_COLLECTION", "demo"),
            ("ASYNC_GEOTIFF_STAC_ASSET_KEY", "visual"),
        ]))
        .unwrap()
        .expect("all STAC variables enable the demo");
        assert_eq!(cfg.collection, "demo");
        assert!(StacDemoConfig::from_env_values(env(&[(
            "ASYNC_GEOTIFF_STAC_URL",
            "https://stac.example.test",
        )]))
        .is_err());
    }

    #[test]
    fn maplibre_viewer_uses_the_configured_source_mode() {
        let view = DefaultView {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 2.0,
        };
        assert!(maplibre_viewer_html(view, true).contains("const SERVER_DEFAULT_MODE = \"mosaic\""));
        assert!(
            maplibre_viewer_html(view, false).contains("const SERVER_DEFAULT_MODE = \"single\"")
        );
    }

    #[cfg(feature = "stac")]
    #[derive(Clone)]
    struct StacFixture {
        asset_url: String,
        requests: Arc<tokio::sync::Mutex<Vec<Value>>>,
    }

    #[cfg(feature = "stac")]
    async fn mock_stac_search(
        State(fixture): State<StacFixture>,
        Json(request): Json<Value>,
    ) -> Json<Value> {
        fixture.requests.lock().await.push(request);
        Json(json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "stac_version": "1.0.0",
                "id": "fixture-item",
                "bbox": [-180.0, -90.0, 180.0, 90.0],
                "geometry": {"type": "Point", "coordinates": [0.0, 0.0]},
                "properties": {"datetime": "2024-01-01T00:00:00Z"},
                "assets": {
                    "visual": {
                        "href": fixture.asset_url,
                        "type": "image/tiff; application=geotiff"
                    }
                },
                "links": []
            }],
            "links": []
        }))
    }

    #[cfg(feature = "stac")]
    async fn serve_test_router(app: Router) -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture listener");
        let address = listener.local_addr().expect("fixture listener address");
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("fixture server should run");
        });
        (format!("http://{address}"), task)
    }

    #[cfg(feature = "stac")]
    async fn fixture_tile_coord() -> TileCoord {
        let store = Arc::new(LocalFileSystem::new_with_prefix(".").expect("fixture store"));
        let reader = CogReader::open_object_store(store, "tests/fixtures/rgb_nodata.tif")
            .await
            .expect("open fixture COG");
        let [west, south, east, north] = Tiler::new(Arc::new(reader))
            .wgs84_bounds()
            .expect("fixture bounds");
        let zoom = 12u8;
        let tiles = 2f64.powi(i32::from(zoom));
        let longitude = (west + east) / 2.0;
        let latitude = (south + north) / 2.0;
        let x = (((longitude + 180.0) / 360.0 * tiles)
            .floor()
            .clamp(0.0, tiles - 1.0)) as u32;
        let latitude_radians = latitude.to_radians();
        let y = (((1.0
            - (latitude_radians.tan() + 1.0 / latitude_radians.cos()).ln() / std::f64::consts::PI)
            / 2.0
            * tiles)
            .floor()
            .clamp(0.0, tiles - 1.0)) as u32;
        TileCoord {
            tms: TileMatrixSet::WebMercatorQuad,
            z: zoom,
            x,
            y,
        }
    }

    #[cfg(feature = "stac")]
    #[tokio::test]
    async fn stac_demo_queries_a_local_asset_and_renders_a_tile() {
        let data_name = "rgb_nodata.tif";
        assert!(std::path::Path::new("tests/fixtures")
            .join(data_name)
            .exists());
        let data_state = server::AppState::new(
            None,
            Some(std::path::PathBuf::from("tests/fixtures")),
            vec![data_name.to_string()],
            "http://127.0.0.1:0",
            Arc::new(BlockCache::new(CacheConfig::default())),
        );
        let (data_base, data_task) = serve_test_router(server::router(data_state)).await;
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let fixture = StacFixture {
            asset_url: format!("{data_base}/data/{data_name}"),
            requests: Arc::clone(&requests),
        };
        let (stac_base, stac_task) = serve_test_router(
            Router::new()
                .route("/search", post(mock_stac_search))
                .with_state(fixture.clone()),
        )
        .await;
        let coord = fixture_tile_coord().await;
        let source = StacSource::new(&stac_base, vec!["demo-collection".to_string()], "visual")
            .expect("construct STAC source");
        let assets = source
            .assets_for_tile(coord)
            .await
            .expect("STAC query should return the fixture asset");
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].url, fixture.asset_url);
        let mosaic = MosaicTiler::builder(source)
            .http_policy(HttpAccessPolicy::permissive())
            .build();
        let demo_state = server::AppState::new(
            None,
            None,
            Vec::new(),
            "http://127.0.0.1:0",
            Arc::new(BlockCache::new(CacheConfig::default())),
        )
        .with_stac_mosaic(Arc::new(mosaic));
        for query in [
            "version=2.0&type=rgb&bidx=1&bidx=2&bidx=3&pixel_selection=first&stage=before_channels",
            "version=2.0&type=rgb&bidx=1&bidx=2&bidx=3&pixel_selection=mean&stage=after_channels",
        ] {
            let response = server::mosaic_tile_handler(
                axum::extract::State(demo_state.clone()),
                axum::extract::Path((
                    "WebMercatorQuad".to_string(),
                    coord.z,
                    coord.x,
                    format!("{}.png", coord.y),
                )),
                axum::extract::RawQuery(Some(query.to_owned())),
            )
            .await;
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let tile = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("render STAC fixture tile");
            assert!(!tile.is_empty());
        }
        let requests = requests.lock().await;
        assert!(requests.iter().all(|request| {
            request["collections"] == json!(["demo-collection"])
                && request["bbox"]
                    .as_array()
                    .is_some_and(|bbox| bbox.len() == 4)
        }));
        data_task.abort();
        stac_task.abort();
    }

    #[tokio::test]
    async fn single_band_auto_style_renders_http_viewport() {
        let reader = CogReader::open_object_store(
            Arc::new(object_store::local::LocalFileSystem::new_with_prefix(".").unwrap()),
            "tests/fixtures/uint16.tif",
        )
        .await
        .unwrap();
        let tiler = Tiler::new(Arc::new(reader));
        let [w, s, e, n] = tiler.wgs84_bounds().unwrap();
        let zoom = 14;
        let grid = 2_f64.powi(zoom);
        let x = (((w + e) / 2.0 + 180.0) / 360.0 * grid).floor() as u32;
        let lat = ((s + n) / 2.0).to_radians();
        let y = ((1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * grid)
            .floor() as u32;
        let state = server::AppState::new(
            Some(Arc::new(tiler)),
            None,
            vec![],
            "http://127.0.0.1:0",
            Arc::new(BlockCache::new(CacheConfig::default())),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, server::router(state)).await.unwrap();
        });
        let client = reqwest::Client::new();
        let mut visible = false;
        for ty in y - 1..=y + 1 {
            for tx in x - 1..=x + 1 {
                let response = client.get(format!("{base}/tiles/WebMercatorQuad/{zoom}/{tx}/{ty}.png?version=2.0&type=gray&bidx=1&method=linear&rescale=0,65535&resampling=bilinear&reproject=bilinear")).send().await.unwrap();
                assert!(
                    response.status() == reqwest::StatusCode::OK
                        || response.status() == reqwest::StatusCode::NO_CONTENT,
                    "viewport tile failed: {}",
                    response.status()
                );
                if response.status() == reqwest::StatusCode::OK {
                    let pixels = image::load_from_memory(&response.bytes().await.unwrap())
                        .unwrap()
                        .to_rgba8();
                    assert_eq!(pixels.dimensions(), (256, 256));
                    visible |= pixels.pixels().any(|p| p[3] > 0);
                }
            }
        }
        assert!(
            visible,
            "single-band viewport must retain visible source pixels"
        );
        let invalid = client.get(format!("{base}/tiles/WebMercatorQuad/{zoom}/{x}/{y}.png?version=2.0&type=rgb&bidx=1&bidx=2&bidx=3")).send().await.unwrap();
        assert_eq!(invalid.status(), reqwest::StatusCode::BAD_REQUEST);
        task.abort();
    }

    #[test]
    fn tokio_console_env_accepts_common_truthy_values() {
        assert!(tokio_console_enabled(Some("1")));
        assert!(tokio_console_enabled(Some("true")));
        assert!(tokio_console_enabled(Some("yes")));
        assert!(!tokio_console_enabled(Some("0")));
        assert!(!tokio_console_enabled(None));
    }
}

fn local_http_base(bind_addr: SocketAddr) -> String {
    let host = if bind_addr.ip().is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        bind_addr.ip()
    };
    match host {
        IpAddr::V4(ip) => format!("http://{}:{}", ip, bind_addr.port()),
        IpAddr::V6(ip) => format!("http://[{}]:{}", ip, bind_addr.port()),
    }
}
