# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While the version is `0.0.x` the public API is unstable; **breaking changes
can land in any release** and are called out explicitly under a
**⚠️ BREAKING** marker.

## [Unreleased]

- Upgrade to Raster Style SDK 0.4.0 and core 3.0/Q3. Tile and mosaic settings
  require exact versioned extension declarations and use `raster:tile` /
  `raster:mosaic`; Q3 carries them in `extension_fields`. Reject incomplete
  validation, unsupported extension versions, and all legacy tile/mosaic keys.

- Add minmax, percentile, stddev, histogram equalization and curve stretch to
  Raster Style and all three viewers. Statistical methods share a fixed full-footprint
  grid across tiles; fixed MosaicJSON collections support both merge stages.
- Fix viewer zoom controls covered by the toolbar and MapLibre Apply requests
  being dropped while source tiles load.

### ⚠️ BREAKING

- Require Rust 1.92 and adopt `raster-style` 0.4.0 (protocol 3.0/Q3) for style
  input. Use `TileStyle::from_json`, `from_raster_query` or `from_raster_style`.
  Remove the legacy query parser, mutable style fields, `Stretch`,
  `ColourDefinition`, `Colour`, `lut_from_stops` and `compile_validated` APIs.
- HTTP tile requests require Q3 `version=3.0&type=...`. Old and unknown
  parameters fail validation. The default is gray band 1 with a fixed 0–255
  range; there is no implicit statistics-based stretch or metadata unscale.
- Mosaic selection now belongs to the style document. Call
  `mosaic.tile(coord, &style)` or `tile_with_options(coord, &style, &options)`.
  Support five selection methods, before/after-channel stages and explicit
  ranking channels with whole-tuple validity. `Tiler::warp_tile` returns compact
  dependencies or evaluated channels instead of all source bands.
- Reject unsupported protocol capabilities explicitly, including source Alpha,
  external statistics snapshots, nontrivial calibration, terrain rendering, advanced
  expressions/effects, JPEG and lossy WebP. The low-level JPEG encoder remains
  available separately. Align HTTP, viewers, Rust examples and documentation
  with the supported JSON/Q3 contract.

### Fixed

- Recover tile/warp/mosaic requests from transient working-memory contention:
  unwind the failed attempt, queue behind active requests, and retry once with
  exclusive admission under the original deadline. Keep the 512 MiB working
  budget, request-count limit, and nonblocking nested reservations unchanged.
  Cancellation retains the memory-admission guard until any running blocking
  job actually exits. Intrinsically oversized requests still fail.

## [0.11.0] - 2026-09-20

### ⚠️ BREAKING

- Upgrade `async-tiff` to 0.4.0 and `object_store` to 0.14. Callers passing
  object stores or async-tiff readers must use these compatible dependency
  versions. The direct HTTP client dependency is now `reqwest` 0.13.

### Added

- `PixelData::into_f64` consumes pixel data and reuses existing F64 storage.

### Changed

- Borrow F64 arrays with `Cow` in mosaic reduction, statistics and fallback
  stretching; copy only when mutation or retained ownership requires it.
- Reuse uniquely owned F64 buffers during unscale and move masks and CRS
  metadata instead of cloning them.
- Share TIFF metadata with prefetch tasks and inspect overview IFDs directly,
  avoiding copies of complete tile-offset and byte-count tables.
- Move Snapshot response metadata, attributes and extensions while preserving
  bounded file-range reads and working-memory accounting.
- Deduplicate MosaicJSON URLs as borrowed strings before allocating asset
  references, preserving first-occurrence paint priority.

### Fixed

- Keep the lockfile compatible with Rust 1.88 by selecting `crc-fast` 1.9.0;
  version 1.10.0 requires Rust 1.89.

## [0.10.1] - 2026-09-14

### Fixed

- Read `First` mosaic assets sequentially and stop as soon as coverage is
  complete, avoiding unnecessary remote reads and working-memory reservations.
- Limit MapLibre image requests to two concurrent loads so online STAC
  viewports do not overwhelm the server's bounded working-memory budget.
