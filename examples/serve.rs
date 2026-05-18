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
use async_geotiff::io::CogReader;
use async_geotiff::mosaic::{MosaicJsonSource, MosaicTiler};
use async_geotiff::tiler::Tiler;
use async_geotiff::warp::Projector;
use axum::{response::Html, routing::get};

#[path = "serve/server.rs"]
mod server;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("async_geotiff=debug,warn")),
        )
        .with_target(false)
        .init();

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

    let (default_tiler, view) = if let Some(u) = url.as_deref() {
        let reader = CogReader::open_http(u).await?;
        let v = default_view(&reader).unwrap_or(DefaultView {
            center_lon: 0.0,
            center_lat: 0.0,
            zoom: 2.0,
        });
        (Some(Arc::new(Tiler::new(Arc::new(reader)))), v)
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
    let mut state =
        server::AppState::new(default_tiler, Some(data_dir), datasets, http_base.clone());
    let mut mosaic_loaded = false;
    if let Ok(path) = std::env::var("ASYNC_GEOTIFF_MOSAIC") {
        let json = std::fs::read_to_string(&path)?;
        let source = MosaicJsonSource::from_str(&json)?;
        state = state.with_mosaic(Arc::new(MosaicTiler::new(source)));
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
    println!("Tile endpoint: {http_base}/tiles/WebMercatorQuad/{{z}}/{{x}}/{{y}}.png");
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
