//! Example tile server + MapLibre viewer.
//!
//! The HTTP layer lives entirely in this example — the library crate
//! exposes only the rendering pipeline (`Tiler`, `CogReader`, …). Run:
//!
//! ```text
//! cargo run --release --example serve --features-from-example
//! cargo run --release --example serve -- <optional-cog-url>
//! ```
//!
//! With no argument it serves every `data/*.tif` over its own
//! `/data/<name>` route (HTTP, range-capable) and offers a dataset
//! switcher in the viewer.

use std::sync::Arc;

use async_geotiff::crs::Crs;
use async_geotiff::io::CogReader;
use async_geotiff::tiler::Tiler;
use async_geotiff::warp::Projector;
use axum::{response::Html, routing::get};

/// Self-contained axum HTTP layer (state, routes, router). Kept in the
/// example so the library core has no web-framework dependency.
mod server {
    use std::io::SeekFrom;
    use std::path::PathBuf;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::extract::{Path, Query, State};
    use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::{routing::get, Router};
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    use async_geotiff::error::{AgError, AgResult};
    use async_geotiff::io::CogReader;
    use async_geotiff::style::query::RenderSpec;
    use async_geotiff::tiler::Tiler;
    use async_geotiff::tms::TmsKind;

    /// Shared HTTP state.
    ///
    /// - `default_tiler`: a single COG opened from the CLI argument (URL),
    ///   served when no `?dataset=` is given.
    /// - `data_dir` + `datasets`: local `*.tif` discovered at startup and
    ///   exposed over this server's `/data/<name>` route. `?dataset=<name>`
    ///   opens that file over HTTP (loopback — same object_store http path
    ///   as production) and caches the `Tiler` lazily (single-flight).
    #[derive(Clone)]
    pub struct AppState {
        pub default_tiler: Option<Arc<Tiler>>,
        pub data_dir: Option<PathBuf>,
        pub datasets: Arc<Vec<String>>,
        http_base: String,
        tilers: moka::future::Cache<String, Arc<Tiler>>,
    }

    impl AppState {
        pub fn new(
            default_tiler: Option<Arc<Tiler>>,
            data_dir: Option<PathBuf>,
            datasets: Vec<String>,
            http_base: impl Into<String>,
        ) -> Self {
            Self {
                default_tiler,
                data_dir,
                datasets: Arc::new(datasets),
                http_base: http_base.into(),
                tilers: moka::future::Cache::new(64),
            }
        }

        /// Resolve the `Tiler` for an optional `?dataset=` selector.
        /// Datasets are opened over HTTP from this server's own
        /// `/data/<name>` route and cached lazily (concurrent first-hits
        /// are coalesced by moka).
        pub async fn tiler_for(&self, dataset: Option<&str>) -> AgResult<Arc<Tiler>> {
            if let Some(name) = dataset.filter(|s| !s.is_empty()) {
                if !self.datasets.iter().any(|d| d == name) {
                    return Err(AgError::Tiff(format!("unknown dataset: {name}")));
                }
                let url = format!("{}/data/{}", self.http_base, name);
                return self
                    .tilers
                    .try_get_with(name.to_string(), async move {
                        let reader = CogReader::open_http(&url).await?;
                        Ok::<_, AgError>(Arc::new(Tiler::new(Arc::new(reader))))
                    })
                    .await
                    .map_err(|e: Arc<AgError>| AgError::Tiff(format!("open dataset {name}: {e}")));
            }
            self.default_tiler
                .clone()
                .ok_or_else(|| AgError::Tiff("no dataset selected and no default COG".into()))
        }
    }

    pub fn router(state: AppState) -> Router {
        Router::new()
            .route(
                "/tiles/:tms/:z/:x/:ye",
                get(tile_handler).options(cors_preflight),
            )
            .route("/datasets", get(datasets_handler))
            .route("/bounds", get(bounds_handler))
            .route(
                "/data/:name",
                get(data_file_handler).options(cors_preflight),
            )
            .with_state(state)
    }

    pub async fn tile_handler(
        State(state): State<AppState>,
        Path((tms, z, x, ye)): Path<(String, u8, u32, String)>,
        Query(params): Query<Vec<(String, String)>>,
    ) -> Response {
        let (y_str, ext) = ye.split_once('.').unwrap_or((ye.as_str(), "png"));
        if ext != "png" {
            return with_cors(
                (StatusCode::UNPROCESSABLE_ENTITY, "unsupported format").into_response(),
            );
        }
        let y: u32 = match y_str.parse() {
            Ok(v) => v,
            Err(_) => return with_cors((StatusCode::BAD_REQUEST, "bad y").into_response()),
        };

        let tms_kind = match tms.parse::<TmsKind>() {
            Ok(k) => k,
            Err(_) => {
                return with_cors(
                    (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        format!("unknown TMS: {tms}"),
                    )
                        .into_response(),
                )
            }
        };

        // `dataset` (and the cache-busting `_v`) select the source / force
        // a refresh; they are not render params. RenderSpec::from_query
        // ignores unknown keys, so the query is passed through unchanged.
        let dataset = params
            .iter()
            .find(|(k, _)| k == "dataset")
            .map(|(_, v)| v.clone());

        let q: String = params
            .into_iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");
        let spec = match RenderSpec::from_query(&q) {
            Ok(s) => s,
            Err(e) => return with_cors((StatusCode::BAD_REQUEST, e.to_string()).into_response()),
        };

        let tiler = match state.tiler_for(dataset.as_deref()).await {
            Ok(t) => t,
            Err(e) => return error_response(e),
        };

        match tiler.tile(tms_kind, z, x, y, &spec).await {
            Ok(bytes) => with_cors(([("content-type", "image/png")], bytes).into_response()),
            Err(e) => error_response(e),
        }
    }