- Preserve MosaicJSON asset priority when deduplicating URLs, including prefix
  lookups, so `First` follows the index's paint order.
- Apply Mosaic band selection before warp, unscale and reduction; selected-band
  nodata and scale/offset metadata now follow the requested order. Reindex the
  reduced image for final rendering without selecting source bands twice.
- Give passthrough warp results the canonical destination grid so tiny source
  alignment differences do not reject otherwise compatible mosaic assets.
- Retain request admission through cancelled blocking decode, stitch, statistics,
  DNS and HTTP initialization jobs. Skip queued work after cancellation and let
  healthy block-cache waiters retry an expired owner's claim.
- Respect fractional nodata and strided validity masks in RGB fast paths, and
  match infinite nodata in rendering, direct colour mapping and point queries.

### Changed

- Replaced the style compiler's debug-string cache key with a typed key that
  covers every `TileStyle` field and preserves exact floating-point bit
  patterns, so cache identity does not depend on debug formatting.

## [0.10.0] - 2026-07-26

### ⚠️ BREAKING

- Removed the built-in typed spectral-index API (`Index` and its predefined
  indices). Use the arithmetic `expression` style parameter instead; this keeps
  band math extensible without growing a fixed list of index variants.

### Added

- Arithmetic band-math expressions through the `expression` query parameter.
  Expressions use 1-indexed `b<N>` references, decimal literals, parentheses,
  `+`, `-`, `*`, `/`, and unary minus; multiplication/division bind more
  tightly and same-precedence operators are left-associative. The result is a
  synthetic single band that can be rescaled and colorized.
- Expression rendering semantics and validation: referenced nodata, division
  by zero, and other non-finite results become transparent pixels; `expression`
  is mutually exclusive with `bidx` and `color_formula` and accepts at most one
  `rescale` range. Expression size, token-count, nesting, and missing-band
  cases are bounded and return query errors.
- Band-math examples in the MapLibre, OpenLayers, and Leaflet viewers,
  including expression-aware URL permalinks and state restoration.
- Type-state validation for tile styles, IFD metadata, and mosaic reducers,
  making invalid intermediate states harder to construct while preserving the
  existing tile-style compilation entry point.

- Optional STAC-backed Mosaic demo for `examples/serve`, enabled only when the
  existing `stac` feature and all three startup settings are present:
  `ASYNC_GEOTIFF_STAC_URL`, `ASYNC_GEOTIFF_STAC_COLLECTION`, and
  `ASYNC_GEOTIFF_STAC_ASSET_KEY`.
- Offline end-to-end coverage that starts a local mock STAC API and serves a
  repository COG fixture, verifying the STAC request, returned asset, and a
  non-empty rendered tile without external network access.

### Changed

- The MapLibre viewer defaults to mosaic when a MosaicJSON or STAC source is
  configured, keeps the normal single-COG demo unchanged otherwise, and no
  longer lets local dataset discovery override mosaic mode.
- The example viewers use blank basemaps by default, so the demo does not
  depend on external map-tile providers.
- Mosaic and tile hot paths reduce allocations and blocking handoffs, reuse
  native pixel layouts during resize, stream `mean`, use quickselect for
  `median`, and track `First` reducer completion incrementally. Shared CPU
  limiting now covers reader, warp, render, and mosaic reduction work.
- Remote tile responses from the example server advertise one-hour immutable
  browser caching for a fixed URL and style.
- Mosaic tile fan-out is bounded by `MosaicConfig::max_assets_per_tile` (64 by
  default), and oversized or invalid read windows are rejected before they can
  cause unbounded allocation or remote work.

### Security

- STAC endpoint, collection, and asset selection are fixed at process startup;
  tile requests cannot supply arbitrary remote STAC configuration.
- HTTP-backed mosaic assets are checked by the new `HttpAccessPolicy` before
  opening. The default policy blocks loopback and link-local destinations,
  `strict()` also blocks private ranges and requires HTTPS, and `permissive()`
  is available only for operator-controlled asset URLs. Credential-bearing URL
  query strings remain redacted from mosaic errors.

