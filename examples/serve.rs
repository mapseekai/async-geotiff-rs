use std::sync::Arc;

use async_geotiff::crs::Crs;
use async_geotiff::io::CogReader;
use async_geotiff::tiler::Tiler;
use async_geotiff::warp::Projector;
use axum::{response::Html, routing::get};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("async_geotiff=debug,warn")),
        )
        .with_target(false)
        .init();

    let url = std::env::args()
        .nth(1)
        .expect("usage: cargo run --example serve --features server -- <cog-url>");
    let reader = CogReader::open_http(&url).await?;
    let view = default_view(&reader).unwrap_or(DefaultView {
        center_lon: 0.0,
        center_lat: 0.0,
        zoom: 2.0,
    });
    let viewer_html = Arc::new(maplibre_viewer_html(view));
    let reader = Arc::new(reader);
    let tiler = Arc::new(Tiler::new(reader));
    let state = async_geotiff::server::AppState { tiler };
    let app = async_geotiff::server::router(state).route(
        "/",
        get({
            let viewer_html = viewer_html.clone();
            move || {
                let viewer_html = viewer_html.clone();
                async move { Html((*viewer_html).clone()) }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    println!("MapLibre viewer: http://127.0.0.1:8080/");
    println!("Tile endpoint: http://127.0.0.1:8080/tiles/WebMercatorQuad/{{z}}/{{x}}/{{y}}.png");
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
