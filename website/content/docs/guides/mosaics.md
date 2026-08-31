---
title: Mosaics and STAC
description: Render overlapping COG assets from MosaicJSON, STAC, or a custom source.
weight: 20
---

The default-enabled `mosaic` Cargo feature adds `MosaicTiler` and the
`MosaicSource` trait; applications that do not need mosaics can disable default
features. Because that also disables `proj`, use
`--no-default-features --features proj` when general CRS transformation is
still required. The source maps a tile coordinate to assets; the tiler opens
and warps those assets, reduces overlapping pixels, and renders the composite
once.

## MosaicJSON {#mosaicjson}

Run the embedded example or pass a MosaicJSON file:

```bash
cargo run --example mosaic_json
cargo run --example mosaic_json -- /path/to/mosaic.json
```

The parser accepts MosaicJSON 0.0.3. Its quadkey index is defined in
`WebMercatorQuad`; using a MosaicJSON source with `WorldCRS84Quad` returns a
query error.

Connect the example server to a file at startup:

```bash
ASYNC_GEOTIFF_MOSAIC=/path/to/mosaic.json \
  cargo run --release --example serve
```

Tiles are then available under `/mosaic/tiles/...`.

## Pixel selection {#selection}

| Strategy | Behavior |
| --- | --- |
| `First` | First valid pixel wins; stops early after the tile is fully covered. |
| `Highest` | Maximum value in the first output band per pixel across every asset. |
| `Lowest` | Minimum value in the first output band per pixel across every asset. |
| `Mean` | Streaming mean of valid samples; constant memory in asset count. |
| `Median` | Median of valid samples; retains every contributing image until completion. |

“First output band” is internal index 0; user-facing `bidx` values remain
1-indexed.

`MosaicConfig` defaults to 8 assets processed concurrently per chunk, an
opened-reader capacity of 256 in each `MosaicTiler` reader pool, and a hard cap
of 64 assets per requested tile. When the cap is exceeded, the tiler logs a
warning and keeps the first assets in source-provided order. Configure all
three limits through the builder:

```rust
use std::sync::Arc;

use async_geotiff::io::block_cache::BlockCache;
use async_geotiff::io::cache::CacheConfig;
use async_geotiff::mosaic::{HttpAccessPolicy, MosaicConfig, MosaicTiler};

let cache = Arc::new(BlockCache::new(CacheConfig::default()));
let config = MosaicConfig {
    chunk_size: 4,
    reader_capacity: 64,
    max_assets_per_tile: 32,
};
let mosaic = MosaicTiler::builder(source)
    .config(config)
    .block_cache(Arc::clone(&cache))
    .http_policy(HttpAccessPolicy::strict())
    .build();
```

Keep the asset cap enabled for untrusted or low-zoom indexes to prevent fetch
amplification. Choose a deterministic source order when truncation would affect
visual correctness.

## STAC demo {#stac}

Enable `stac` and provide all three startup settings:

```bash
ASYNC_GEOTIFF_STAC_URL=https://earth-search.aws.element84.com/v1 \
ASYNC_GEOTIFF_STAC_COLLECTION=sentinel-2-l2a \
ASYNC_GEOTIFF_STAC_ASSET_KEY=visual \
  cargo run --release --example serve --features stac
```

The API, collection, and asset key are fixed at process startup. HTTP query
parameters cannot turn the demo into an arbitrary STAC or COG proxy.

After startup, open <http://127.0.0.1:8080/maplibre>. When STAC is configured,
the MapLibre viewer selects mosaic mode by default and lets you verify the
configured collection without guessing a tile coordinate.

## HTTP asset policy {#http-policy}

The default policy allows public and private HTTP(S) assets but blocks
loopback, link-local, and cloud-metadata destinations. Use
`HttpAccessPolicy::strict()` for fully untrusted documents: it requires HTTPS
and also blocks private address ranges. `permissive()` should be reserved for
operator-controlled asset lists.

The example server uses the default policy and does not expose an environment
override. A service that accepts untrusted mosaic or STAC documents should
construct `MosaicTiler` itself and pass `strict()` through the builder as shown
above.

Presigned query strings are redacted from mosaic errors and log messages.
The access policy is a URL-resolution guard, not a complete outbound proxy
sandbox. Applications still own client timeouts, redirect policy, egress
firewalling, document-size limits, and any logging they add around the library.
