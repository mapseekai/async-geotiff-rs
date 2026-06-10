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
                .checked_shl(20)
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
        TilerConfig {
            max_blocking_tasks: self.tiler.max_blocking_tasks,
        }
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
    let viewer_html = Arc::new(maplibre_viewer_html(view));
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
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    println!("Listening on: http://{bind_addr}");
    println!("Loopback COG base: {http_base}");
    println!("MapLibre viewer: {http_base}/maplibre");
    println!("OpenLayers viewer: {http_base}/openlayers");
    println!("Leaflet viewer: {http_base}/leaflet");
    let tile_base = format!("{http_base}/tiles/WebMercatorQuad/{{z}}/{{x}}/{{y}}");
    println!("Tile endpoint: {tile_base}.png");
    println!("  WebP tiles: {tile_base}.webp");
    println!("  JPEG tiles: {tile_base}.jpg");
    println!("  (the path extension picks the format; WebP/JPEG require: --features webp,jpeg)");
    println!("Formats endpoint: {http_base}/formats");
    if mosaic_loaded {
        println!(
            "Mosaic endpoint: {http_base}/mosaic/tiles/WebMercatorQuad/{{z}}/{{x}}/{{y}}.png?pixel_selection=first"
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

fn maplibre_viewer_html(view: DefaultView) -> String {
    include_str!("maplibre.html")
        .replace(
            "__ASYNC_GEOTIFF_DEFAULT_CENTER__",
            &format!("{:.6},{:.6}", view.center_lon, view.center_lat),
        )
        .replace(
            "__ASYNC_GEOTIFF_DEFAULT_ZOOM__",
            &format!("{:.2}", view.zoom),
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

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl FnMut(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find_map(|(k, v)| (*k == key).then(|| (*v).to_string()))
        }
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
