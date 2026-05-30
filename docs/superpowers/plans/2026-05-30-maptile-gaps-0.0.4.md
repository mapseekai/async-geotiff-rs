# maptile Integration Gaps — 0.0.4 (`unscale` + `mode`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close maptile gaps #4 (`unscale` — apply GDAL band scale/offset) and #5 (`mode` resampling — categorical majority), and release `async-geotiff` 0.0.4.

**Architecture:** Two independent features.
- **#4 unscale**: `CogReader` reads per-band `SCALE`/`OFFSET` from `GDAL_METADATA` (TIFF tag 42112) at open, reusing the existing XML item parser. `TileStyle` gains `unscale: bool`. `Tiler::tile` applies `v*scale+offset` to the warped `ImageData` *before* render (post-warp is equivalent to post-decode because resampling is a convex combination), AND applies the same linear transform to the auto-stretch ranges in `effective_compiled` so stretch bounds (derived from raw-DN `STATISTICS_*`) stay consistent with unscaled pixels.
- **#5 mode**: `Resampling::Mode` (downsample-only majority, degrades to Nearest on upsample — categorical data must never interpolate). It has no `fast_image_resize` equivalent, so it bypasses both SIMD fast-paths in `warp()` and runs the scalar grid path via a new `sample_mode` box-majority kernel mirroring `sample_average`.

**Tech Stack:** Rust, `quick-xml` (already used for GDAL metadata in `src/io/stats.rs`), `ndarray`, `fast_image_resize` (bypassed for mode), `rayon`.

**Source spec:** `docs/superpowers/specs/2026-05-30-maptile-integration-gaps-design.md`.

**Conventions (follow them):**
- Tests in `#[cfg(test)] mod tests` at the bottom of each source file. Local fixtures in `data/`, opened via `LocalFileSystem::new_with_prefix(".")` + `Path::from("data/<f>")` (pattern: `src/io/stats.rs:398`).
- User-input errors → `AgError::query(field, msg)` (HTTP 400).
- `cargo clippy --all-targets -- -D warnings` must stay clean. Run one test: `cargo test <name> -- --exact`. Full lib: `cargo test --lib`.
- `TileStyle` is already `#[non_exhaustive]` (from 0.0.3); construct in tests with `TileStyle { field, ..TileStyle::default() }`.

**Verified code facts (do not re-derive):**
- `ImageData` (`src/pixel.rs:113`): `pub pixels: PixelData`, `pub mask: Option<Array2<bool>>`, `pub crs: Crs`, `pub transform: GeoTransform`, `pub nodata: Option<f64>`. `PixelData` (`:10`) has per-dtype variants and `to_f64() -> Array3<f64>` (`:47`) / `as_f64() -> Cow<Array3<f64>>` (`:63`). Planar layout `(bands, h, w)`.
- `Tiler::tile` (`src/tiler.rs:227`): `warp_tile` → `compile_cached` → `effective_compiled` → `render`. `effective_compiled` (`:88`) computes `ranges` from `dataset_stats` then sets `next.rescale`, `next.composite.vmin/vmax`.
- `dataset_stats` come from raw-DN `STATISTICS_*` (`src/io/stats.rs:209-212`).
- GDAL metadata XML item parser: the `items(xml) -> Vec<(name, sample, value)>` closure inside `parse_embedded_stats` (`src/io/stats.rs:156-197`). `ifd.gdal_metadata()` is the accessor (`:263`).
- `warp()` dispatch (`src/warp/reproject.rs:52`): same-CRS+in-bounds → `warp_affine` (fr SIMD, returns early `:94`); else cross-CRS grid → `try_subtile_affine` (fr SIMD, returns early `:135`) → scalar fallback (`:149+`) where `is_average` (`:194`) computes a Jacobian footprint and calls `sample_average`.
- Two `match resampling { … }` → `fr::ResizeAlg` blocks at `reproject.rs:401` and `:532` (in `warp_affine` and the subtile path) — both must gain a `Mode` arm.
- `Resampling` (`src/warp/resampler.rs:6`), `parse` (`:28`), `sample_masked` match (`:101`, groups `Nearest` separately from the interpolating kernels incl. `Average`), `sample_average` (`:179`).

