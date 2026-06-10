use std::io::SeekFrom;
use std::path::PathBuf;
#[cfg(feature = "perf-tracing")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
#[cfg(feature = "perf-tracing")]
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::Method;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{routing::get, Router};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::Instrument;

use async_geotiff::error::{AgError, AgResult};
use async_geotiff::io::{BlockCache, BlockCacheStats, CogReader, ReaderConfig};
use async_geotiff::mosaic::{MosaicJsonSource, MosaicTiler, PixelSelection};
use async_geotiff::style::{TileFormat, TileStyle};
use async_geotiff::tiler::{Tiler, TilerConfig};
use async_geotiff::tms::TileMatrixSet;

const MAX_DATA_RESPONSE_BYTES: u64 = 32 << 20;
#[cfg(feature = "perf-tracing")]
static NEXT_TILE_TRACE_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(feature = "perf-tracing")]
fn next_tile_trace_id() -> u64 {
    NEXT_TILE_TRACE_ID.fetch_add(1, Ordering::Relaxed)
}

#[cfg(feature = "perf-tracing")]
struct TileTraceFields<'a> {
    route: &'static str,
    tms: &'a str,
    tile_matrix_set: TileMatrixSet,
    z: u8,
    x: u32,
    y: u32,
    format: &'a TileFormat,
    dataset: Option<&'a str>,
}

#[cfg(feature = "perf-tracing")]
fn tile_trace_span(fields: TileTraceFields<'_>) -> tracing::Span {
    tracing::info_span!(
        "tile",
        tile_id = next_tile_trace_id(),
        route = fields.route,
        tms = %fields.tms,
        tile_matrix_set = ?fields.tile_matrix_set,
        z = fields.z,
        x = fields.x,
        y = fields.y,
        format = ?fields.format,
        dataset = fields.dataset.unwrap_or("")
    )
}

#[cfg(feature = "perf-tracing")]
struct StageTimer {
    label: &'static str,
    started: Instant,
    last: Instant,
    stages: Vec<(&'static str, Duration)>,
}

#[cfg(feature = "perf-tracing")]
impl StageTimer {
    fn new(label: &'static str) -> Self {
        let now = Instant::now();
        Self {
            label,
            started: now,
            last: now,
            stages: Vec::new(),
        }
    }

    fn mark(&mut self, name: &'static str) {
        let now = Instant::now();
        self.stages.push((name, now.duration_since(self.last)));
        self.last = now;
    }

    fn finish(self, outcome: &str, slow_tile_ms: Option<u64>) {
        let stages = self
            .stages
            .iter()
            .map(|(name, duration)| format!("{name}={:.2}ms", duration.as_secs_f64() * 1000.0))
            .collect::<Vec<_>>()
            .join(" ");
        let total_ms = self.started.elapsed().as_secs_f64() * 1000.0;

        if slow_tile_ms.is_some_and(|threshold| total_ms >= threshold as f64) {
            tracing::info!(
                label = self.label,
                stages = %stages,
                total_ms,
                outcome,
                "tile timing"
            );
        } else {
            tracing::debug!(
                label = self.label,
                stages = %stages,
                total_ms,
                outcome,
                "tile timing"
            );
        }
    }

    fn finish_ok_bytes(self, bytes: &bytes::Bytes, slow_tile_ms: Option<u64>) {
        self.finish(&format!("ok {} bytes", bytes.len()), slow_tile_ms);
    }
}

#[cfg(not(feature = "perf-tracing"))]
struct StageTimer;

#[cfg(not(feature = "perf-tracing"))]
impl StageTimer {
    fn new(_label: &'static str) -> Self {
        Self
    }

