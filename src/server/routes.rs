use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::server::state::AppState;
use crate::style::query::RenderSpec;
use crate::tms::TmsKind;

pub async fn tile_handler(
    State(state): State<AppState>,
    Path((tms, z, x, ye)): Path<(String, u8, u32, String)>,
    Query(params): Query<Vec<(String, String)>>,
) -> Response {
    // ye is "{y}.{ext}"; split.
    let (y_str, _ext) = ye.split_once('.').unwrap_or((ye.as_str(), "png"));
    let y: u32 = match y_str.parse() {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad y").into_response(),
    };

    let tms_kind = match TmsKind::from_str(&tms) {
        Ok(k) => k,
        Err(_) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("unknown TMS: {tms}"),
            )
                .into_response()
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
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };

    match state.tiler.tile(tms_kind, z, x, y, &spec).await {
        Ok(bytes) => ([("content-type", "image/png")], bytes).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