---

## File Structure

| File | Change | For |
|------|--------|-----|
| `src/io/scale_offset.rs` | Create | Parse `SCALE`/`OFFSET` per band from GDAL_METADATA XML |
| `src/io/mod.rs` | Modify | `mod scale_offset;` |
| `src/io/cog.rs` | Modify | Store `band_scale_offset: Vec<(f64,f64)>`; accessor; populate at open |
| `src/style/params.rs` | Modify | `TileStyle.unscale: bool` + parse arm |
| `src/tiler.rs` | Modify | Apply unscale to warped image + to stretch ranges |
| `src/warp/resampler.rs` | Modify | `Resampling::Mode` + `parse` + `sample_masked` arm + `sample_mode` |
| `src/warp/reproject.rs` | Modify | Bypass fr fast-paths for Mode; `is_mode` scalar branch; `ResizeAlg` arms |
| `Cargo.toml`, `CHANGELOG.md` | Modify | Release 0.0.4 |

---

# PHASE A — #4 `unscale`

## Task A1: Parse per-band SCALE/OFFSET from GDAL metadata

**Files:** Create `src/io/scale_offset.rs`; modify `src/io/mod.rs`.

- [ ] **Step 1: Register module.** In `src/io/mod.rs` add (with the other `mod` lines): `mod scale_offset;`

- [ ] **Step 2: Failing test (new file, tests first).** Create `src/io/scale_offset.rs`:

```rust
//! Per-band SCALE/OFFSET from a COG's `GDAL_METADATA` XML (TIFF tag 42112).
//!
//! GDAL writes `<Item name="SCALE" sample="0">0.0001</Item>` /
//! `<Item name="OFFSET" sample="0">0</Item>`. Bands without metadata default
//! to identity `(1.0, 0.0)`.

/// Returns `bands` entries of `(scale, offset)`, identity where absent.
pub fn parse_scale_offset(gdal_metadata: Option<&str>, bands: usize) -> Vec<(f64, f64)> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<GDALMetadata>
      <Item name="SCALE" sample="0">0.0001</Item>
      <Item name="OFFSET" sample="0">-0.1</Item>
      <Item name="SCALE" sample="1">2.0</Item>
    </GDALMetadata>"#;

    #[test]
    fn parses_per_band_scale_offset() {
        let so = parse_scale_offset(Some(XML), 3);
        assert_eq!(so.len(), 3);
        assert_eq!(so[0], (0.0001, -0.1));
        assert_eq!(so[1], (2.0, 0.0)); // offset absent → 0
        assert_eq!(so[2], (1.0, 0.0)); // band absent → identity
    }

    #[test]
    fn none_metadata_is_all_identity() {
        assert_eq!(parse_scale_offset(None, 2), vec![(1.0, 0.0), (1.0, 0.0)]);
    }

    #[test]
    fn malformed_xml_is_identity() {
        assert_eq!(parse_scale_offset(Some("<not xml"), 1), vec![(1.0, 0.0)]);
    }
}
```

- [ ] **Step 3: Run, expect fail.** `cargo test --lib scale_offset` → panics on `todo!()`.

- [ ] **Step 4: Implement.** Replace `todo!()` body. Reuse the SAME XML-walking shape as `parse_embedded_stats`'s inner `items` closure (`src/io/stats.rs:156-197`) — copy that closure verbatim into this module as a private `fn items(xml: &str) -> Vec<(String, usize, String)>`, then:

```rust
pub fn parse_scale_offset(gdal_metadata: Option<&str>, bands: usize) -> Vec<(f64, f64)> {
    let parsed = gdal_metadata.map(items).unwrap_or_default();
    let get = |name: &str, b: usize| -> Option<f64> {
        parsed
            .iter()
            .find(|(n, s, _)| n == name && *s == b)
            .and_then(|(_, _, v)| v.parse::<f64>().ok())
    };
    (0..bands)
        .map(|b| (get("SCALE", b).unwrap_or(1.0), get("OFFSET", b).unwrap_or(0.0)))
        .collect()
}
```