    /// `GET /datasets` → JSON array of available local dataset filenames.
    pub async fn datasets_handler(State(state): State<AppState>) -> Response {
        let mut body = String::from("[");
        for (i, name) in state.datasets.iter().enumerate() {
            if i > 0 {
                body.push(',');
            }
            body.push('"');
            body.push_str(&name.replace('\\', "\\\\").replace('"', "\\\""));
            body.push('"');
        }
        body.push(']');
        with_cors(([("content-type", "application/json")], body).into_response())
    }

    /// `GET /bounds?dataset=<name>` → JSON `[west, south, east, north]`
    /// in WGS84 so viewers can fly to the dataset. Opens (and caches) the
    /// dataset on first call.
    pub async fn bounds_handler(
        State(state): State<AppState>,
        Query(params): Query<Vec<(String, String)>>,
    ) -> Response {
        let dataset = params
            .iter()
            .find(|(k, _)| k == "dataset")
            .map(|(_, v)| v.as_str());
        let tiler = match state.tiler_for(dataset).await {
            Ok(t) => t,
            Err(e) => return error_response(e),
        };
        match tiler.wgs84_bounds() {
            Ok([w, s, e, n]) => with_cors(
                (
                    [("content-type", "application/json")],
                    format!("[{w},{s},{e},{n}]"),
                )
                    .into_response(),
            ),
            Err(e) => error_response(e),
        }
    }

    /// Parse a single-range `Range: bytes=START-[END]` header. Multi-range
    /// and suffix forms are not used by object_store's http store.
    fn parse_byte_range(h: &str, total: u64) -> Option<(u64, u64)> {
        let spec = h.strip_prefix("bytes=")?;
        if spec.contains(',') {
            return None;
        }
        let (s, e) = spec.split_once('-')?;
        let start: u64 = s.trim().parse().ok()?;
        let end: u64 = if e.trim().is_empty() {
            total.saturating_sub(1)
        } else {
            e.trim().parse().ok()?
        };
        let end = end.min(total.saturating_sub(1));
        if start > end {
            return None;
        }
        Some((start, end))
    }

    /// `GET /data/:name` — serves a discovered local `*.tif` with `Range`
    /// support so object_store's http store can do ranged reads (loopback;
    /// keeps the production HTTP path exercised). Only names in the dataset
    /// list are served (no path traversal).
    pub async fn data_file_handler(
        State(state): State<AppState>,
        Path(name): Path<String>,
        headers: HeaderMap,
    ) -> Response {
        if name.contains('/') || name.contains("..") || !state.datasets.iter().any(|d| d == &name) {
            return with_cors((StatusCode::NOT_FOUND, "unknown dataset").into_response());
        }
        let Some(dir) = state.data_dir.clone() else {
            return with_cors((StatusCode::NOT_FOUND, "no data dir").into_response());
        };
        let path = dir.join(&name);
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                return with_cors((StatusCode::NOT_FOUND, format!("open: {e}")).into_response())
            }
        };
        let total = match file.metadata().await {
            Ok(m) => m.len(),
            Err(e) => {
                return with_cors(
                    (StatusCode::INTERNAL_SERVER_ERROR, format!("stat: {e}")).into_response(),
                )
            }
        };

        let range = headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|h| parse_byte_range(h, total));

        let (status, start, len) = match range {
            Some((s, e)) => (StatusCode::PARTIAL_CONTENT, s, e - s + 1),
            None => (StatusCode::OK, 0, total),
        };

        if file.seek(SeekFrom::Start(start)).await.is_err() {
            return with_cors((StatusCode::INTERNAL_SERVER_ERROR, "seek").into_response());
        }
        let mut buf = vec![0u8; len as usize];
        if file.read_exact(&mut buf).await.is_err() {
            return with_cors((StatusCode::INTERNAL_SERVER_ERROR, "read").into_response());
        }

        let mut builder = Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, len);
        if status == StatusCode::PARTIAL_CONTENT {
            builder = builder.header(
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{}", start, start + len - 1, total),
            );
        }
        match builder.body(Body::from(buf)) {
            Ok(resp) => with_cors(resp),
            Err(_) => with_cors((StatusCode::INTERNAL_SERVER_ERROR, "body").into_response()),
        }
    }

    pub async fn cors_preflight() -> Response {
        with_cors(StatusCode::NO_CONTENT.into_response())
    }

    fn error_response(e: AgError) -> Response {
        let status = match &e {
            AgError::Query { .. } | AgError::TileOutOfBounds { .. } => StatusCode::BAD_REQUEST,
            AgError::UnknownTms(_) | AgError::UnknownColormap(_) | AgError::UnsupportedPixel(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        with_cors((status, e.to_string()).into_response())
    }

    fn with_cors(mut response: Response) -> Response {
        let headers = response.headers_mut();
        headers.insert("access-control-allow-origin", HeaderValue::from_static("*"));
        headers.insert(
            "access-control-allow-methods",
            HeaderValue::from_static("GET, OPTIONS"),
        );
        headers.insert(
            "access-control-allow-headers",
            HeaderValue::from_static("*"),
        );
        headers.insert("access-control-max-age", HeaderValue::from_static("86400"));
        response
    }
}

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

    let viewer_html = Arc::new(maplibre_viewer_html(view));
    let state = server::AppState::new(
        default_tiler,
        Some(data_dir),
        datasets,
        "http://127.0.0.1:8080",
    );
    let app = server::router(state)
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
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    println!("MapLibre viewer: http://127.0.0.1:8080/maplibre");
    println!("MapLibre viewer: http://127.0.0.1:8080/openlayers");
    println!("MapLibre viewer: http://127.0.0.1:8080/leaflet");
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
