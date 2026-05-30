# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While the version is `0.0.x` the public API is unstable; **breaking changes
can land in any release** and are called out explicitly under a
**⚠️ BREAKING** marker.

## [Unreleased]

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

[Unreleased]: https://github.com/zwishing/async-geotiff-rs/compare/v0.0.2...HEAD
[0.0.2]: https://github.com/zwishing/async-geotiff-rs/releases/tag/v0.0.2

<!-- 0.0.1 predates tagging; no release tag exists. -->