Add the `items` fn (copied from stats.rs). NOTE: `items` is currently a nested closure inside `parse_embedded_stats`; copying it here duplicates ~40 lines. That duplication is acceptable for this task; do NOT refactor stats.rs in this task (separate concern). If you prefer, make `items` `pub(crate)` in stats.rs and re-use it — but only if that's a clean one-line visibility change; otherwise copy.

- [ ] **Step 5: Run, expect pass.** `cargo test --lib scale_offset` (3 pass). Then `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 6: Commit.** `git add src/io/scale_offset.rs src/io/mod.rs && git commit -m "feat(io): parse per-band SCALE/OFFSET from GDAL metadata"`

## Task A2: Store scale/offset on CogReader

**Files:** `src/io/cog.rs`.

- [ ] **Step 1: Failing test.** Add to a `#[cfg(test)]` block in `src/io/cog.rs` (a non-ignored test against a fixture; if no fixture has SCALE metadata, assert the identity default instead):

```rust
    #[tokio::test]
    async fn band_scale_offset_defaults_to_identity() {
        use crate::io::{CogReader, ObjectReader};
        use object_store::local::LocalFileSystem;
        use std::sync::Arc;
        let fs = Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        let r = CogReader::open_with_reader(ObjectReader::new(
            fs,
            object_store::path::Path::from("data/cog_uint8_rgb_nodata.tif"),
        ))
        .await
        .unwrap();
        // RGB fixture without SCALE/OFFSET metadata → identity per band.
        let so = r.band_scale_offset();
        assert!(so.iter().all(|&(s, o)| s == 1.0 && o == 0.0));
        assert_eq!(so.len(), 3);
    }
```

- [ ] **Step 2: Run, expect fail** (`no method band_scale_offset`).

- [ ] **Step 3: Add the field + accessor + populate.**
  - Add field to `struct CogReader` (after `alpha_bands`): `band_scale_offset: Vec<(f64, f64)>,`
  - In `open_with_reader_and_config` (the constructor that builds `Self {…}`, ~`cog.rs:173`), compute it from the full-res data IFD before the `Ok(Self{…})`:
    ```rust
    let band_scale_offset = {
        let bands = tiff
            .ifds()
            .get(data_ifds[0])
            .map(|ifd| ifd.samples_per_pixel() as usize)
            .unwrap_or(0);
        let md = tiff.ifds().get(data_ifds[0]).and_then(|ifd| ifd.gdal_metadata());
        crate::io::scale_offset::parse_scale_offset(md, bands)
    };
    ```
    Then add `band_scale_offset,` to the `Ok(Self { … })` initializer.
    NOTE: confirm the exact accessor names — `ifd.samples_per_pixel()` and `ifd.gdal_metadata()` are used in `src/io/stats.rs` (`:261,:263`); mirror those. `data_ifds` is the local `Vec<usize>` already computed at `:168`.
  - Add accessor in `impl CogReader`:
    ```rust
    /// Per-band `(scale, offset)` from GDAL metadata; identity `(1.0, 0.0)`
    /// where absent. Apply as `physical = raw * scale + offset` when unscaling.
    pub fn band_scale_offset(&self) -> &[(f64, f64)] {
        &self.band_scale_offset
    }
    ```

- [ ] **Step 4: Run, expect pass.** `cargo test --lib band_scale_offset_defaults_to_identity -- --exact`, then full `cargo test --lib`, then clippy.

- [ ] **Step 5: Commit.** `git add src/io/cog.rs && git commit -m "feat(io): expose per-band scale/offset on CogReader"`

## Task A3: `TileStyle.unscale` field + parse

**Files:** `src/style/params.rs`.

- [ ] **Step 1: Failing test.** Add to params.rs tests:

```rust
    #[test]
    fn unscale_parsed_from_query() {
        let s = TileStyle::from_query("unscale=true").unwrap();
        assert!(s.unscale);
        assert!(!TileStyle::default().unscale);
    }
```

- [ ] **Step 2: Run, expect fail** (no field `unscale`).