### Fixed

- Expression rendering now preserves finite numeric literals and masks
  referenced nodata, including infinite nodata sentinels, instead of allowing
  invalid values to become visible pixels.
- Rendering and resizing paths now handle non-standard array layouts and
  malformed or empty dimensions with structured errors rather than panics or
  wrapped allocations.

## [0.0.9] - 2026-06-11

### ⚠️ BREAKING

- `tms::TileCoord` replaces the positional `(tile_matrix_set, z, x, y)`
  parameter group: `Tiler::tile` / `Tiler::warp_tile`, `MosaicTiler::tile`,
  and the `MosaicSource::assets_for_tile` trait method now take a single
  `TileCoord { tms, z, x, y }`. Construct it with a struct literal so `x`/`y`
  are named at the call site and cannot be swapped silently.
- `CogReader::band_scale_offset()` now returns `&[io::ScaleOffset]`
  (named `scale`/`offset` fields with `apply()` / `IDENTITY`) instead of
  `&[(f64, f64)]`.

### Changed

- Added type-state boundaries for validated tile styles, validated IFD stitch
  metadata, and non-empty mosaic reducers so invalid intermediate states are
  represented in Rust types instead of late runtime checks.

### Added

- `CogReader::builder()` — builder-style construction with setters mirroring
  `ReaderConfig` (`source_id`, `block_cache`, `cache_config`, `cpu_limiter`,
  `max_decode_tasks`, `prefetch_ring`, `reader_config`, `client_options`) and
  async terminal methods `open_http(url)`, `open_object_store(store, key)`,
  and `open(reader)`.
- `MosaicTiler::builder(source)` — builder with `.config(cfg)`,
  `.block_cache(cache)`, and `.backend(backend)`; covers combinations the old
  constructors could not express (e.g. object_store + shared block cache +
  custom `MosaicConfig`).
- `mosaic::Backend` — public, `#[non_exhaustive]` enum selecting how mosaic
  assets are opened: `Http` (default) or `Store(Arc<dyn ObjectStore>)`.

### Deprecated

- `MosaicTiler::with_config` / `with_block_cache` / `with_object_store` /
  `with_object_store_config` / `with_object_store_and_block_cache` — use
  `MosaicTiler::builder(source)`.
- `CogReader::open_http_with_options` / `open_http_with_cache` /
  `open_http_with_config_default` / `open_http_with_config` /
  `open_with_reader_and_cache` / `open_with_reader_and_config` /
  `open_object_store_with_cache` / `open_object_store_with_config` — use
  `CogReader::builder()`. The zero-config shortcuts `open_http`,
  `open_with_reader`, and `open_object_store` remain.

## [0.0.8] - 2026-06-10

### Added

- Object-store backed COG opening for mosaic assets, including injected `object_store`
  support for MinIO/S3 deployments and updated server configuration examples.
- `perf` timing helpers plus a reproducible tile benchmark script and benchmark
  report covering cache capacity, output format cost, decode concurrency, and
  CPU profile hotspots.

### Changed

- Raised the default decoded block cache budget to 512 MB and documented
  practical cache sizing, ZSTD COG creation, tile concurrency, and in-flight
  single-flight behavior.
- Optimized mosaic and tile hot paths by reducing allocations, inlining
  `read_window` stitching, fusing CPU handoffs, and adding integer/LUT render
  fast paths for common stretch and color-ramp cases.
- Updated mosaic reader pooling so assets opened through injected object stores
  share cache and CPU-limiter behavior with HTTP-backed readers.

### Fixed

- Redacted URL credentials from mosaic asset errors and log output.
- Made canceled foreground block owners retryable for concurrent waiters instead
  of surfacing cancellation as a tile failure.

## [0.0.7] - 2026-06-05

### Added

