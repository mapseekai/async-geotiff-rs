---
title: Example server configuration
description: Environment variables accepted by the included Axum server.
weight: 30
---

These variables configure `cargo run --example serve`. They are example-server
contracts, not global library configuration.

| Variable | Default | Meaning |
| --- | --- | --- |
| `ASYNC_GEOTIFF_BIND` | `127.0.0.1:8080` | Socket address for the HTTP server. |
| `ASYNC_GEOTIFF_BLOCK_CACHE_MB` | `512` | Shared decoded-block cache capacity in MiB; must be positive. |
| `ASYNC_GEOTIFF_MAX_DECODE_TASKS` | available parallelism | Maximum concurrent decode/stitch CPU jobs; must be positive. |
| `ASYNC_GEOTIFF_MAX_TILE_TASKS` | available parallelism | Maximum concurrent tile warp/render CPU jobs; must be positive. |
| `ASYNC_GEOTIFF_PREFETCH_RING` | `1` | Number of native-block rings prefetched around a read; `0` disables it. |
| `ASYNC_GEOTIFF_SLOW_TILE_MS` | unset | Emit a slow-tile warning above this positive millisecond threshold. |
| `ASYNC_GEOTIFF_MOSAIC` | unset | Path to a MosaicJSON file loaded at startup. |
| `ASYNC_GEOTIFF_STAC_URL` | unset | Fixed STAC API URL; requires the `stac` feature and the next two variables. |
| `ASYNC_GEOTIFF_STAC_COLLECTION` | unset | Fixed STAC collection ID. |
| `ASYNC_GEOTIFF_STAC_ASSET_KEY` | unset | Fixed STAC COG asset key. |
| `ASYNC_GEOTIFF_TOKIO_CONSOLE` | off | Enable Tokio Console with `1`, `true`, `yes`, or `on` when compiled with `tokio-console`. |
| `RUST_LOG` | `async_geotiff=debug,warn` | Standard tracing filter for the example server. |

`ASYNC_GEOTIFF_MOSAIC` and the STAC configuration are mutually exclusive. All
three STAC variables must be supplied together.

The decode-task and tile-task values tune single-COG work, but are not a single
process-wide CPU cap. The command-line default COG uses separate limiters;
lazily selected local datasets share one limiter sized to the larger value;
mosaics own their own limiter. The example server does not expose environment
variables for `MosaicConfig` fields such as `max_assets_per_tile`.

## Example {#example}

```bash
ASYNC_GEOTIFF_BIND=0.0.0.0:8080 \
ASYNC_GEOTIFF_BLOCK_CACHE_MB=1024 \
ASYNC_GEOTIFF_MAX_DECODE_TASKS=8 \
ASYNC_GEOTIFF_MAX_TILE_TASKS=8 \
ASYNC_GEOTIFF_PREFETCH_RING=0 \
ASYNC_GEOTIFF_SLOW_TILE_MS=500 \
RUST_LOG=async_geotiff=info \
  cargo run --release --example serve -- https://example.com/cog.tif
```

Invalid numeric values fail at process startup instead of being silently
clamped.