- [ ] **Step 3: Implement.**
  - Add field after `nodata` (or near the other bools): `pub unscale: bool,`
  - In `Default`: `unscale: false,`
  - In `apply_param` add an arm:
    ```rust
            "unscale" => self.unscale = matches!(v, "true" | "1" | "yes"),
    ```

- [ ] **Step 4: Run, expect pass** (`cargo test --lib unscale_parsed_from_query -- --exact`), then clippy.

- [ ] **Step 5: Commit.** `git add src/style/params.rs && git commit -m "feat(style): add unscale flag to TileStyle"`

## Task A4: Apply unscale in the Tiler (pixels + stretch ranges)

**Files:** `src/tiler.rs`. This is the integration task — read `effective_compiled` (`:88-137`) and `tile` (`:227-243`) fully before editing.

- [ ] **Step 1: Failing test.** Add to tiler.rs tests a unit test for the pixel transform helper you will add (a pure function is easier to test than the async tile path):

```rust
    #[test]
    fn unscale_image_applies_per_band_affine() {
        use crate::pixel::{ImageData, PixelData};
        use ndarray::array;
        // 2 bands, 1x2 each: band0=[10,20], band1=[1,2]
        let pixels = PixelData::F64(array![[[10.0, 20.0]], [[1.0, 2.0]]]);
        let img = ImageData {
            pixels,
            mask: None,
            crs: crate::crs::Crs::epsg(3857),
            transform: crate::bounds::GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 0.0, 0.0, -1.0]),
            nodata: Some(-9999.0),
        };
        let so = [(0.1, 1.0), (2.0, 0.0)];
        let out = unscale_image(&img, &so);
        let f = out.pixels.to_f64();
        assert_eq!(f[[0, 0, 0]], 10.0 * 0.1 + 1.0); // band0
        assert_eq!(f[[0, 0, 1]], 20.0 * 0.1 + 1.0);
        assert_eq!(f[[1, 0, 0]], 1.0 * 2.0); // band1
        // nodata unchanged (sentinel must not be transformed)
        assert_eq!(out.nodata, Some(-9999.0));
    }
```

NOTE: confirm `PixelData::F64` is a real variant (check `src/pixel.rs:10`). If the variant is named differently (e.g. `F64(Array3<f64>)` vs another), use the actual name. If there is no owned-F64 constructor, build via `PixelData::from`/the existing constructor used elsewhere.

- [ ] **Step 2: Run, expect fail** (`cannot find function unscale_image`).

- [ ] **Step 3: Implement `unscale_image`.** Add a free function in `src/tiler.rs` (module level):

```rust
/// Apply per-band `physical = raw * scale + offset`, returning a new F64
/// `ImageData`. `nodata` is left as-is (the sentinel is compared pre-unscale
/// at decode and is not a physical value to transform). Bands beyond
/// `scale_offset.len()` use identity.
fn unscale_image(img: &ImageData, scale_offset: &[(f64, f64)]) -> ImageData {
    use ndarray::Axis;
    let mut data = img.pixels.to_f64(); // owned Array3<f64> (bands, h, w)
    for (b, mut band) in data.axis_iter_mut(Axis(0)).enumerate() {
        let (scale, offset) = scale_offset.get(b).copied().unwrap_or((1.0, 0.0));
        if scale != 1.0 || offset != 0.0 {
            band.mapv_inplace(|v| v * scale + offset);
        }
    }
    ImageData {
        pixels: PixelData::F64(data),
        mask: img.mask.clone(),
        crs: img.crs.clone(),
        transform: img.transform,
        nodata: img.nodata,
    }
}
```

Add imports as needed (`use crate::pixel::PixelData;` if not present). Confirm `Crs` is `Clone` and `GeoTransform` is `Copy` (used elsewhere in tiler).

- [ ] **Step 4: Run, expect pass** (`cargo test --lib unscale_image_applies_per_band_affine -- --exact`).

- [ ] **Step 5: Wire into `tile()`.** In `Tiler::tile` (`:227`), after `warp_tile` returns `warped` and BEFORE `compile_cached`/`render`, insert:

