# Average Resampling — Design Spec

Date: 2026-05-16
Status: Approved (brainstorming), pending implementation plan

## Goal

Add an `average` resampling kernel to `async-geotiff`, behaviour-compatible
with titiler / rio-tiler / gdalwarp `resampling=average`: when downsampling,
a destination pixel's value is the mean of the source pixels it covers.
Primary use case: overview / thumbnail generation for continuous-value data
(DEM, reflectance) where box-mean avoids the aliasing of point samplers.

## Non-Goals

- Per-pixel bit-exact parity with gdalwarp. The existing scalar cross-CRS
  path already collapses non-nearest kernels to bilinear on the rationale
  that PROJ reprojection error dominates kernel choice. Average follows the
  same philosophy: correct box-mean semantics, not reference-tool lockstep.
- No new dependencies. `fast_image_resize` (already a dependency) supplies
  the box filter for the same-CRS path.

## Key Decisions (from brainstorming)

1. **Intent**: match titiler/GDAL `average` string + downsample semantics.
2. **Same-CRS resize path**: `Average` → `fast_image_resize`
   `ResizeAlg::Convolution(FilterType::Box)`.
3. **Scalar cross-CRS path**: real footprint mean (not a bilinear fallback).
4. **Footprint estimation**: neighbour-difference Jacobian (reuse the warp
   grid; no extra parameters threaded through).
5. **Upsample degeneracy** (footprint ≤ 1 source px): fall back to Bilinear.
6. **Mask/nodata**: average over valid pixels only; all-invalid → `fill`.
   Mirrors the existing reweighted-bilinear convention.
7. **Hard constraint**: zero added per-pixel cost for the 5 existing
   kernels — kernel dispatch is hoisted out of the pixel loop.

## Architecture & Components

### 1. Interface — `src/warp/resampler.rs`

- `Resampling` enum: add variant `Average`.
- `Resampling::parse`: add `"average" => Some(Self::Average)`.
- New method (separate from `sample_masked`, which stays byte-for-byte
  unchanged so the 5 existing kernels' hot path — including the
  contiguous-slice fast path from the prior optimization — is untouched):

  ```rust
  pub fn sample_average(
      self,
      view: ArrayView2<f64>,
      valid: Option<ArrayView2<bool>>,
      sc: f64, sr: f64,      // source-space centre of the dst pixel
      rx: f64, ry: f64,      // half-footprint (source px) in x / y
      fill: f64,
  ) -> f64
  ```

  Algorithm:
  - If `rx < 0.5 && ry < 0.5` (footprint ≤ 1px): delegate to
    `Resampling::Bilinear.sample_masked(view, valid, sc, sr, fill)` —
    reuses existing edge-clamp and mask logic.
  - Else: integer box `[floor(sc-rx), ceil(sc+rx)] ×
    [floor(sr-ry), ceil(sr+ry)]`, clamped to `[0,w-1]×[0,h-1]`. Sum
    every source pixel whose `valid` is true, divide by count.
    `count == 0` → `fill`.
  - Box accumulation uses the contiguous-slice fast path
    (`view.as_slice()` + flat indexing) with strided `view[[r,c]]`
    fallback for non-contiguous views.

### 2. Same-CRS resize path — `src/warp/reproject.rs`

Both `match resampling` → `ResizeAlg` tables (the two `resize_bands`
dispatch sites) gain:

```rust
Resampling::Average => fr::ResizeAlg::Convolution(fr::FilterType::Box),
```

Evaluated once per resize call, not per pixel — zero per-pixel impact.

### 3. Scalar cross-CRS path — `src/warp/reproject.rs`

- **Dispatch hoist**: `match resampling` once before the row/column loop,
  selecting the sampling strategy. Non-`Average` keeps generating the
  exact same machine code as today (the hard constraint).
- **Footprint (Jacobian via neighbour difference)**: in the column loop
  the source coord `(sc, sr)` is already computed per dst pixel. Add:
  - `dx = |sc(c+1) - sc(c)|`, `dy_col = |sr(c+1) - sr(c)|` (col step)
  - `dx_row = |sc(r+1) - sc(r)|`, `dy = |sr(r+1) - sr(r)|` (row step)
  - `rx = max(dx, dx_row) / 2`, `ry = max(dy, dy_col) / 2` — axis-aligned
    bounding box that also covers the skew component under rotation.
  - Edge columns/rows use one-sided difference.
  - `rx`/`ry` lower-clamped to a small positive epsilon to avoid a 0-area
    box; projection flips (decreasing `sc`) handled by `abs`.
- The `Average` branch calls `sample_average(view, mask, sc, sr, rx, ry,
  fill)`; all other kernels call `sample_masked` unchanged.

## Data Flow

```
style resampling="average"
  └─ Resampling::parse → Resampling::Average
       ├─ same-CRS:  reproject::resize_bands → ResizeAlg::Convolution(Box)
       └─ cross-CRS: reproject scalar loop
                       ├─ hoisted dispatch picks Average path
                       ├─ per-pixel: warp grid → (sc,sr); neighbour diff → rx,ry
                       └─ sample_average → box mean (mask-aware) or bilinear (≤1px)
```

## Error Handling / Edge Cases

- All source pixels in box invalid → `fill` (= `nodata` or 0.0).
- Footprint ≤ 1px → Bilinear delegation (smooth upsample, no pixel blocks).
- Tile-edge dst pixels → one-sided neighbour difference.
- Projection axis flip → `abs` on differences.
- Non-contiguous source view → strided `[[r,c]]` fallback inside the box.

## Testing

Unit (`resampler.rs` tests, `approx` style):
- `average_box_mean`: 4×4 known array, `rx=ry=1.5` → arithmetic mean.
- `average_respects_mask`: partial invalid → mean of valid only;
  all-invalid → `fill`.
- `average_degenerates_to_bilinear`: `rx=ry=0.3` → bit-equal to
  `Bilinear.sample_masked`.
- `parse_string`: `parse("average") == Some(Average)`.

Integration (existing `warp` test skeleton):
- 2× downsample of a known COG: `Average` vs `Bilinear` same shape;
  `Average` output within source min/max (box-mean invariant). No
  gdalwarp lockstep assertion.

Performance regression gate (`benches/resampler_bench.rs`):
- Add `average_downsample` case.
- Re-run existing `contiguous_fastpath{,_masked}` as a **control**;
  assert no regression vs current baseline — proves the
  "dispatch hoist = zero overhead" hard constraint empirically.

Verification checklist: `cargo test --lib warp` green, `cargo clippy`
zero warnings, bench control group no regression.
