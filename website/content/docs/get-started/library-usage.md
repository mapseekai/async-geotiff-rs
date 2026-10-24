---
title: Library usage
description: Open a COG and render a tile directly from Rust.
weight: 30
---

The repository currently sets `publish = false`, so another local project uses
a path dependency while developing. Create the application next to the clone,
then replace the relative path if your directories differ:

```bash
# Starting inside the async-geotiff-rs checkout:
cd ..
cargo new async-geotiff-demo
cd async-geotiff-demo
```

```toml
[dependencies]
async-geotiff = { path = "../async-geotiff-rs" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

To avoid the system `libproj` dependency while retaining the mosaic APIs used
by the example server, set `default-features = false, features = ["mosaic"]` on
the path dependency. General CRS transformations will then be unavailable.

## Render one tile {#render-one-tile}

```rust
use std::sync::Arc;

use async_geotiff::io::cog::CogReader;
use async_geotiff::style::TileStyle;
use async_geotiff::tiler::Tiler;
use async_geotiff::tms::{TileCoord, TileMatrixSet};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = "https://raw.githubusercontent.com/cogeotiff/rio-tiler/0b08b7f35a8b639cee2f35a0cb565034f7b55bfd/tests/fixtures/cog.tif";
    let reader = Arc::new(
        CogReader::builder()
            .source_id(url)
            .open_http(url)
            .await?,
    );
    let tiler = Tiler::new(reader);
    let style = TileStyle::from_raster_query("version=3.0&type=pseudocolor&bidx=1&colormap_name=viridis&method=linear&rescale=0,3000")?;
    let coord = TileCoord {
        tms: TileMatrixSet::WebMercatorQuad,
        z: 0,
        x: 0,
        y: 0,
    };

    if let Some(bytes) = tiler.tile(coord, &style).await? {
        std::fs::write("tile.png", bytes)?;
    } else {
        eprintln!("tile is outside the dataset or projection domain");
    }
    Ok(())
}
```

`TileStyle::from_raster_query` accepts the same query string used by the example HTTP
server, without the leading `?`. Save the example as `src/main.rs`, then run
`cargo run`. The pinned URL is a small, range-capable test COG; replace it
after verifying the integration.

## Point queries and geographic bounds {#point-and-bounds}

`CogReader::point(x, y, &coord_crs, bidx)` returns a nearest-pixel value without
rendering. Coordinates use `coord_crs`; a point outside the image returns `None`.
Band indices are one-based and refer to the bands returned by `read_window`.
An empty selection returns all of those bands in their existing order.

When no independent TIFF mask is present, the first Alpha sample is promoted
to the validity mask and removed from the returned bands. Thus ordinary RGBA
data exposes RGB to point queries. With an independent mask, the reader keeps
its native bands and uses that mask instead. A masked pixel returns
`Some(PointValue { valid: false, .. })`, rather than a band-selection error.
Raster Style's separate restriction on Alpha sources still applies to rendering.

`wgs84_bounds()` gives the geographic envelope used for dataset statistics.
When a projected footprint contains a pole, the envelope includes that pole
and spans `[-180, 180]` longitude. An envelope can include space outside the
actual image; query pixels or use their validity mask to determine coverage.

The low-level `io::stats::Histogram::quantile` API accepts public `u64` bin counts and
accumulates them without `u64` overflow, including directly constructed
histograms. The 100th percentile is the upper edge of the last nonempty bin,
excluding trailing empty bins even when their preceding counts are very large.
These embedded histograms are not the statistics used by Raster Style.

## Share memory and CPU budgets {#shared-budgets}

For multiple COGs, create one block cache and give every reader a stable,
distinct source ID:

```rust
use std::sync::Arc;

use async_geotiff::io::block_cache::BlockCache;
use async_geotiff::io::cache::CacheConfig;
use async_geotiff::io::cog::CogReader;

let cache = Arc::new(BlockCache::new(CacheConfig::default()));

let first = CogReader::builder()
    .source_id("scene-a")
    .block_cache(Arc::clone(&cache))
    .open_http("https://example.com/a.tif")
    .await?;

let second = CogReader::builder()
    .source_id("scene-b")
    .block_cache(Arc::clone(&cache))
    .open_http("https://example.com/b.tif")
    .await?;
```

Readers with the same cache share its default 512 MiB budget. The source ID is
part of the cache key: reuse it only for the same logical COG.