- WebP and JPEG tile output, behind the `webp` and `jpeg` cargo features (PNG is
  always available). The output format is chosen by the tile **path extension**
  (`.png` / `.webp` / `.jpg`, titiler-style); an optional `format=` query param,
  if given, must agree with the extension. WebP is lossless (pure-Rust
  `image-webp`); JPEG is quality 75 with transparency flattened over black
  (JPEG has no alpha channel). The example server gains a `GET /formats`
  endpoint that reports only the formats the running build can encode, and the
  MapLibre / OpenLayers / Leaflet viewers gain a Format switcher populated from
  it (so an unavailable format is never offered).
- `CogReader::open_http_with_config_default(url, ReaderConfig)` — open a COG over
  HTTP with a full `ReaderConfig` (to inject a shared block cache + source id)
  while keeping the default tuned client options.
- Observability hooks for metrics/monitoring. `BlockCache::stats() -> BlockCacheStats`
  snapshots `{ hits, misses, decoded, fetched, entries, resident_bytes }` — poll it
  to export cache hit ratio, decode/fetch counts (incl. fetch amplification), and
  resident memory vs the budget. `Tiler::tile` and `CogReader::read_window` carry
  `#[tracing::instrument]` spans (with source/tile/level/format fields). The library
  emits `tracing` only and takes no OpenTelemetry dependency; bridge it in your app
  via `tracing-opentelemetry` and poll `stats()` into OTel gauges/counters.
- `MosaicTiler::with_block_cache(source, Arc<BlockCache>)` and
  `MosaicTiler::block_cache()` — inject / read the mosaic reader pool's shared
  block cache, so the mosaic path is observable (and can share one budget with
  your other readers) instead of owning a hidden internal cache.
- `CpuLimiter` (re-exported at the crate root) — a cloneable, shared bound on
  concurrent CPU-heavy `spawn_blocking` jobs (decode, stitch, warp, render).
  Inject one via `ReaderConfig::cpu_limiter` so a whole process — or a whole
  mosaic — draws from a single CPU budget instead of `N × available_parallelism`
  independent semaphores. `ReaderPool` shares one limiter across every asset
  reader and the final composite render, so a many-asset mosaic no longer
  oversubscribes the machine.

### Changed

- ⚠️ **BREAKING** — `ReaderConfig` is now `#[non_exhaustive]` and gained an
  `Option<CpuLimiter>` field. Construct it via `ReaderConfig::default()` and
  assign fields (struct-literal construction from outside the crate no longer
  compiles); this makes future field additions non-breaking. When the limiter is
  `None` the reader sizes its own from `max_decode_tasks` (prior behaviour);
  inject `Some(..)` to share one CPU budget across readers/tilers.
- ⚠️ **BREAKING (behaviour)** — `PixelSelection::Highest` / `Lowest` now read
  **every** asset and take the global per-pixel extremum instead of
  short-circuiting once the tile was fully covered. The old behaviour could miss
  a higher/lower value carried by a not-yet-read overlapping asset; the new one
  matches rio-tiler (only `First` exits when filled). Overlapping mosaics get
  correct extrema at the cost of reading all assets.
- Concurrent requests that both miss the same cold block now **single-flight the
  network fetch across requests** (not just the decode), resolving the known
  limitation noted in the block-cache entry above: the first caller fetches; the
  others await its result through an in-flight registry. A dropped owner (fetch
  error / task abort) cancels waiters, which retry.
- Performance: mosaic `Mean` is computed as a streaming per-band sum/count
  (memory O(1) in the number of assets, was O(assets); results bit-identical);
  `Median` uses quickselect (`select_nth_unstable`, O(n)) instead of a full sort;
  `First`'s `is_done` is O(1) (incremental hole count, was a per-asset mask scan);
  and the render path converts only the **selected** bands to f64 (borrowing when
  the source is already f64) instead of cloning the whole image up front.
