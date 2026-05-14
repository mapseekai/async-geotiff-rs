//! Optional axum HTTP server (feature `server`).

pub mod routes;
pub mod state;

pub use state::AppState;

use axum::{routing::get, Router};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/tiles/:tms/:z/:x/:ye",
            get(routes::tile_handler).options(routes::cors_preflight),
        )
        .with_state(state)
}