```rust
        let warped = if style.unscale {
            unscale_image(&warped, self.reader.band_scale_offset())
        } else {
            warped
        };
```

(Place it right after the `let warped = match self.warp_tile(...) { … }` block so the unscaled image flows into both `effective_compiled` and `render`.)

- [ ] **Step 6: Fix the stretch-bounds trap in `effective_compiled`.** `effective_compiled` (`:88`) derives `ranges` from raw-DN `dataset_stats`. When `style.unscale` is set, the auto-stretch ranges must be unscaled too, or unscaled pixels render against DN bounds (silent wrong colors). After `ranges` is computed (the `let ranges = match self.dataset_stats()… ` block, ~`:118-129`) and before it is assigned into `next`, add:

```rust
        let ranges = if style.unscale {
            let so = self.reader.band_scale_offset();
            ranges
                .iter()
                .enumerate()
                .map(|(i, &(lo, hi))| {
                    // `bidx[i]` is the source band this range came from.
                    let b = bidx.get(i).copied().unwrap_or(i);
                    let (s, o) = so.get(b).copied().unwrap_or((1.0, 0.0));
                    (lo * s + o, hi * s + o)
                })
                .collect::<Vec<_>>()
        } else {
            ranges
        };
```

NOTE: `bidx` is the local `Vec<usize>` already computed in `effective_compiled` (`:108-116`). Confirm `ranges` and `bidx` are both in scope at the insertion point; if `ranges` is declared with `let ranges = …;` immediately before being used, shadow it as shown. The explicit-`rescale` path (`:94`, early `return`) is unaffected — a caller passing `unscale=true` with explicit `rescale` is responsible for giving physical-unit ranges.

- [ ] **Step 7: Failing test for the bounds fix.** Add an integration-style test asserting an auto-stretched `unscale=true` render uses unscaled bounds. This needs a fixture COG WITH scale metadata, which may not exist in `data/`. If none exists, instead add a focused unit test on the range-transform logic by extracting it into a tiny pure helper `fn unscale_ranges(ranges, bidx, so) -> Vec<(f64,f64)>` and testing that directly:

```rust
    #[test]
    fn unscale_ranges_applies_source_band_affine() {
        let ranges = vec![(0.0, 1000.0), (10.0, 20.0)];
        let bidx = vec![0usize, 1];
        let so = vec![(0.01, 5.0), (2.0, 0.0)];
        let out = unscale_ranges(&ranges, &bidx, &so);
        assert_eq!(out[0], (0.0 * 0.01 + 5.0, 1000.0 * 0.01 + 5.0));
        assert_eq!(out[1], (10.0 * 2.0, 20.0 * 2.0));
    }
```

Refactor Step 6's inline closure to call this `unscale_ranges` helper so the test exercises the real path. Run it (`-- --exact`), expect pass.

- [ ] **Step 8: Full suite + clippy.** `cargo test --lib` and `cargo clippy --all-targets -- -D warnings`. Both green.

- [ ] **Step 9: Commit.** `git add src/tiler.rs && git commit -m "feat(tiler): apply unscale to pixels and auto-stretch ranges"`

---

# PHASE B — #5 `mode` resampling

## Task B1: `Resampling::Mode` + parse + sample_masked + `sample_mode`

**Files:** `src/warp/resampler.rs`.

- [ ] **Step 1: Failing tests.** Add to resampler.rs tests:

