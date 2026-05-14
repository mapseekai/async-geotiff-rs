use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::error::AgError;
use crate::server::state::AppState;
use crate::style::query::RenderSpec;
use crate::tms::TmsKind;

pub async fn tile_handler(
    State(state): State<AppState>,
    Path((tms, z, x, ye)): Path<(String, u8, u32, String)>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    // ye is "{y}.{ext}"; split.
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

    // Reconstruct repeated-param query string so `bidx`/`rescale` preserve order.
    let q: String = params
        .into_iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");
    let spec = match RenderSpec::from_query(&q) {
        Ok(s) => s,
        Err(e) => return with_cors((StatusCode::BAD_REQUEST, e.to_string()).into_response()),
    };

    match state.tiler.tile(tms_kind, z, x, y, &spec).await {
        Ok(bytes) => with_cors(([("content-type", "image/png")], bytes).into_response()),
        Err(e) => error_response(e),
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
    add_cors_headers(response.headers_mut());
    response
}

fn add_cors_headers(headers: &mut HeaderMap) {
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
}
