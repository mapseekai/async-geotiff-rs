use std::sync::Arc;

use async_geotiff::io::CogReader;
use async_geotiff::tiler::Tiler;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args()
        .nth(1)
        .expect("usage: cargo run --example serve --features server -- <cog-url>");
    let reader = Arc::new(CogReader::open_http(&url).await?);
    let tiler = Arc::new(Tiler::new(reader));
    let state = async_geotiff::server::AppState { tiler };
    let app = async_geotiff::server::router(state);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    println!("Listening on http://0.0.0.0:8080");
    axum::serve(listener, app).await?;
    Ok(())
}