```rust
    #[test]
    fn parse_mode() {
        assert_eq!(Resampling::parse("mode"), Some(Resampling::Mode));
    }

    #[test]
    fn mode_box_majority() {
        // 4x4: three 5s and rest 1s in the footprint → majority 5.
        let a = ndarray::array![
            [5.0, 5.0, 1.0, 1.0],
            [5.0, 1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0, 1.0]
        ];
        // footprint over top-left 2x2 (values 5,5,5,1) → majority 5
        let v = Resampling::Mode.sample_mode(a.view(), None, 0.5, 0.5, 1.0, 1.0, -1.0);
        assert_eq!(v, 5.0);
    }

    #[test]
    fn mode_subpixel_uses_nearest_not_bilinear() {
        // sub-pixel footprint must NOT interpolate (categorical).
        let a = ndarray::array![[0.0, 10.0], [0.0, 10.0]];
        let m = Resampling::Mode.sample_mode(a.view(), None, 0.5, 0.0, 0.3, 0.3, -1.0);
        let n = Resampling::Nearest.sample_masked(a.view(), None, 0.5, 0.0, -1.0);
        assert_eq!(m, n);
    }

    #[test]
    fn mode_ties_pick_lowest_value() {
        // two 1s and two 2s in footprint → tie → lowest (1).
        let a = ndarray::array![[1.0, 2.0], [2.0, 1.0]];
        let v = Resampling::Mode.sample_mode(a.view(), None, 0.5, 0.5, 1.0, 1.0, -1.0);
        assert_eq!(v, 1.0);
    }

    #[test]
    fn mode_all_invalid_returns_fill() {
        let a = ndarray::Array2::from_elem((2, 2), 5.0);
        let none_valid = ndarray::Array2::from_elem((2, 2), false);
        let v = Resampling::Mode.sample_mode(a.view(), Some(none_valid.view()), 0.5, 0.5, 1.0, 1.0, -7.0);
        assert_eq!(v, -7.0);
    }
```

- [ ] **Step 2: Run, expect fail** (no `Mode` variant / no `sample_mode`).

- [ ] **Step 3: Add the variant + parse.**
  - In `enum Resampling` add after `Average`:
    ```rust
        /// Majority (most-frequent) value over the source footprint a
        /// destination pixel covers — for categorical/classification rasters.
        /// Downsample-only: a sub-pixel footprint degrades to Nearest (never
        /// interpolates). Ties resolve to the lowest value. Matches GDAL/
        /// titiler `resampling=mode`.
        Mode,
    ```
  - In `parse`: add `"mode" => Some(Self::Mode),`
  - In `sample_masked`'s match (`:101`): add `Mode` to the **Nearest** arm (categorical → nearest), i.e. change `Self::Nearest =>` to `Self::Nearest | Self::Mode =>`. (Mode reaching the per-sample path means a non-box context; nearest is the only correct categorical behaviour there.)

- [ ] **Step 4: Add `sample_mode`.** Add a method mirroring `sample_average` (`:179-237`) but voting instead of averaging:

```rust
    /// Majority value over the source footprint `[sc±rx, sr±ry]`. Counts each
    /// valid value; returns the most frequent (ties → lowest value). Sub-pixel
    /// footprint degrades to Nearest (categorical data must not interpolate).
    /// Empty/all-invalid box returns `fill`.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn sample_mode(
        self,
        view: ArrayView2<f64>,
        valid: Option<ArrayView2<bool>>,
        sc: f64,
        sr: f64,
        rx: f64,
        ry: f64,
        fill: f64,
    ) -> f64 {
        let (h, w) = (view.shape()[0], view.shape()[1]);
        if w == 0 || h == 0 {
            return fill;
        }
        if !sc.is_finite() || !sr.is_finite() {
            return fill;
        }
        // Sub-pixel footprint => nearest (NOT bilinear — no interpolation).
        if rx < 0.5 && ry < 0.5 {
            return Self::Nearest.sample_masked(view, valid, sc, sr, fill);
        }
        let c_lo = (sc - rx).floor().max(0.0) as usize;
        let c_hi = ((sc + rx).ceil() as i64).clamp(0, w as i64 - 1) as usize;
        let r_lo = (sr - ry).floor().max(0.0) as usize;
        let r_hi = ((sr + ry).ceil() as i64).clamp(0, h as i64 - 1) as usize;

        // Tally counts; tie-break to the lowest value. Categorical rasters
        // have few distinct classes, so a small Vec of (value, count) is
        // cheaper and allocation-light vs hashing f64 (which isn't Hash/Eq).
        let mut tally: Vec<(f64, u32)> = Vec::new();
        for r in r_lo..=r_hi {
            for c in c_lo..=c_hi {
                let ok = match valid {
                    Some(m) => m[[r, c]],
                    None => true,
                };
                if !ok {
                    continue;
                }
                let val = view[[r, c]];
                if let Some(e) = tally.iter_mut().find(|(v, _)| *v == val) {
                    e.1 += 1;
                } else {
                    tally.push((val, 1));
                }
            }
        }
        // Most frequent; tie → lowest value.
        tally
            .into_iter()
            .reduce(|best, cur| {
                if cur.1 > best.1 || (cur.1 == best.1 && cur.0 < best.0) {
                    cur
                } else {
                    best
                }
            })
            .map(|(v, _)| v)
            .unwrap_or(fill)
    }
```

