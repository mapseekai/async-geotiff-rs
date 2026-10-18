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

## Raster Style 3.0

```rust
let style = TileStyle::from_json(r#"{
  "version": "3.0",
  "extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/mosaic/v1.0.0/schema.json"],
  "renderer": {"type": "rgb", "bidx": [3, 2, 1]},
  "stretch": {"method": "linear", "rescale": [[0, 3000]]},
  "raster:mosaic": {"pixel_selection": "highest", "stage": "before_channels", "rank_channel": 1}
}"#)?;
let tile = mosaic.tile(coord, &style).await?;
```

`before_channels` merges dependencies before evaluating expressions;
`after_channels` evaluates each source first, then merges outputs. The example's
dependency table is `[1,2,3]`, so rank_channel=1 compares source band 1;
after_channels would compare the first output, source band 3. Ties preserve the
first source's complete tuple. Mean/median use the same complete valid tuples
for every channel and avoid intermediate overflow for extreme finite values.

Q3 example:

```text
/mosaic/tiles/WebMercatorQuad/0/0/0.png?version=3.0&type=rgb&bidx=3&bidx=2&bidx=1&method=linear&rescale=0,3000&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Fmosaic%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Amosaic%22%3A%7B%22pixel_selection%22%3A%22mean%22%2C%22stage%22%3A%22after_channels%22%7D%7D
```

In MapLibre, select mosaic under Source to configure stage and ranking.
Asset locations remain server-bound; JSON/Q3 does not contain the asset list.
Protocol mosaics require explicit configuration. Source Alpha bands remain
unsupported and return a capability error. See the [styling guide](../styling/).

## Pixel selection {#selection}

| Strategy | Behavior |
| --- | --- |
| `First` | First valid pixel wins; stops early after the tile is fully covered. |
| `Highest` | Select the complete tuple with maximum rank_channel. |
| `Lowest` | Select the complete tuple with minimum rank_channel. |
| `Mean` | Streaming mean of valid samples; constant memory in asset count. |
| `Median` | Median of valid samples; retains every contributing image until completion. |

Both `bidx` and `rank_channel` are one-based; the latter is a position at the merge stage.

`MosaicConfig` defaults to 8 assets processed concurrently per chunk, an
opened-reader capacity of 256 in each `MosaicTiler` reader pool, and a hard cap
of 64 assets per requested tile. Exceeding the cap returns an error,
never a truncated partial result. Configure all
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
    ..Default::default()
};
let mosaic = MosaicTiler::builder(source)
    .config(config)
    .block_cache(Arc::clone(&cache))
    .http_policy(HttpAccessPolicy::strict())
    .build();
```

Keep the asset cap enabled for untrusted or low-zoom indexes to prevent fetch
amplification. Choose deterministic source order for first and extremum ties.
Median additionally limits retained inputs to 256 MiB and observes the shared work budget.

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
