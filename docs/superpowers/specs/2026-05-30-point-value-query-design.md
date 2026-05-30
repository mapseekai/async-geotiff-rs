# Point Value Query — `CogReader::point()` design

**Date:** 2026-05-30
**Status:** Approved (design); ready for implementation planning

## Goal

Add a library method that, given a coordinate, returns the COG's pixel value(s)
at that location. Supports multi-band rasters and an optional 1-indexed band
selection. This is the equivalent of titiler's `/point` endpoint, as a reusable
`CogReader` method (no HTTP endpoint in this scope).

## Surface area (decided)

- A single async method on `CogReader`. No HTTP route, no mosaic variant in this
  scope (both can build on this method later).

## API

New file `src/io/point.rs` (keeps `cog.rs`, already ~700 lines, from growing).
The method is an `impl CogReader` block in that file; the result type lives there too.

```rust
/// Pixel value(s) sampled at a coordinate.
#[derive(Debug, Clone, PartialEq)]
pub struct PointValue {
    /// One value per requested band, in `bidx` order (or all bands in native
    /// order when `bidx` is empty). Raw decoded values cast to f64 — scale/
    /// offset is NOT applied.
    pub values: Vec<f64>,
    /// `false` when the pixel is masked (alpha/mask band) or any returned band
    /// equals the dataset nodata sentinel; `true` otherwise.
    pub valid: bool,
}

impl CogReader {
    /// Sample the full-resolution (level 0) pixel value(s) at `(x, y)`.
    ///
    /// `coord_crs` is the CRS of the input coordinate (pass `Crs::epsg(4326)`
    /// for lon/lat). The coordinate is reprojected to the dataset's source CRS
    /// internally (skipped when the CRSs are equal).
    ///
    /// `bidx` is a 1-indexed band selection (matching the crate's `TileStyle`
    /// convention). An empty slice returns all bands in native order.
    ///
    /// Returns `Ok(None)` when the coordinate falls outside the dataset extent
    /// or outside the projection's valid domain. Returns `Err(AgError::query)`
    /// when a band index is 0 or exceeds the band count.
    pub async fn point(
        &self,
        x: f64,
        y: f64,
        coord_crs: &Crs,
        bidx: &[u32],
    ) -> AgResult<Option<PointValue>>;
}
```

## Decisions (resolved during brainstorming)

| Topic | Decision |
|-------|----------|
| Surface | Library method on `CogReader` only |
| Input CRS | `(x, y)` + `coord_crs` (caller passes 4326 for lon/lat); reproject to source CRS |
| Resolution | Full resolution (level 0) always — a point value reflects true source data |
| Band selection | `bidx: &[u32]`, 1-indexed; empty = all bands in order; returns `Vec<f64>` |
| Value semantics | Raw values + a `valid` flag; **no** unscale (scale/offset not applied) |
| Out of bounds | `Ok(None)` for outside-extent or projection-domain; `AgError::query` only for a bad band index |

## Data flow

1. **Reproject.** If `coord_crs != self.src_crs`, run `Projector::new(coord_crs,
   self.src_crs)?.convert(&mut [(x, y)])`. A `AgError::ProjectionDomain` →
   `Ok(None)`; other projection errors propagate. If the CRSs are equal, use
   `(x, y)` as-is (mirrors `wgs84_bounds`).
2. **Coord → pixel.** `self.transform.inverse()` (→ `AgError::Warp` if not
   invertible), then `inv.forward(src_x, src_y)` gives fractional `(col, row)`
   at level 0.
3. **Bounds check.** Floor `(col, row)` to integers. If `col < 0 || row < 0` or
   `col >= width || row >= height` (from `level_dimensions(0)`) → `Ok(None)`.
4. **Validate `bidx`.** Let `n = samples_per_pixel` of the level-0 data IFD.
   For each `b` in `bidx`: `b == 0` or `b > n` → `AgError::query("bidx", …)`.
   Resolve to 0-indexed selection; empty `bidx` → `0..n`.
5. **Read.** `read_window(0, col, row, 1, 1).await?` → `Arc<ImageData>` (1×1
   window; reuses the decode cache and single-flight coalescing).
6. **Extract.** `image.pixels.to_f64()` → take `[band, 0, 0]` for each selected
   band into `values`. Compute `valid`: start `true`; if `image.mask` is
   `Some(m)` and `m[[0,0]] == false` → `false`; if `image.nodata` is `Some(nd)`
   and any selected band's raw value equals `nd` (NaN-aware: `nd.is_nan()` →
   match NaN; else `(v - nd).abs() < 1e-9`) → `false`.
7. Return `Some(PointValue { values, valid })`.

## Components & reuse

All building blocks exist; no new dependencies:

- `Projector::new` / `convert` (`src/warp/projection.rs`) — reprojection.
- `GeoTransform::inverse` / `forward` (`src/bounds.rs`) — coord ↔ pixel.
- `CogReader::level_dimensions` / `read_window` / `data_ifd` (`src/io/cog.rs`).
- `ImageData.{pixels, mask, nodata}` + `PixelData::to_f64` (`src/pixel.rs`).
- `AgError::query` / `AgError::ProjectionDomain` (`src/error.rs`).

`PointValue` is re-exported from `src/io/mod.rs` (`pub use point::PointValue;`)
alongside the existing `CogReader`/`ReaderConfig`/`CacheConfig` exports. `point.rs`
is registered with `mod point;` (the `impl CogReader` block makes the method
available wherever `CogReader` is).

## Error handling

- Outside extent / projection domain → `Ok(None)` (expected, not an error).
- Band index 0 or > band count → `AgError::query` (HTTP 400 if surfaced).
- Non-invertible transform, projection failure (non-domain), I/O / decode
  errors from `read_window` → propagate the underlying `AgError`.

## Testing

Unit tests in `src/io/point.rs`, using the `LocalFileSystem` fixture pattern
from `src/io/cog.rs` / `src/io/stats.rs` tests against a `data/` COG:

1. **Known interior coordinate** returns the expected per-band value(s)
   (compare against values read by an independent tool / hardcoded truth for the
   fixture). Use `data/cog_uint8_rgb_nodata.tif` (3-band) or similar.
2. **`bidx` selection** — `bidx=[1]` returns exactly one value; empty returns
   all bands; `bidx=[3,1]` returns values in that order.
3. **Out-of-extent** lon/lat → `Ok(None)`.
4. **Bad band index** — `bidx=[0]` and `bidx=[99]` → `Err(AgError::Query{..})`.
5. **Nodata pixel** → `valid == false` (use the nodata fixture's known nodata
   location, or assert the flag toggles for a sentinel pixel).
6. **CRS reproject path** — a lon/lat query on a non-4326 COG hits a plausible
   interior pixel (reprojection is exercised, not bypassed).

## Out of scope (deferred)

- HTTP `/point` endpoint in the serve example.
- Mosaic point query (across assets).
- Optional overview/zoom level selection (always level 0 here).
- `unscale` (physical-units) output — caller applies scale/offset if needed; the
  per-band `band_scale_offset` is already accessible on the reader.
- Multi-coordinate batch queries.