- [ ] **Step 5: Run, expect pass** (all five mode tests), then `cargo test --lib resampler`, then clippy. The compiler will now flag non-exhaustive matches in `reproject.rs` (the two `ResizeAlg` blocks) — that is expected and fixed in Task B2. If `cargo test --lib` fails to compile ONLY in reproject.rs on those matches, proceed to B2; the resampler.rs unit tests themselves should pass when run as `cargo test -p async-geotiff --lib resampler::` only if the crate compiles — so you may need B2's match arms in place to compile. If so, do Step 5's verification after B2 Step 2.

- [ ] **Step 6: Commit.** `git add src/warp/resampler.rs && git commit -m "feat(warp): add Mode (majority) resampling kernel"`

## Task B2: Route `Mode` through the scalar path in `warp()`

**Files:** `src/warp/reproject.rs`. Read `warp` (`:52-260`) and both `ResizeAlg` matches (`:401`, `:532`) first.

- [ ] **Step 1: Bypass the fr fast-paths for Mode.**
  - The same-CRS early return (`:83-106`): change the guard so Mode does NOT take it. Change `if &src.crs == dst_crs {` … `if fully_in_bounds {` so Mode skips `warp_affine`. Simplest: add `&& !matches!(resampling, Resampling::Mode)` to the `fully_in_bounds` condition (so Mode same-CRS falls through to the grid path, which for same-CRS still works — `convert_batch` is identity, then `try_subtile_affine` is also bypassed in the next bullet, reaching the scalar fallback).
  - The `try_subtile_affine` call (`:135`): guard it so Mode skips it: wrap the `if let Some(out) = try_subtile_affine(…)` in `if !matches!(resampling, Resampling::Mode) { if let Some(out) = … { return Ok(out); } }`. This forces Mode to the scalar fallback where the footprint is available.

- [ ] **Step 2: Add `Mode` arms to the two `ResizeAlg` matches** (`:401`, `:532`). Mode never reaches these (bypassed in Step 1), but the match must be exhaustive. Add defensively (NOT a continuous filter):
    ```rust
        Resampling::Mode => fr::ResizeAlg::Nearest, // unreachable: Mode bypasses fr
    ```
  (Using `Nearest` rather than `unreachable!()` avoids a panic risk if a future caller reaches it; the comment documents intent.)

- [ ] **Step 3: Add the `is_mode` scalar branch.** In the scalar fallback, mirror the `is_average` handling. After `let is_average = matches!(resampling, Resampling::Average);` (`:194`) add:
    ```rust
        let is_mode = matches!(resampling, Resampling::Mode);
    ```
  Then in the per-row parallel closure, the code currently branches `if is_average { … } else { … sample_masked … }`. Change it to a three-way branch: `if is_average { …existing… } else if is_mode { …mirror the is_average footprint block but call sample_mode… } else { …existing sample_masked… }`.

  The `is_mode` block is the `is_average` footprint block (`:205-261`, computing `rx`/`ry` from the row/column Jacobian) with the single call site `resampling.sample_average(*band_view, src_mask_view, sc, sr, rx, ry, fill)` replaced by `resampling.sample_mode(*band_view, src_mask_view, sc, sr, rx, ry, fill)`. Copy the `is_average` footprint-computation block verbatim and change only that one call. Do not change the Jacobian math.

- [ ] **Step 4: Failing test → behavioural.** Add a reproject.rs test that downsampling a categorical block via Mode preserves a class value rather than averaging it. Mirror the existing `Average` integration tests (`reproject.rs:755+`):

