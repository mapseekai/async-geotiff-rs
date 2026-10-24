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
| `ASYNC_GEOTIFF_MAX_IN_FLIGHT_REQUESTS` | `64` | Shared tile request admission limit; must be positive. |
| `ASYNC_GEOTIFF_REQUEST_TIMEOUT_MS` | `30000` | Tile request timeout, including dataset lookup/opening; must be positive. |
| `ASYNC_GEOTIFF_PREFETCH_RING` | `1` | Native-block rings prefetched for default/local single-COG readers; `0` disables it. |
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

The request admission limit and timeout apply to the command-line default COG,
lazy `?dataset=` selections, MosaicJSON and STAC tiles. These paths share the
configured library admission limiter, including when running CPU work outlives
a cancelled HTTP request. Increasing the HTTP settings also increases the
effective library settings; no additional fixed 64-request admission cap or
30-second total request deadline is retained. Transport connection/read
timeouts remain independent. Excess requests return 503 and expired requests return 504.
Once the request deadline is reached, it takes precedence over a ready response
or transport error, including during the initial lazy dataset open. Transport
errors observed before that deadline keep their existing status.

Deadline, cancellation and overload classifications are preserved through
mosaic asset opening and wrapped TIFF errors: they return 504, 408 and 503
respectively, while ordinary HTTP asset errors still use redacted diagnostics.

Prefetch tuning applies to both single-COG entrypoints. Mosaic asset readers
keep their existing prefetch default of zero; the server does not expose a
Mosaic prefetch setting.

HTTP mosaic asset errors omit URL userinfo, query strings and fragments from
their diagnostic labels. Store-backed object keys retain their existing key
semantics. Keep source credentials in server-side configuration; tile callers
do not need the original asset URL.

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