    fn mark(&mut self, _name: &'static str) {}

    fn finish(self, _outcome: &str, _slow_tile_ms: Option<u64>) {}

    fn finish_ok_bytes(self, _bytes: &bytes::Bytes, _slow_tile_ms: Option<u64>) {}
}

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
    /// Optional MosaicJSON-backed mosaic, served at `/mosaic/tiles/...`.
    /// Loaded once at startup from `ASYNC_GEOTIFF_MOSAIC` (file path).
    pub mosaic: Option<Arc<MosaicTiler<MosaicJsonSource>>>,
    http_base: String,
    tilers: moka::future::Cache<String, Arc<Tiler>>,
    /// One decoded-block cache shared by every per-dataset reader, so the
    /// total decode-memory budget is bounded once instead of per dataset.
    block_cache: Arc<BlockCache>,
    max_decode_tasks: usize,
    max_tile_tasks: usize,
    slow_tile_ms: Option<u64>,
}

impl AppState {
    pub fn new(
        default_tiler: Option<Arc<Tiler>>,
        data_dir: Option<PathBuf>,
        datasets: Vec<String>,
        http_base: impl Into<String>,
        block_cache: Arc<BlockCache>,
    ) -> Self {
        Self {
            default_tiler,
            data_dir,
            datasets: Arc::new(datasets),
            mosaic: None,
            http_base: http_base.into(),
            tilers: moka::future::Cache::new(64),
            block_cache,
            max_decode_tasks: ReaderConfig::default().max_decode_tasks,
            max_tile_tasks: TilerConfig::default().max_blocking_tasks,
            slow_tile_ms: None,
        }
    }

    pub fn with_runtime_tuning(
        mut self,
        max_decode_tasks: usize,
        max_tile_tasks: usize,
        slow_tile_ms: Option<u64>,
    ) -> Self {
        self.max_decode_tasks = max_decode_tasks.max(1);
        self.max_tile_tasks = max_tile_tasks.max(1);
        self.slow_tile_ms = slow_tile_ms;
        self
    }

