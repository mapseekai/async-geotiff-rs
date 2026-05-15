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

const MAX_DATA_RESPONSE_BYTES: u64 = 32 << 20;

/// Shared HTTP state.
///
/// - `default_tiler`: a single COG opened from the CLI argument (URL),
///   served when no `?dataset=` is given.
/// - `data_dir` + `datasets`: local `*.tif` discovered at startup and
///   exposed over this server's `/data/<name>` route. `?dataset=<name>`
///   opens that file over HTTP (loopback, same object_store HTTP path as
///   production) and caches the `Tiler` lazily.
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
    /// Datasets are opened over HTTP from this server's own `/data/<name>`
    /// route and cached lazily. Concurrent first-hits are coalesced by moka.
    pub async fn tiler_for(&self, dataset: Option<&str>) -> AgResult<Arc<Tiler>> {
        if let Some(name) = dataset.filter(|s| !s.is_empty()) {
            if !self.datasets.iter().any(|d| d == name) {
                return Err(AgError::Tiff(format!("unknown dataset: {name}")));
            }
            let url = self.dataset_url(name)?;
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

    fn dataset_url(&self, name: &str) -> AgResult<String> {
        let mut url = url::Url::parse(&self.http_base)
            .map_err(|e| AgError::Tiff(format!("invalid local base url: {e}")))?;
        url.path_segments_mut()
            .map_err(|_| AgError::Tiff("local base url cannot be a base".into()))?
            .push("data")
            .push(name);
        Ok(url.into())
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
        return with_cors((StatusCode::UNPROCESSABLE_ENTITY, "unsupported format").into_response());
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

    let dataset = params
        .iter()
        .find(|(k, _)| k == "dataset")
        .map(|(_, v)| v.clone());

    let spec = match RenderSpec::from_pairs(params.iter().map(|(k, v)| (k.as_str(), v.as_str()))) {
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

/// `GET /datasets` -> JSON array of available local dataset filenames.
pub async fn datasets_handler(State(state): State<AppState>) -> Response {
    match serde_json::to_string(&*state.datasets) {
        Ok(body) => with_cors(([("content-type", "application/json")], body).into_response()),
        Err(e) => with_cors(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("serialize datasets: {e}"),
            )
                .into_response(),
        ),
    }
}

/// `GET /bounds?dataset=<name>` -> JSON `[west, south, east, north]`
/// in WGS84 so viewers can fly to the dataset. Opens and caches the
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

/// Parse a single-range `Range: bytes=START-[END]` header. Multi-range and
/// suffix forms are rejected because object_store's HTTP store does not need
/// them for this loopback COG path.
fn parse_byte_range(h: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let spec = h.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (s, e) = spec.split_once('-')?;
    if s.trim().is_empty() {
        return None;
    }
    let start: u64 = s.trim().parse().ok()?;
    let end: u64 = if e.trim().is_empty() {
        total - 1
    } else {
        e.trim().parse().ok()?
    };
    let end = end.min(total - 1);
    if start > end || start >= total {
        return None;
    }
    Some((start, end))
}

/// `GET /data/:name` serves a discovered local `*.tif` with bounded `Range`
/// support so object_store's HTTP store can do ranged reads. Only names in
/// the startup dataset list are served.
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
        Err(e) => return with_cors((StatusCode::NOT_FOUND, format!("open: {e}")).into_response()),
    };
    let total = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            return with_cors(
                (StatusCode::INTERNAL_SERVER_ERROR, format!("stat: {e}")).into_response(),
            )
        }
    };

    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let range = match range_header {
        Some(h) => match parse_byte_range(h, total) {
            Some(r) => Some(r),
            None => return range_not_satisfiable(total),
        },
        None => None,
    };

    let (status, start, len) = match range {
        Some((s, e)) => (StatusCode::PARTIAL_CONTENT, s, e - s + 1),
        None => (StatusCode::OK, 0, total),
    };
    if len > MAX_DATA_RESPONSE_BYTES {
        return with_cors(
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("response exceeds {MAX_DATA_RESPONSE_BYTES} byte demo limit; use Range"),
            )
                .into_response(),
        );
    }

    if file.seek(SeekFrom::Start(start)).await.is_err() {
        return with_cors((StatusCode::INTERNAL_SERVER_ERROR, "seek").into_response());
    }
    let Ok(buf_len) = usize::try_from(len) else {
        return with_cors((StatusCode::PAYLOAD_TOO_LARGE, "range too large").into_response());
    };
    let mut buf = vec![0u8; buf_len];
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

fn range_not_satisfiable(total: u64) -> Response {
    let response = Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, format!("bytes */{total}"))
        .body(Body::from("unsatisfiable range"));
    match response {
        Ok(resp) => with_cors(resp),
        Err(_) => {
            with_cors((StatusCode::RANGE_NOT_SATISFIABLE, "unsatisfiable range").into_response())
        }
    }
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
