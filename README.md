# async-geotiff

[中文文档](README.zh-CN.md) | English | [Documentation site source](website/)

Pure-Rust async crate that reads cloud-hosted Cloud-Optimized GeoTIFFs (COGs)
and serves them as XYZ map tiles with Raster Style 3.0 inputs.

**No GDAL.** All raster IO goes through [`async-tiff`] + [`object_store`].
Reprojection through [`proj`] (libproj system dep, feature-gated). Affine math
through [`raffine`] (the author's own Rust port of Python's `affine`).
Colormaps via [`colorous`]. Resampling via
[`fast_image_resize`]. PNG encoding via [`image`].

## Quick start

```bash
cargo run --release --example serve -- https://example.com/cog.tif

# Or omit the URL to browse local data/*.tif files with the dataset picker.
cargo run --release --example serve

# then open one of:
# http://127.0.0.1:8080/maplibre
# http://127.0.0.1:8080/openlayers
# http://127.0.0.1:8080/leaflet

curl 'http://127.0.0.1:8080/tiles/WebMercatorQuad/0/0/0.png?version=3.0&type=pseudocolor&bidx=1&method=linear&rescale=0,3000&colormap_name=viridis'
```

The example server binds to `127.0.0.1:8080` by default. To expose it on a
network interface, set `ASYNC_GEOTIFF_BIND`, for example:

```bash
ASYNC_GEOTIFF_BIND=0.0.0.0:8080 cargo run --release --example serve
```

## Raster Style 3.0 inputs

Uses `raster-style = "=0.4.0"`. Rust entrypoints are `TileStyle::from_json`,
`from_raster_query`, and `from_raster_style`; all three viewers default to Q3.
New inputs declare `version=3.0&type=...`; omitted stretch uses fixed `[0,255]`.
HTTP accepts complete Q3 documents only. Unsupported protocol capabilities fail explicitly.
See the [migration guide and supported subset](website/content/docs/guides/styling.md).

## Band math expressions

`renderer.expression` uses one-based `bN` band references, parentheses,
arithmetic operators, and unary minus. Gray/pseudocolor accept one expression;
RGB accepts three semicolon-separated expressions. Expressions and `bidx` are
mutually exclusive. Referenced NoData and non-finite results become transparent.

```bash
curl 'http://127.0.0.1:8080/tiles/WebMercatorQuad/0/0/0.png?version=3.0&type=gray&expression=(b5-b4)%2F(b5%2Bb4)&method=linear&rescale=-1,1'
```

## Library usage

HTTP serving lives only in `examples/serve.rs`; the library itself does not
require axum. Downstream applications can use `CogReader`, `Tiler`, or
`MosaicTiler` directly to read COGs and produce tiles. When reading multiple
sources, reuse one `Arc<BlockCache>` and give each reader a distinct
`source_id` so all sources share one decoded-block memory budget.

## Optional STAC demo

With the existing `stac` feature enabled, startup environment variables can
connect `/mosaic/tiles/...` to a fixed STAC API, collection, and COG asset key:

```bash
ASYNC_GEOTIFF_STAC_URL=https://earth-search.aws.element84.com/v1 \
ASYNC_GEOTIFF_STAC_COLLECTION=sentinel-2-l2a \
ASYNC_GEOTIFF_STAC_ASSET_KEY=visual \
cargo run --example serve --features stac
```

All three variables are required and are mutually exclusive with
`ASYNC_GEOTIFF_MOSAIC`. They are read only at process startup; HTTP query
parameters cannot choose the STAC endpoint, collection, or asset key. STAC
assets still use the default HTTP access policy, which rejects loopback and
link-local destinations so the demo cannot become a client-controlled public
proxy.

STAC discovery follows same-origin GET/POST pagination in server order and
rejects incomplete results when limits are exceeded: 1 MiB per response,
8 MiB total responses, 128 pages and 8 MiB of asset metadata. The asset count
uses `MosaicConfig::max_assets_per_tile`; setting it to zero disables only
that count limit. Direct source calls can pass `DiscoveryOptions` to
`assets_for_tile_with_options`. Custom `MosaicSource` implementations should
override this method and `statistics_assets_with_options` to apply limits
before allocating; their compatibility defaults validate returned assets.