    /// Attach a MosaicJSON-backed mosaic, exposed at `/mosaic/tiles/...`.
    pub fn with_mosaic(mut self, mosaic: Arc<MosaicTiler<MosaicJsonSource>>) -> Self {
        self.mosaic = Some(mosaic);
        self
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
            // Inject the one shared block cache, with the dataset name as the
            // stable `source_id`, so every dataset's readers draw from a single
            // bounded decode-memory budget (and re-opens reuse cached blocks).
            let mut cfg = ReaderConfig::default();
            cfg.source_id = name.to_string();
            cfg.block_cache = Arc::clone(&self.block_cache);
            cfg.max_decode_tasks = self.max_decode_tasks;
            let tiler_config = TilerConfig {
                max_blocking_tasks: self.max_tile_tasks,
            };
            return self
                .tilers
                .try_get_with(name.to_string(), async move {
                    let reader = CogReader::open_http_with_config_default(&url, cfg).await?;
                    Ok::<_, AgError>(Arc::new(Tiler::with_config(Arc::new(reader), tiler_config)))
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
            "/tiles/{tms}/{z}/{x}/{ye}",
            get(tile_handler).options(cors_preflight),
        )
        .route(
            "/mosaic/tiles/{tms}/{z}/{x}/{ye}",
            get(mosaic_tile_handler).options(cors_preflight),
        )
        .route("/datasets", get(datasets_handler))
        .route("/cache/stats", get(cache_stats_handler))
        .route("/formats", get(formats_handler))
        .route("/bounds", get(bounds_handler))
        .route(
            "/data/{name}",
            get(data_file_handler)
                .head(data_file_handler)
                .options(cors_preflight),
        )
        .with_state(state)
}

pub async fn tile_handler(
    State(state): State<AppState>,
    Path((tms, z, x, ye)): Path<(String, u8, u32, String)>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    let mut timer = StageTimer::new("tile");
    let (y_str, ext) = ye.split_once('.').unwrap_or((ye.as_str(), "png"));
    let path_format = match tile_format_from_extension(ext) {
        Ok(format) => format,
        Err(e) => return error_response(e),
    };
    let y: u32 = match y_str.parse() {
        Ok(v) => v,
        Err(_) => return with_cors((StatusCode::BAD_REQUEST, "bad y").into_response()),
    };

    let tile_matrix_set = match tms.parse::<TileMatrixSet>() {
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

    let style = match style_from_tile_params(&params, path_format) {
        Ok(s) => s,
        Err(e) => return error_response(e),
    };
    timer.mark("parse");

    #[cfg(feature = "perf-tracing")]
    let span = tile_trace_span(TileTraceFields {
        route: "tile",
        tms: &tms,
        tile_matrix_set,
        z,
        x,
        y,
        format: &style.format,
        dataset: dataset.as_deref(),
    });
    #[cfg(not(feature = "perf-tracing"))]
    let span = tracing::Span::none();
    async move {
        let tiler = match state.tiler_for(dataset.as_deref()).await {
            Ok(t) => t,
            Err(e) => return error_response(e),
        };
        timer.mark("select_tiler");

        let result = tiler.tile(tile_matrix_set, z, x, y, &style).await;
        timer.mark("render");

        match result {
            Ok(Some(bytes)) => {
                timer.finish_ok_bytes(&bytes, state.slow_tile_ms);
                with_cors(([("content-type", style.format.content_type())], bytes).into_response())
            }
            // Tile fully outside the dataset -> no content, so the map draws nothing
            // there (a black JPEG would otherwise cover out-of-range areas).
            Ok(None) => {
                timer.finish("no_content", state.slow_tile_ms);
                with_cors(StatusCode::NO_CONTENT.into_response())
            }
            Err(e) => {
                timer.finish("error", state.slow_tile_ms);
                error_response(e)
            }
        }
    }
    .instrument(span)
    .await
}

fn parse_pixel_selection(s: &str) -> Option<PixelSelection> {
    match s {
        "first" => Some(PixelSelection::First),
        "highest" => Some(PixelSelection::Highest),
        "lowest" => Some(PixelSelection::Lowest),
        "mean" => Some(PixelSelection::Mean),
        "median" => Some(PixelSelection::Median),
        _ => None,
    }
}

/// `GET /mosaic/tiles/{tms}/{z}/{x}/{y}.{ext}?pixel_selection=first&...`
/// Serves the startup-loaded MosaicJSON mosaic. Styling query params are
/// the same as `/tiles`; `pixel_selection` picks the reducer (default
/// `first`).
pub async fn mosaic_tile_handler(
    State(state): State<AppState>,
    Path((tms, z, x, ye)): Path<(String, u8, u32, String)>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    let mut timer = StageTimer::new("mosaic_tile");
    let Some(mosaic) = state.mosaic.clone() else {
        return with_cors(
            (
                StatusCode::NOT_FOUND,
                "no mosaic loaded; set ASYNC_GEOTIFF_MOSAIC to a MosaicJSON path",
            )
                .into_response(),
        );
    };

    let (y_str, ext) = ye.split_once('.').unwrap_or((ye.as_str(), "png"));
    let path_format = match tile_format_from_extension(ext) {
        Ok(format) => format,
        Err(e) => return error_response(e),
    };
    let y: u32 = match y_str.parse() {
        Ok(v) => v,
        Err(_) => return with_cors((StatusCode::BAD_REQUEST, "bad y").into_response()),
    };

    let tile_matrix_set = match tms.parse::<TileMatrixSet>() {
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

    let selection = match params.iter().find(|(k, _)| k == "pixel_selection") {
        Some((_, v)) => match parse_pixel_selection(v) {
            Some(s) => s,
            None => {
                return with_cors(
                    (
                        StatusCode::BAD_REQUEST,
                        format!("unknown pixel_selection: {v} (first|highest|lowest|mean|median)"),
                    )
                        .into_response(),
                )
            }
        },
        None => PixelSelection::First,
    };

    let style = match style_from_tile_params(&params, path_format) {
        Ok(s) => s,
        Err(e) => return error_response(e),
    };
    timer.mark("parse");

    #[cfg(feature = "perf-tracing")]
    let span = tile_trace_span(TileTraceFields {
        route: "mosaic_tile",
        tms: &tms,
        tile_matrix_set,
        z,
        x,
        y,
        format: &style.format,
        dataset: None,
    });
    #[cfg(not(feature = "perf-tracing"))]
    let span = tracing::Span::none();
    async move {
        let result = mosaic
            .tile(tile_matrix_set, z, x, y, selection, &style)
            .await;
        timer.mark("render");

        match result {
            Ok(Some(bytes)) => {
                timer.finish_ok_bytes(&bytes, state.slow_tile_ms);
                with_cors(([("content-type", style.format.content_type())], bytes).into_response())
            }
            // No asset covers this tile -> no content (map draws nothing here).
            Ok(None) => {
                timer.finish("no_content", state.slow_tile_ms);
                with_cors(StatusCode::NO_CONTENT.into_response())
            }
            Err(e) => {
                timer.finish("error", state.slow_tile_ms);
                error_response(e)
            }
        }
    }
    .instrument(span)
    .await
}

fn tile_format_from_extension(ext: &str) -> AgResult<TileFormat> {
    let format = match ext.to_ascii_lowercase().as_str() {
        "png" => TileFormat::Png,
        "webp" => TileFormat::Webp,
        "jpg" | "jpeg" => TileFormat::Jpeg,
        _ => {
            return Err(AgError::UnsupportedPixel(format!(
                "unsupported tile extension '{ext}'; expected png|webp|jpg|jpeg"
            )))
        }
    };
    format.ensure_available()?;
    Ok(format)
}

fn style_from_tile_params(
    params: &[(String, String)],
    path_format: TileFormat,
) -> AgResult<TileStyle> {
    path_format.ensure_available()?;
    let mut style = TileStyle::from_pairs(params.iter().map(|(k, v)| (k.as_str(), v.as_str())))?;
    if style.format == path_format {
        return Ok(style);
    }
    if params.iter().any(|(k, _)| k == "format") {
        return Err(AgError::query(
            "format",
            format!(
                "query format {} does not match path extension {}",
                style.format.extension(),
                path_format.extension()
            ),
        ));
    }
    style.format = path_format;
    Ok(style)
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

/// `GET /cache/stats` -> current decoded-block cache counters.
pub async fn cache_stats_handler(State(state): State<AppState>) -> Response {
    with_cors(
        (
            [("content-type", "application/json")],
            cache_stats_json(&state.block_cache.stats()),
        )
            .into_response(),
    )
}

fn cache_stats_json(stats: &BlockCacheStats) -> String {
    serde_json::json!({
        "decoded": stats.decoded,
        "entries": stats.entries,
        "fetched": stats.fetched,
        "hits": stats.hits,
        "misses": stats.misses,
        "resident_bytes": stats.resident_bytes,
    })
    .to_string()
}

/// `GET /formats` -> JSON array of the tile output formats this build can
/// actually encode, e.g. `[{"value":"png","label":"PNG"}, ...]`. WebP/JPEG only
/// appear when the server was built with their cargo features, so viewers can
/// list only formats that won't 400. The `value` is the tile path extension
/// (`.png`/`.webp`/`.jpg`), which is what drives the output format.
pub async fn formats_handler() -> Response {
    use async_geotiff::style::TileFormat;

    let available: Vec<serde_json::Value> = [TileFormat::Png, TileFormat::Webp, TileFormat::Jpeg]
        .into_iter()
        .filter(TileFormat::is_available)
        .map(|f| {
            serde_json::json!({
                "value": f.extension(),
                "label": f.extension().to_uppercase(),
                "content_type": f.content_type(),
            })
        })
        .collect();

    match serde_json::to_string(&available) {
        Ok(body) => with_cors(([("content-type", "application/json")], body).into_response()),
        Err(e) => with_cors(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("serialize formats: {e}"),
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
        // Suffix range `bytes=-N`: the last N bytes. TIFF/COG readers use
        // this to fetch the file footer before they know the total size.
        let n: u64 = e.trim().parse().ok()?;
        if n == 0 {
            return None;
        }
        let start = total.saturating_sub(n);
        return Some((start, total - 1));
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

/// `GET /data/{name}` serves a discovered local `*.tif` with bounded `Range`
/// support so object_store's HTTP store can do ranged reads. Only names in
/// the startup dataset list are served.
pub async fn data_file_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    method: Method,
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

    // object_store's HTTP store issues a HEAD to learn the object size
    // before it ever does a ranged read. The size must be discoverable
    // regardless of file size, so HEAD must bypass the body-size cap and
    // never touch file bytes — answer purely from metadata.
    if method == Method::HEAD {
        let resp = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, total)
            .body(Body::empty());
        return match resp {
            Ok(r) => with_cors(r),
            Err(_) => with_cors((StatusCode::INTERNAL_SERVER_ERROR, "head").into_response()),
        };
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn params(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn router_builds_with_axum_08_capture_syntax() {
        let block_cache = Arc::new(BlockCache::new(async_geotiff::io::CacheConfig::default()));
        let state = AppState::new(None, None, Vec::new(), "http://127.0.0.1:0", block_cache);
        let _router = router(state);
    }

    #[test]
    fn cache_stats_json_exposes_hit_miss_and_resident_bytes() {
        let body = cache_stats_json(&async_geotiff::io::BlockCacheStats {
            hits: 10,
            misses: 4,
            decoded: 3,
            fetched: 3,
            entries: 2,
            resident_bytes: 4096,
        });

        assert_eq!(
            body,
            r#"{"decoded":3,"entries":2,"fetched":3,"hits":10,"misses":4,"resident_bytes":4096}"#
        );
    }

    #[cfg(feature = "perf-tracing")]
    #[test]
    fn tile_trace_ids_increase_monotonically() {
        let first = next_tile_trace_id();
        let second = next_tile_trace_id();

        assert_eq!(second, first + 1);
    }

    #[test]
    fn png_extension_keeps_default_format() {
        let style = style_from_tile_params(&params(&[]), TileFormat::Png).unwrap();
        assert_eq!(style.format, TileFormat::Png);
    }

    #[cfg(feature = "webp")]
    #[test]
    fn webp_extension_sets_format_when_query_absent() {
        assert_eq!(
            tile_format_from_extension("webp").unwrap(),
            TileFormat::Webp
        );
        let style = style_from_tile_params(&params(&[]), TileFormat::Webp).unwrap();
        assert_eq!(style.format, TileFormat::Webp);
    }

    #[cfg(feature = "jpeg")]
    #[test]
    fn jpeg_extensions_set_format_when_query_absent() {
        assert_eq!(tile_format_from_extension("jpg").unwrap(), TileFormat::Jpeg);
        assert_eq!(
            tile_format_from_extension("jpeg").unwrap(),
            TileFormat::Jpeg
        );
        let style = style_from_tile_params(&params(&[]), TileFormat::Jpeg).unwrap();
        assert_eq!(style.format, TileFormat::Jpeg);
    }

    #[cfg(not(feature = "webp"))]
    #[test]
    fn webp_extension_errors_without_feature() {
        let err = tile_format_from_extension("webp").unwrap_err();
        assert!(matches!(err, AgError::UnsupportedPixel(_)));
    }

    #[cfg(not(feature = "jpeg"))]
    #[test]
    fn jpeg_extension_errors_without_feature() {
        let err = tile_format_from_extension("jpg").unwrap_err();
        assert!(matches!(err, AgError::UnsupportedPixel(_)));
    }

    #[cfg(feature = "webp")]
    #[test]
    fn query_format_must_match_path_extension() {
        let err =
            style_from_tile_params(&params(&[("format", "webp")]), TileFormat::Png).unwrap_err();
        assert!(matches!(err, AgError::Query { .. }));
    }
}
