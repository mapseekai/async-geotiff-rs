# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

While the version is `0.0.x` the public API is unstable; **breaking changes
can land in any release** and are called out explicitly under a
**⚠️ BREAKING** marker.

## [Unreleased]

### Added

- `Resampling::Average` box-mean kernel: a same-CRS box filter and a
  cross-CRS path that uses the warp Jacobian footprint, matching GDAL/titiler
  `average` (antialiased downsample, nearest on upsample). Selectable from all
  three viewers and via `?resampling=average` / `?reproject=average`.

### Fixed

- Non-ASCII (CJK, etc.) dataset filenames returned HTTP 500. The loopback
  `/data/<name>` URL was percent-encoded once by the `url` crate, then
  `object_store::path::Path::from` re-encoded the `%` (double-encoding), so
  the fetch 404'd. Now uses `Path::from_url_path` (the decoding constructor)
  so the name round-trips. ASCII names were unaffected.

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