`StacSource::new` keeps its existing signature and disables automatic proxy
discovery. For authentication, explicit proxies or custom HTTP timeouts, use
`StacSource::with_http_client(api_url, http, collections, asset_key)?`, passing
your configured **reqwest 0.11** client. The old `with_client(stac_client::Client,
...)` constructor is deprecated and discovery now returns an error: that
opaque client's transport cannot enforce response byte limits. Carry its
authentication into the HTTP client's default headers when migrating; it
is never silently discarded.

## Styles and migration

All styles use Raster Style 3.0 JSON/Q3:

```rust
let style = async_geotiff::style::TileStyle::from_json(r#"{
  "version": "3.0",
  "renderer": {"type": "rgb", "bidx": [1, 2, 3]},
  "stretch": {"method": "linear", "rescale": [[0, 3000]]}
}"#)?;
let bytes = tiler.tile(coord, &style).await?;
```

For mosaics, declare the Mosaic 1.0.0 schema in `extensions` and add `raster:mosaic` to the same document and call `mosaic.tile(coord, &style)`.
Five selection methods, both merge stages, and rank channels are supported.
MosaicJSON/STAC supplies assets separately.

This is a breaking migration: `from_query`, `from_pairs`, mutable style fields,
the old `Stretch`, `ColourDefinition`, `Colour`, and `lut_from_stops` APIs,
and the separate mosaic selection argument are removed. The default style is gray band 1 with fixed 0–255 mapping;
provide a linear range for high-bit-depth data. Styles support PNG and lossless
WebP (with the `webp` feature). Unknown fields, old parameters, and unimplemented
capabilities return errors.

See the [styling guide](website/content/docs/guides/styling.md) for fields,
migration, and capabilities, and [tile API](website/content/docs/reference/tile-api.md)
for request paths and encoding.

## Supported TileMatrixSets

- `WebMercatorQuad` (EPSG:3857) — default for slippy XYZ clients
- `WorldCRS84Quad` (EPSG:4326)

## TMS supported pixel types

Source COG can be U8 / U16 / I16 / U32 / I32 / F32 / F64. Internally
unified to F64 at the read/warp seam.

## Build requirements

- Rust ≥ 1.92, matching `Cargo.toml`'s `rust-version`.
- For the default `proj` feature: `libproj` 9.x on the system
  (`brew install proj` on macOS; `apt install libproj-dev libsqlite3-dev
  libtiff-dev clang` on Debian/Ubuntu).
  Build without it via `cargo build --no-default-features`.

## Architecture

```
Example HTTP request (axum, `examples/serve.rs`)
  ├─ TMS.tile_bbox(z, x, y)            tile-grid
  ├─ CogReader.read_window              async-tiff + object_store
  │    └─ decoded native blocks from a shared BlockCache (one byte budget);
  │       misses fetched coalesced + decoded in semaphore-limited spawn_blocking
  ├─ warp::warp (if src_crs ≠ dst_crs)  proj coarse-grid + bilinear
  │    └─ runs in semaphore-limited spawn_blocking (Proj is !Send + !Sync)
  ├─ render pipeline:                   ndarray + raster-style
  │    ├─ rescale per band
  │    ├─ Gamma / opacity
  │    └─ Raster Style gray / RGB / native colormaps
  └─ encode PNG / lossless WebP           image (+ image-webp)
```

Out-of-range tiles return `Ok(None)` (the example server replies HTTP 204);
Lossless WebP output requires the `webp` cargo feature.

Module layout:

```
src/
  error.rs            AgError + AgResult
  bounds.rs           Bbox + GeoTransform (wraps raffine::Affine)
  crs.rs              Crs identifier
  pixel.rs            PixelData + ImageData
  tms.rs              WebMercatorQuad + WorldCRS84Quad
  io/
    cog.rs            CogReader open/metadata/block-fetch orchestration
    block_cache.rs    shared decoded-block cache (BlockKey/BlockCache, one budget)
    cache.rs          CacheConfig (block-cache byte budget + optional TTL)
    decode.rs         semaphore-limited single-block decode spawn_blocking helper
    window.rs         source tile-range and stitch helpers
  warp/               Projector + Resampling + coarse-grid warp
  render/             colormap, expression, encode, pipeline
  style/              Raster Style JSON/Q3 validation and render plans
  tiler.rs            orchestrator
examples/
  serve.rs            demo entrypoint and viewer wiring
  serve/server.rs     axum routes for the demo server
  maplibre.html       MapLibre viewer
  openlayers.html     OpenLayers viewer
  leaflet.html        Leaflet viewer
```