- ⚠️ **BREAKING** — `Tiler::tile` and `MosaicTiler::tile` now return
  `AgResult<Option<Bytes>>`. A tile fully outside the dataset (or the
  projection's valid domain) yields `Ok(None)` instead of a synthesised blank
  tile; the example server maps `None` to **HTTP 204 No Content**. This stops
  out-of-range JPEG tiles from rendering as opaque black (PNG/WebP were
  transparent; JPEG has no alpha). In-range all-nodata tiles still render
  normally. The internal `empty_tile` helper was removed.
- ⚠️ **BREAKING** — replaced the per-`CogReader` decoded-window cache with a
  shared, injected `Arc<BlockCache>` that caches decoded native TIFF blocks
  under **one global byte budget**. `ReaderConfig` now carries
  `source_id: String` and `block_cache: Arc<BlockCache>` (and no longer a
  `cache` field); build readers via the `open_*_with_config` /
  `open_*_with_cache` entry points. This bounds total decode memory across many
  datasets / mosaic assets to a single budget instead of `N × 256 MB`, and
  decoded blocks are reused across overlapping windows and across readers that
  share a `source_id`. `read_window` now probes the cache per native block,
  fetches only the misses (coalesced) and decodes them concurrently
  (single-flighted per block via the shared cache), then stitches; data and
  mask blocks are resolved in parallel. `MosaicTiler`'s `ReaderPool` injects one
  shared block cache (with a stable per-URL `source_id`) into every asset
  reader. Known limitation: concurrent requests that both miss the same cold
  block may each fetch its bytes once (decode is still single-flighted).
- ⚠️ **BREAKING** — `AssetRef` (the `MosaicSource` contract) now carries a
  caller-supplied `source_id: String`; the reader pool no longer invents an
  internal `asset-{n}` id. Set `source_id` to a stable identity for each COG
  (the built-in `MosaicJsonSource`/STAC sources use the asset URL) and reuse the
  **same** string when opening that COG via single-dataset `ReaderConfig` to
  share decoded blocks across the mosaic and single-dataset paths (given a
  shared `BlockCache`). `ReaderPool::get_or_open` gained a `source_id` argument.

## [0.0.6] - 2026-05-31

### Added

- `CogReader::point(x, y, coord_crs, bidx)` — sample the full-resolution pixel
  value(s) at a coordinate (titiler `/point` equivalent as a library method).
  Returns `Option<PointValue { values: Vec<f64>, valid: bool }>`. The input
  coordinate is given in `coord_crs` (pass `Crs::epsg(4326)` for lon/lat) and
  reprojected to the dataset's source CRS. `bidx` is a 1-indexed band selection
  (empty = all bands, in order); values are raw decoded samples (no scale/
  offset). `valid` is `false` when the pixel is masked or any returned band hits
  nodata (NaN-aware). Returns `Ok(None)` for coordinates outside the dataset
  extent or the projection's valid domain; `Err(AgError::query)` for a band
  index that is 0 or exceeds the band count.

### Fixed

- Hardened band-math expression parsing with byte and token ceilings so flat
  but very long query expressions cannot force unbounded lexing/parsing or
  per-pixel evaluation work. This complements the existing nesting-depth cap.

## [0.0.5] - 2026-05-30

### Added

- Band-math `expression` style param (e.g. `expression=(b5-b4)/(b5+b4)`): an arithmetic evaluator (`+ - * /`, parentheses, unary minus, decimal literals, 1-indexed `b<N>`) computes a synthetic single band that is then rescaled and colormapped. Division-by-zero / non-finite results and nodata in any referenced band render transparent. Mutually exclusive with `bidx` and `color_formula`; accepts at most one `rescale` range (#1).
- Typed `Index` spectral-index sugar (`Ndvi`, `Ndwi`, `Ndbi`, `Evi`, `Savi`) lowering to expression strings via `Index::to_expression()`.
- When `expression` is set without an explicit `rescale`, the stretch is derived from the computed index's own value range (not the source bands' raw statistics). Pass e.g. `rescale=-1,1` for a fixed normalized-index range.

### Known limitations

- Expression output is single-band; comma-separated multi-band expressions and functions (`sqrt`/`abs`/…) are not supported in this release. Numbers are plain decimals — scientific notation (`1e3`) is not accepted.

## [0.0.4] - 2026-05-30

### Added

- `unscale` style flag: applies per-band GDAL `SCALE`/`OFFSET` metadata (TIFF tag 42112) as `physical = raw * scale + offset` before rendering. Auto-stretch bounds are unscaled to match, so `unscale=true` renders correct colors without an explicit `rescale` (#4).
- `Resampling::Mode` (`?resampling=mode` / `?reproject=mode`): majority resampling for categorical/classification rasters. Downsample-only (degrades to nearest on upsample); ties resolve to the lowest value (#5).

### Known limitations

- `unscale` applies the single GDAL nodata sentinel through band 0's scale/offset. For multi-band COGs whose bands carry *different* scale/offset affines and rely on a value-based nodata sentinel, interior nodata in bands 1+ may not mask correctly. Single-affine and mask-IFD/alpha datasets are unaffected.

## [0.0.3] - 2026-05-30

### Added

- Multi-source **mosaic** rendering (feature `mosaic`, on by default). A
  `MosaicSource` trait + `AssetRef` define the asset-discovery extension point;
  `MosaicJsonSource` resolves tiles against a MosaicJSON document via quadkey
  lookup (preserving the document's metadata for crate consumers). A URL-keyed
  `CogReader` pool shares open readers across assets, and `MosaicTiler`
  orchestrates chunked parallel reads, pixel-selection reduction, and
  early-exit once a tile is fully covered. Mosaic tiling is explicit about its
  tile matrix.
- Pixel-selection reducer for mosaics with five NaN-safe strategies:
  `first`, `highest`, `lowest`, `mean`, and `median`.
- `StacSource` for STAC-API-backed asset discovery, behind feature `stac`
  (which implies `mosaic`).
- `Resampling::Average` box-mean kernel: a same-CRS box filter and a
  cross-CRS path that uses the warp Jacobian footprint, matching GDAL/titiler
  `average` (antialiased downsample, nearest on upsample). Selectable from all
  three viewers and via `?resampling=average` / `?reproject=average`.
- Re-export `ObjectReader` and the `object_store` crate from `async_geotiff::io`, plus `CogReader::open_object_store{,_with_cache,_with_config}` — open a COG from an existing `Arc<dyn ObjectStore>` (S3/MinIO, GCS, local) without depending on `async-tiff` directly.
- `terrain` builtin colormap (matplotlib stops: deep blue → cyan → green → sand → brown → white), in addition to the `colorous` set.
- Inline `colormap=` JSON on `TileStyle`: discrete value maps (`{"1":[r,g,b,a]}`) and gradient breaks (`[[v,[r,g,b,a]]]`), routed by JSON shape and reusing the existing `ColourDefinition` machinery. Mutually exclusive with `colormap_name`; empty/degenerate maps return HTTP 400.

### Fixed

- Non-ASCII (CJK, etc.) dataset filenames returned HTTP 500. The loopback
  `/data/<name>` URL was percent-encoded once by the `url` crate, then
  `object_store::path::Path::from` re-encoded the `%` (double-encoding), so
  the fetch 404'd. Now uses `Path::from_url_path` (the decoding constructor)
  so the name round-trips. ASCII names were unaffected.
- Hardened request and file-header edge cases: invalid window dimensions,
  overflowing window endpoints, zero tile/image dimensions, and non-RGBA PNG
  buffers now return structured errors instead of panicking or producing
  wrapped coordinates.
- Updated the resampler flat-slice fast paths with explicit standard-layout
  debug assertions and safety comments so future strided-view changes cannot
  accidentally rely on logical-width indexing.
- Replaced hand-rolled GDAL metadata XML scanning with `quick-xml`, and
  reject invalid color formula parameters such as non-positive gamma values.
- Fixed WebMercator inverse conversion to clamp out-of-extent `y` values to
  the supported latitude range, mirroring the forward projection clamp.

### Changed

- Mask and data tile reads are fetched concurrently for masked COGs, decoded
  tile registries are shared across decode jobs, and Average reprojection
  precomputes row coordinate grids to avoid repeated per-row allocations.
- Empty transparent tile caching now handles poisoned locks as errors instead
  of panicking, `resize_bands` no longer carries an unused `Resizer` parameter,
  and duplicate window-copy clipping logic is centralized.
- `colormap_name` now rejects multiple `rescale` ranges instead of silently
  ignoring all but the first range.
- Removed the unused `render::rescale` module; pipeline rendering remains the
  single implementation path for rescaling.
- Extracted a `warp_tile` (read + warp) step out of the single-source tiler so
  the mosaic path can reuse it; single-source behavior is unchanged.
- **⚠️ BREAKING** `TileStyle` is now `#[non_exhaustive]`; construct it with `..TileStyle::default()` (external crates can no longer use a bare struct literal).

## [0.0.2] - 2026-05-16

### Added

- QGIS-style raster stretch modes via query params:
  `?stretch=minmax|percent|stddev`, with `?pc=lo,hi` (default `2,98`) and
  `?sigma=k` (default `2.0`). Explicit `?rescale=` (Custom) still wins.
- Automatic per-dataset statistics: exact from embedded GDAL `STATISTICS_*`
  tags when present, otherwise estimated once from the highest overview and
  memoized for the dataset's lifetime (`?rescale=` requests never trigger it).
- Stretch and Tile-size controls in the MapLibre, OpenLayers, and Leaflet
  example viewers.
- README "Data stretching" section documenting modes, priority, and the
  exact/estimated statistics provenance.

### Changed

- **⚠️ BREAKING (behavior):** non-uint8 COGs (UInt16/Int16/Float) requested
  **without** `?rescale=` no longer render saturated white — they now
  auto-stretch using `stretch=stddev` with `sigma=2.0`. To restore the
  previous exact output, pass an explicit `?rescale=min,max` per band. `u8`
  RGB is unaffected (it keeps its 0–255 fast path and is never auto-stretched).
- **⚠️ BREAKING (library API):** the stretch mode enum is named `Stretch`
  (it was briefly `StretchSpec` during development). `TileStyle` gained
  `stretch_kind` / `stretch_pc` / `stretch_sigma` fields and a
  `resolved_stretch()` method; the moka style-cache key is unaffected.
- Invalid stretch params (`?stretch=bogus`, `?pc=2`, `?sigma=0`, or
  `pc`/`sigma` without `stretch=`) now return HTTP 400 instead of being
  silently ignored.

## [0.0.1] - 2026-05-14

### Added

- Initial MVP: pure-Rust async COG reader serving XYZ map tiles with a
  titiler-style URL parameter subset (`bidx`, `rescale`, `nodata`,
  `colormap_name`, `color_formula`, `resampling` [`nearest`/`bilinear`],
  `tilesize`).
- TileMatrixSets: `WebMercatorQuad` (EPSG:3857) and `WorldCRS84Quad`
  (EPSG:4326). Source pixel types U8/U16/I16/U32/I32/F32/F64.
- PNG output; example axum server with MapLibre / OpenLayers / Leaflet
  viewers and a local `data/*.tif` dataset picker.

[Unreleased]: https://github.com/mapseekai/async-geotiff-rs/compare/v0.11.0...HEAD

[0.11.0]: https://github.com/mapseekai/async-geotiff-rs/releases/tag/v0.11.0

[0.10.1]: https://github.com/mapseekai/async-geotiff-rs/releases/tag/v0.10.1

[0.10.0]: https://github.com/zwishing/async-geotiff-rs/releases/tag/v0.10.0

[0.0.9]: https://github.com/zwishing/async-geotiff-rs/releases/tag/v0.0.9

[0.0.8]: https://github.com/zwishing/async-geotiff-rs/releases/tag/v0.0.8
[0.0.7]: https://github.com/zwishing/async-geotiff-rs/releases/tag/v0.0.7
[0.0.2]: https://github.com/zwishing/async-geotiff-rs/releases/tag/v0.0.2

<!-- 0.0.1 predates tagging; no release tag exists. -->