```rust
    #[test]
    fn mode_downsample_preserves_class_not_mean() {
        use crate::pixel::{ImageData, PixelData};
        // 4x4 single band: left half class 1, right half class 9.
        let a = ndarray::Array3::from_shape_fn((1, 4, 4), |(_, _, c)| if c < 2 { 1.0 } else { 9.0 });
        let src = ImageData {
            pixels: PixelData::F64(a),
            mask: None,
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 4.0, 0.0, -1.0]),
            nodata: None,
        };
        // Downsample 4x4 → 2x2, same CRS. Each output pixel covers a 2x2 block
        // that is pure class 1 or pure class 9 → no 5.0 (mean) may appear.
        let out = warp(&src, &Crs::epsg(3857), src.bbox, 2, 2, Resampling::Mode).unwrap();
        let f = out.pixels.to_f64();
        for v in f.iter() {
            assert!(*v == 1.0 || *v == 9.0, "mode produced a non-class value {v}");
        }
    }
```

NOTE: confirm `ImageData` has a `bbox` field/accessor used by the existing Average tests (`reproject.rs:792` uses `src.bbox`). If `bbox` is a method, call it. Match whatever the adjacent Average tests do.

- [ ] **Step 5: Run.** `cargo test --lib mode_downsample_preserves_class_not_mean -- --exact`, then `cargo test --lib`, then `cargo clippy --all-targets -- -D warnings`. All green.

- [ ] **Step 6: Commit.** `git add src/warp/reproject.rs && git commit -m "feat(warp): route Mode resampling through scalar majority path"`

---

# PHASE C — Release 0.0.4

## Task C1: version + CHANGELOG

**Files:** `Cargo.toml`, `CHANGELOG.md`.

- [ ] **Step 1.** `Cargo.toml`: `version = "0.0.3"` → `version = "0.0.4"`.
- [ ] **Step 2.** `CHANGELOG.md`: add a new section below the empty `## [Unreleased]`:
    ```markdown
    ## [0.0.4] - <today's date, YYYY-MM-DD>

    ### Added

    - `unscale` style flag: applies per-band GDAL `SCALE`/`OFFSET` metadata (TIFF tag 42112) as `physical = raw * scale + offset` before rendering. Auto-stretch bounds are unscaled to match, so `unscale=true` renders correct colors without an explicit `rescale` (#4).
    - `Resampling::Mode` (`?resampling=mode` / `?reproject=mode`): majority resampling for categorical/classification rasters. Downsample-only (degrades to nearest on upsample); ties resolve to the lowest value (#5).
    ```
- [ ] **Step 3.** Update `Cargo.lock`: run `cargo build` (rewrites the lock to 0.0.4).
- [ ] **Step 4.** Commit: `git add Cargo.toml Cargo.lock CHANGELOG.md && git commit -m "chore(release): 0.0.4 — unscale + mode resampling"`

---

## Final verification

- [ ] `cargo test` (full suite green)
- [ ] `cargo clippy --all-targets -- -D warnings` (clean)
- [ ] `cargo build --examples` (examples still compile — `#[non_exhaustive]` and the new `Resampling::Mode` variant must not break any example's `match` on `Resampling`; if an example or `serve` has an exhaustive `match Resampling { … }`, add a `Mode` arm there)
- [ ] `cargo doc --no-deps` (no NEW warnings beyond the 2 pre-existing unrelated ones)

## Notes for the engineer

- **`Resampling` is `#[derive(PartialEq, Eq)]`** — adding `Mode` is fine. Grep for `match` on `Resampling` across the whole crate (`src/`, `examples/`, `serve`) before finishing — the compiler will flag any non-exhaustive site; add a `Mode` arm (route to scalar/nearest semantics, never an interpolating filter).
- **Order independence of unscale vs warp** is intentional: resampling is a convex combination, so `unscale(warp(DN)) == warp(unscale(DN))`. Applying post-warp (Task A4) is equivalent and lets the reader keep caching raw-DN windows shared across requests.
- **Out of scope for 0.0.4** (0.0.5): band-math `expression` + `Index` enum (#1). Per the spec, expression must run on UNSCALED values — so it composes after this phase's unscale.