The library crate does not expose an axum server module; HTTP serving lives in
the example so downstream library users do not inherit web-framework
dependencies.

CPU-heavy decode/stitch and warp/render work is dispatched with
`spawn_blocking` behind async `Semaphore` limits. Use
`CogReader::builder()` (e.g. `.max_decode_tasks(n)`, `.block_cache(cache)`,
then `.open_http(url)` / `.open_object_store(store, key)` / `.open(reader)`)
to tune decode concurrency, and `Tiler::with_config` with
`async_geotiff::tiler::TilerConfig` to tune warp/render concurrency. Defaults
use the host's available parallelism and clamp zero to one permit.

Decoded native tile blocks are cached in a shared `Arc<BlockCache>` under one
byte budget (default **512 MB**). `ReaderConfig` carries a caller-supplied
`source_id` (the cache key prefix) and the `block_cache`; pass the **same**
`block_cache` with **distinct** `source_id`s to several readers (e.g. every
dataset in a server, or every asset in a mosaic) to bound their combined decode
memory to a single budget instead of one cache per reader. `MosaicTiler` does
this automatically via its `ReaderPool`.

### Performance notes

See the [large-COG performance comparison with TiTiler](docs/performance-comparison-titiler.md) (Chinese) for cold/warm requests, browser viewport loading, and concurrent batches.

- **Cache capacity is the strongest lever.** 512 MB holds a typical browsing
  viewport for large RGB COGs; use 1024 MB for multi-dataset servers or wide
  pan/zoom sessions.
- **Prefer ZSTD over DEFLATE when creating COGs.** Benchmarks showed ZSTD
  block decode at ~1.5 ms P50 vs ~4.5 ms P50 for DEFLATE (≈3× faster). Use
  `gdal_translate -co COMPRESS=ZSTD -co ZSTD_LEVEL=1` for lowest read latency.
- **`MAX_TILE_TASKS` should stay at its default (`available_parallelism()`).** Pinning
  it lower inflates warp/encode P95 at high concurrency via scheduling queuing.
  Only override after profiling confirms CPU contention at the default level.
- **Concurrent in-flight single-flight.** When multiple requests race for the
  same uncached block, only the first fetches and decodes it; the rest wait on
  the result channel. If the owner is canceled (client disconnect), waiters
  transparently retry with a fresh claim rather than surfacing a 500 error.

## Observability

The library emits [`tracing`](https://docs.rs/tracing) only — it takes **no**
OpenTelemetry (or other metrics-backend) dependency, so your application owns
the export pipeline. Two integration points:

**Spans → traces.** `Tiler::tile` and `CogReader::read_window` are
`#[tracing::instrument]`-ed (fields: source id, tile coords, level, format).
Bridge them to OpenTelemetry with a `tracing-opentelemetry` layer:

```rust
use tracing_subscriber::prelude::*;
tracing_subscriber::registry()
    .with(tracing_subscriber::EnvFilter::new("async_geotiff=info"))
    .with(tracing_opentelemetry::layer().with_tracer(tracer)) // your OTLP tracer
    .init();
```

**Cache metrics → gauges/counters.** You construct the shared `Arc<BlockCache>`,
so poll `BlockCache::stats()` periodically and feed it to your metrics backend:

```rust
let s = block_cache.stats(); // BlockCacheStats { hits, misses, decoded, fetched, entries, resident_bytes }
// hit ratio        = hits / (hits + misses)
// fetch amplification = fetched / decoded   (1.0 = no duplicate cold fetches)
// memory pressure  = resident_bytes / configured budget
```

Boundary metrics you can't read from the library (request rate, latency
percentiles, the HTTP-204 "no tile" rate) are best recorded where you call
`Tiler::tile` / `MosaicTiler::tile` — match on `Ok(Some)` / `Ok(None)` / `Err`
and record OTel histograms/counters there.

## License

MIT OR Apache-2.0
