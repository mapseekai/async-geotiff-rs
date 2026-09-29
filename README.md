# async-geotiff

[中文文档](README.zh-CN.md) | English | [Documentation site source](website/)

Pure-Rust async crate that reads cloud-hosted Cloud-Optimized GeoTIFFs (COGs)
and serves them as XYZ map tiles with titiler-style URL parameter styling.

**No GDAL.** All raster IO goes through [`async-tiff`] + [`object_store`].
Reprojection through [`proj`] (libproj system dep, feature-gated). Affine math
through [`raffine`] (the author's own Rust port of Python's `affine`).
Colormaps via [`colorous`] + [`enterpolation`]. Resampling via
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

curl 'http://127.0.0.1:8080/tiles/WebMercatorQuad/0/0/0.png?colormap_name=viridis&rescale=0,3000'
```

The example server binds to `127.0.0.1:8080` by default. To expose it on a
network interface, set `ASYNC_GEOTIFF_BIND`, for example:

```bash
ASYNC_GEOTIFF_BIND=0.0.0.0:8080 cargo run --release --example serve
```

## Band math expressions

Use the `expression` query parameter to calculate one derived band per pixel.
Band references are 1-indexed (`b1` is the first source band). The grammar is
arithmetic-only:

- band references such as `b1`, `b4`, and `b5` (uppercase `B` is also accepted);
- finite decimal numbers such as `2.5` and `0.5`;
- parentheses, binary `+`, `-`, `*`, `/`, and unary `-`.

Multiplication and division bind more tightly than addition and subtraction;
operators at the same level are evaluated left to right. Scientific notation
such as `1e3` is not supported. For example, NDVI can be rendered from bands 5
and 4 with:

```bash
curl 'http://127.0.0.1:8080/tiles/WebMercatorQuad/0/0/0.png?expression=(b5-b4)%2F(b5%2Bb4)&rescale=-1,1'
```

The `+` and `/` characters are percent-encoded here because this is a URL
query string. Equivalent expressions include `b1/2`, `-b1+3`, and
`(b2-b1)/(b2+b1)`.

`expression` produces a single derived band, so it is mutually exclusive with
`bidx` and `color_formula`, and accepts at most one `rescale` range. A source
nodata value referenced by the expression produces a transparent pixel.
Division by zero and other non-finite results are also rendered as transparent
pixels rather than being replaced with zero. Expressions are limited to 4096
bytes, 256 tokens, and 128 levels of nesting.

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

## URL contract (titiler-compatible subset)

Path: `/tiles/{tms}/{z}/{x}/{y}.{ext}` where `ext ∈ {png, webp, jpg, jpeg}`.
`webp` and `jpg`/`jpeg` require building with the matching cargo feature.

| Parameter | Repeat | Example | Notes |
|---|---|---|---|
| `bidx`          | yes | `?bidx=1&bidx=2&bidx=3` | 1-indexed band selection |
| `rescale`       | yes | `?rescale=0,3000&rescale=0,3000` | per-band `min,max` stretch (Custom; highest priority) |
| `stretch`       | no  | `?stretch=percent&pc=2,98` | auto stretch mode: `minmax`, `percent`, `stddev` |
| `pc`            | no  | `?pc=2,98` | percentile cut for `stretch=percent` (default `2,98`) |
| `sigma`         | no  | `?sigma=2.5` | σ multiplier for `stretch=stddev` (default `2.0`) |
| `nodata`        | no  | `?nodata=nan` | overrides dataset nodata (accepts `nan`, `inf`, `-inf`, float) |
| `colormap_name` | no  | `?colormap_name=viridis` | from `colorous`: viridis/magma/inferno/plasma/cividis/turbo/greys |
| `color_formula` | no  | `?color_formula=gamma%20RGB%201.5` | rio-color subset (`gamma`, `sigmoidal`, `saturation`) |
| `resampling`    | no  | `?resampling=average` | `nearest` (default), `bilinear`, `cubic`, `cubic_spline`, `lanczos`, `average` — `average` is a box mean over the covered source pixels (titiler/GDAL `average`): antialiasing on downsample, nearest on upsample |
| `reproject`     | no  | `?reproject=bilinear` | warp-time kernel (same kernel names as `resampling`) |
| `tilesize`      | no  | `?tilesize=512` | default 256; allowed values: 64, 128, 256, 512, 1024 |

Default tile size is **256** (not titiler's 512) for wider XYZ-client
compatibility; pass `?tilesize=512` to override. Tile sizes are restricted to
the power-of-two values above to prevent unbounded allocations and keep viewer
tile grids predictable.

### Data stretching (non-uint8 rasters)

UInt16/Int16/Float COGs need a value range or they render saturated white.
Stretch priority: explicit **`rescale`** (Custom) → **`stretch`** mode →
**default `stretch=stddev` with `sigma=2.0`** when nothing is passed. `u8`
RGB keeps its 0–255 fast path and is never auto-stretched.

| Mode | Example | Range per band |
|---|---|---|
| Custom    | `?rescale=470,660&rescale=470,700&rescale=350,730` | exactly as given |
| MinMax    | `?stretch=minmax` | dataset `min … max` |
| Percent   | `?stretch=percent&pc=2,98` | 2nd … 98th percentile (QGIS cumulative cut) |
| StdDev    | `?stretch=stddev&sigma=2.5` | `mean ± 2.5·σ` (clamped to min/max) |
| *(none)*  | `?` (no stretch params) | ≡ `stretch=stddev&sigma=2.0` |

Statistics come from the COG's embedded `STATISTICS_*` GDAL tags when
present (**exact**), otherwise from a one-shot read of the highest
overview (**estimated**, computed once per dataset and memoized). For
accurate ranges on tag-less COGs, write stats offline (e.g.
`gdal_edit.py -stats file.tif`). Invalid params (`?stretch=bogus`,
`?pc=2`, `?sigma=0`, or `pc`/`sigma` without `stretch=`) return HTTP 400.

## Supported TileMatrixSets

- `WebMercatorQuad` (EPSG:3857) — default for slippy XYZ clients
- `WorldCRS84Quad` (EPSG:4326)

## TMS supported pixel types

Source COG can be U8 / U16 / I16 / U32 / I32 / F32 / F64. Internally
unified to F64 at the read/warp seam.

## Build requirements

- Rust ≥ 1.88, matching `Cargo.toml`'s `rust-version`.
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
  ├─ render pipeline:                   ndarray + palette
  │    ├─ rescale per band
  │    ├─ color_formula (sRGB Lab/Lch)
  │    └─ composite (LUT for gradient / HashMap for discrete / per-band rgb)
  └─ encode PNG / WebP / JPEG           image (+ image-webp)
```

Out-of-range tiles return `Ok(None)` (the example server replies HTTP 204);
WebP/JPEG output is gated behind the `webp` / `jpeg` cargo features.

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
  render/             colour, colormap, color_formula, composite, encode, pipeline
  style/              titiler-style params parser (`TileStyle`) + compiler
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
