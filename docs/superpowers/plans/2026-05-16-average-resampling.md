# Average Resampling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an `average` resampling kernel (box-mean downsampling, titiler/GDAL-compatible) to `async-geotiff` across both the same-CRS resize path and the cross-CRS scalar warp path.

**Architecture:** New `Resampling::Average` variant. Same-CRS path maps it to `fast_image_resize`'s `Box` filter. Cross-CRS scalar path computes a per-pixel source footprint via neighbour-difference Jacobian and box-averages valid source pixels, falling back to bilinear when the footprint is ≤1 source pixel. Kernel dispatch is hoisted to row level so the 5 existing kernels keep byte-identical codegen, verified by a benchmark control group.

**Tech Stack:** Rust, ndarray, fast_image_resize, rayon, criterion, approx.

**Spec:** `docs/superpowers/specs/2026-05-16-average-resampling-design.md`

---

## File Structure

- `src/warp/resampler.rs` — add `Average` variant, `parse`, exhaustive-match fix, new `sample_average` method + unit tests.
- `src/warp/reproject.rs` — two `ResizeAlg` match arms; row-level dispatch + footprint computation + Average branch; integration test.
- `benches/resampler_bench.rs` — add `average_downsample` case; control group already present.

---

## Task 1: Add `Average` variant, `parse`, and fix exhaustive matches

**Files:**
- Modify: `src/warp/resampler.rs` (enum ~line 5-20, `parse` ~line 23-32, `sample_masked` match ~line 86-102)
- Modify: `src/warp/reproject.rs` (two `ResizeAlg` tables: ~line 324-329 and ~line 460-465)
- Test: `src/warp/resampler.rs` tests module (~line 166 `parse_string`)

- [ ] **Step 1: Write the failing test**

In `src/warp/resampler.rs`, extend the existing `parse_string` test:

```rust
    #[test]
    fn parse_string() {
        assert_eq!(Resampling::parse("bilinear"), Some(Resampling::Bilinear));
        assert_eq!(Resampling::parse("cubic"), Some(Resampling::Cubic));
        assert_eq!(Resampling::parse("average"), Some(Resampling::Average));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib warp::resampler::tests::parse_string`
Expected: compile error — no variant `Average`.

- [ ] **Step 3: Add the enum variant**

In `src/warp/resampler.rs`, add to the `Resampling` enum after `Lanczos`:

```rust
    /// Box mean over the source footprint a destination pixel covers.
    /// Downsampling antialiasing for continuous data; matches titiler /
    /// gdalwarp `resampling=average`.
    Average,
```

- [ ] **Step 4: Add parse arm**

In `Resampling::parse`, add before the `_ => None` arm:

```rust
            "average" => Some(Self::Average),
```

- [ ] **Step 5: Fix `sample_masked` exhaustive match**

In `sample_masked`, change the grouped kernel arm so `Average` is handled (behaves as bilinear if ever called directly; the scalar path routes Average to `sample_average` instead):

```rust
            Self::Cubic | Self::Bilinear | Self::CubicSpline | Self::Lanczos | Self::Average => {
```

- [ ] **Step 6: Fix both `ResizeAlg` tables in reproject.rs**

In `src/warp/reproject.rs`, in **both** `let alg = match resampling {` blocks (~line 324 and ~line 460), add after the `Lanczos` arm:

```rust
        Resampling::Average => fr::ResizeAlg::Convolution(fr::FilterType::Box),
```

- [ ] **Step 7: Run test to verify it passes**

Run: `cargo test --lib warp::resampler::tests::parse_string`
Expected: PASS. Also run `cargo build` — expected: compiles (all matches exhaustive).

- [ ] **Step 8: Commit**

```bash
git add src/warp/resampler.rs src/warp/reproject.rs
git commit -m "feat: add Resampling::Average variant and same-CRS Box filter"
```

---

## Task 2: Implement `sample_average` with box-mean and bilinear degeneracy

**Files:**
- Modify: `src/warp/resampler.rs` (add method to `impl Resampling`, after `sample_masked`)
- Test: `src/warp/resampler.rs` tests module

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/warp/resampler.rs`:

```rust
    #[test]
    fn average_box_mean() {
        // 4x4 ramp 0..16; full-tile footprint => arithmetic mean = 7.5
        let a = ndarray::Array2::from_shape_fn((4, 4), |(r, c)| (r * 4 + c) as f64);
        let v = Resampling::Average.sample_average(a.view(), None, 1.5, 1.5, 1.5, 1.5, -1.0);
        assert_relative_eq!(v, 7.5);
    }

    #[test]
    fn average_respects_mask() {
        let a = ndarray::Array2::from_elem((4, 4), 10.0);
        let mut m = ndarray::Array2::from_elem((4, 4), true);
        m[[0, 0]] = false; // one invalid pixel, value still 10 elsewhere
        let v = Resampling::Average.sample_average(
            a.view(), Some(m.view()), 1.5, 1.5, 1.5, 1.5, -1.0);
        assert_relative_eq!(v, 10.0); // mean of valid pixels (all 10)

        let none_valid = ndarray::Array2::from_elem((4, 4), false);
        let f = Resampling::Average.sample_average(
            a.view(), Some(none_valid.view()), 1.5, 1.5, 1.5, 1.5, -7.0);
        assert_eq!(f, -7.0); // all invalid => fill
    }

    #[test]
    fn average_degenerates_to_bilinear() {
        let a = array![[0.0, 10.0], [0.0, 10.0]];
        let avg = Resampling::Average.sample_average(a.view(), None, 0.5, 0.0, 0.3, 0.3, 0.0);
        let bil = Resampling::Bilinear.sample_masked(a.view(), None, 0.5, 0.0, 0.0);
        assert_eq!(avg, bil); // sub-1px footprint delegates to bilinear
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib warp::resampler::tests::average`
Expected: compile error — no method `sample_average`.

- [ ] **Step 3: Implement `sample_average`**

Add this method inside `impl Resampling`, immediately after `sample_masked`:

```rust
    /// Box-mean over the source footprint `[sc-rx, sc+rx] x [sr-ry, sr+ry]`.
    /// `rx`/`ry` are half-footprint sizes in source pixels (from the warp
    /// Jacobian). When the footprint is below one source pixel the result
    /// degenerates to bilinear (no pixel blocks on upsample). Averages only
    /// `valid` pixels; all-invalid (or empty box) returns `fill`.
    #[inline]
    pub fn sample_average(
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
        // Sub-pixel footprint => bilinear (reuses edge-clamp + mask logic).
        if rx < 0.5 && ry < 0.5 {
            return Self::Bilinear.sample_masked(view, valid, sc, sr, fill);
        }
        let c_lo = (sc - rx).floor().max(0.0) as usize;
        let c_hi = ((sc + rx).ceil() as i64).clamp(0, w as i64 - 1) as usize;
        let r_lo = (sr - ry).floor().max(0.0) as usize;
        let r_hi = ((sr + ry).ceil() as i64).clamp(0, h as i64 - 1) as usize;

        let contig = view.as_slice();
        let mask_contig = valid.as_ref().and_then(|m| m.as_slice());
        let mut sum = 0.0;
        let mut count = 0u32;
        for r in r_lo..=r_hi {
            for c in c_lo..=c_hi {
                let ok = match (mask_contig, valid) {
                    (Some(s), _) => s[r * w + c],
                    (None, Some(m)) => m[[r, c]],
                    (None, None) => true,
                };
                if !ok {
                    continue;
                }
                let val = match contig {
                    Some(s) => s[r * w + c],
                    None => view[[r, c]],
                };
                sum += val;
                count += 1;
            }
        }
        if count == 0 {
            fill
        } else {
            sum / count as f64
        }
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib warp::resampler::tests::average`
Expected: PASS (3 tests: `average_box_mean`, `average_respects_mask`, `average_degenerates_to_bilinear`).

- [ ] **Step 5: Run clippy**

Run: `cargo clippy --lib 2>&1 | grep -cE "^warning|^error"`
Expected: `0`

- [ ] **Step 6: Commit**

```bash
git add src/warp/resampler.rs
git commit -m "feat: implement sample_average box-mean kernel"
```

---

## Task 3: Factor out per-row source-coord computation in reproject.rs

This extracts the inline `gi/i0/i1/di` + column interpolation into a reusable
helper, so the Average path can request source coords for row `r` *and* row
`r+1` to estimate the vertical Jacobian. Non-Average behaviour is byte-identical.

**Files:**
- Modify: `src/warp/reproject.rs` (scalar fallback section, ~line 161-194)

- [ ] **Step 1: Add the helper function**

In `src/warp/reproject.rs`, add this free function just above the function that
contains the scalar fallback (search for the `// -- Scalar fallback` comment
near line 116 and place the helper before that function's definition):

```rust
/// Source-pixel coords `(sc, sr)` for every dst column of dst row `r`,
/// using the warp grid. Extracted so the average kernel can difference
/// row `r` against row `r+1` for the vertical Jacobian.
#[inline]
fn row_src_coords(
    r: usize,
    dst_h: usize,
    dst_w: usize,
    col_table: &[(usize, usize, f64)],
    grid_src_pix: &[(f64, f64)],
) -> Vec<(f64, f64)> {
    let gi = (r as f64 + 0.5) * (WARP_GRID_SIZE as f64) / (dst_h as f64);
    let i0 = (gi.floor() as usize).min(WARP_GRID_SIZE - 1);
    let i1 = i0 + 1;
    let di = gi - i0 as f64;
    let row_i0 = i0 * (WARP_GRID_SIZE + 1);
    let row_i1 = i1 * (WARP_GRID_SIZE + 1);
    let mut out = Vec::with_capacity(dst_w);
    for &(j0, j1, dj) in col_table.iter() {
        let p00 = grid_src_pix[row_i0 + j0];
        let p01 = grid_src_pix[row_i0 + j1];
        let p10 = grid_src_pix[row_i1 + j0];
        let p11 = grid_src_pix[row_i1 + j1];
        let w00 = (1.0 - di) * (1.0 - dj);
        let w01 = (1.0 - di) * dj;
        let w10 = di * (1.0 - dj);
        let w11 = di * dj;
        let sc = p00.0 * w00 + p01.0 * w01 + p10.0 * w10 + p11.0 * w11;
        let sr = p00.1 * w00 + p01.1 * w01 + p10.1 * w10 + p11.1 * w11;
        out.push((sc, sr));
    }
    out
}
```

- [ ] **Step 2: Rewrite the per-row closure to use the helper (non-Average path unchanged in behaviour)**

Replace the closure body (current lines ~163-192, from `let gi = ...` through `(r, mask_row, band_rows)`) with:

```rust
            let coords = row_src_coords(r, dst_h, dst_w, &col_table, grid_src_pix);

            let mut mask_row = vec![true; dst_w];
            let mut band_rows: Vec<Vec<f64>> = (0..bands).map(|_| vec![fill; dst_w]).collect();

            for (c, &(sc, sr)) in coords.iter().enumerate() {
                mask_row[c] = sample_valid(src.mask.as_ref(), src_w, src_h, sc, sr);
                for (b, band_view) in band_views.iter().enumerate() {
                    band_rows[b][c] =
                        resampling.sample_masked(*band_view, src_mask_view, sc, sr, fill);
                }
            }
            (r, mask_row, band_rows)
```

Note: `grid_src_pix` is borrowed by the closure; confirm the existing binding
is a slice/Vec in scope (it is — used at current line 175). If the borrow
checker complains about moving `col_table` into the closure, change the helper
call to borrow (`&col_table` already borrows; `col_table` must outlive the
parallel section — it already does, declared before the loop at ~line 143).

- [ ] **Step 3: Run tests to verify no behaviour change**

Run: `cargo test --lib warp`
Expected: PASS (all 12 warp tests, same as before — pure refactor).

- [ ] **Step 4: Commit**

```bash
git add src/warp/reproject.rs
git commit -m "refactor: extract row_src_coords helper in scalar warp"
```

---

## Task 4: Row-level Average dispatch with Jacobian footprint

**Files:**
- Modify: `src/warp/reproject.rs` (the per-row closure from Task 3)

- [ ] **Step 1: Hoist the dispatch flag and add the Average branch**

In the scalar fallback function, immediately before the `let rows: Vec<...> =
(0..dst_h).into_par_iter().map(|r| {` line, add:

```rust
    let is_average = matches!(resampling, Resampling::Average);
```

Then replace the per-row closure body (the version from Task 3 Step 2) with a
row-level branch. The `else` block is byte-identical to Task 3's body so the
5 existing kernels keep identical codegen:

```rust
            let coords = row_src_coords(r, dst_h, dst_w, &col_table, grid_src_pix);
            let mut mask_row = vec![true; dst_w];
            let mut band_rows: Vec<Vec<f64>> = (0..bands).map(|_| vec![fill; dst_w]).collect();

            if is_average {
                let r1 = (r + 1).min(dst_h - 1);
                let coords_r1 = row_src_coords(r1, dst_h, dst_w, &col_table, grid_src_pix);
                for c in 0..dst_w {
                    let (sc, sr) = coords[c];
                    // Column-step Jacobian (neighbour difference; one-sided at edge).
                    let cn = (c + 1).min(dst_w - 1);
                    let dx_col = (coords[cn].0 - sc).abs();
                    let dy_col = (coords[cn].1 - sr).abs();
                    // Row-step Jacobian.
                    let dx_row = (coords_r1[c].0 - sc).abs();
                    let dy_row = (coords_r1[c].1 - sr).abs();
                    let rx = (dx_col.max(dx_row) * 0.5).max(1e-6);
                    let ry = (dy_col.max(dy_row) * 0.5).max(1e-6);

                    mask_row[c] = sample_valid(src.mask.as_ref(), src_w, src_h, sc, sr);
                    for (b, band_view) in band_views.iter().enumerate() {
                        band_rows[b][c] = resampling.sample_average(
                            *band_view, src_mask_view, sc, sr, rx, ry, fill);
                    }
                }
            } else {
                for (c, &(sc, sr)) in coords.iter().enumerate() {
                    mask_row[c] = sample_valid(src.mask.as_ref(), src_w, src_h, sc, sr);
                    for (b, band_view) in band_views.iter().enumerate() {
                        band_rows[b][c] =
                            resampling.sample_masked(*band_view, src_mask_view, sc, sr, fill);
                    }
                }
            }
            (r, mask_row, band_rows)
```

- [ ] **Step 2: Add a cross-CRS Average integration test**

Add to the `tests` module at the bottom of `src/warp/reproject.rs`:

```rust
    #[test]
    fn average_cross_crs_downsample_within_source_range() {
        // 8x8 ramp source in EPSG:4326, warp+downsample to 4x4 in 3857.
        let arr = Array3::<f64>::from_shape_fn((1, 8, 8), |(_, y, x)| (y * 8 + x) as f64);
        let src = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 8.0, 8.0),
            crs: Crs::epsg(4326),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 8.0, 0.0, -1.0]),
            nodata: None,
        };
        let out = warp(&src, &Crs::epsg(3857), src.bbox, 4, 4, Resampling::Average).unwrap();
        assert_eq!(out.pixels.height(), 4);
        assert_eq!(out.pixels.width(), 4);
        // Box-mean invariant: every output value within source [0, 63].
        if let PixelData::F64(o) = &out.pixels {
            for &v in o.iter() {
                assert!(v >= 0.0 && v <= 63.0, "avg {v} out of source range");
            }
        } else {
            panic!("expected F64 output");
        }
    }
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test --lib warp`
Expected: PASS — 12 prior warp tests still green + `average_cross_crs_downsample_within_source_range` passes.

- [ ] **Step 4: Run clippy**

Run: `cargo clippy --lib 2>&1 | grep -cE "^warning|^error"`
Expected: `0`

- [ ] **Step 5: Commit**

```bash
git add src/warp/reproject.rs
git commit -m "feat: cross-CRS average resampling with Jacobian footprint"
```

---

## Task 5: Benchmark — add average case and verify control group no-regression

**Files:**
- Modify: `benches/resampler_bench.rs`

- [ ] **Step 1: Add an average_downsample bench case**

In `benches/resampler_bench.rs`, inside `fn bench`, after the existing
`strided_fallback_masked` block and before `g.finish();`, add:

```rust
    // Average kernel: 512 -> 256 downsample, ~2px footprint.
    g.bench_function("average_downsample", |b| {
        b.iter(|| {
            let kernel = Resampling::Average;
            let mut acc = 0.0;
            let scale = SRC as f64 / DST as f64;
            for r in 0..DST {
                for c in 0..DST {
                    let sc = (c as f64 + 0.5) * scale;
                    let sr = (r as f64 + 0.5) * scale;
                    acc += kernel.sample_average(
                        base.view(), None, sc, sr, scale * 0.5, scale * 0.5, 0.0);
                }
            }
            black_box(acc)
        })
    });
```

- [ ] **Step 2: Run the full bench**

Run: `cargo bench --bench resampler_bench`
Expected: completes; 5 benchmark results including `average_downsample`.

- [ ] **Step 3: Verify control group has no regression**

The hard constraint from the spec: existing kernel paths must not regress.
Inspect criterion's change report for the control benchmarks:

Run:
```bash
for d in contiguous_fastpath contiguous_fastpath_masked; do
  python3 -c "import json,os;p='target/criterion/resampler_sample_masked/$d/change/estimates.json';print('$d', round(json.load(open(p))['median']['point_estimate']*100,1),'%') if os.path.exists(p) else print('$d','(no prior baseline)')"
done
```
Expected: each within noise (roughly ±10% given prior observed machine variance). A change median consistently beyond +10% on an untouched path means the dispatch hoist regressed — STOP and investigate (likely the `else` block is not byte-identical to the pre-Task-4 loop; diff it).

- [ ] **Step 4: Commit**

```bash
git add benches/resampler_bench.rs
git commit -m "test: benchmark average_downsample with control group"
```

---

## Task 6: Final verification

- [ ] **Step 1: Full warp test suite**

Run: `cargo test --lib warp`
Expected: PASS — all warp tests (12 original + parse + 3 average unit + 1 average integration).

- [ ] **Step 2: Workspace clippy**

Run: `cargo clippy --all-targets 2>&1 | grep -cE "^warning|^error"`
Expected: `0`

- [ ] **Step 3: Release build**

Run: `cargo build --release`
Expected: compiles clean.

- [ ] **Step 4: Final commit if any fixups were needed**

```bash
git add -A
git commit -m "chore: average resampling verification fixups" || echo "nothing to commit"
```

---

## Self-Review Notes

- **Spec coverage:** Interface/parse (Task 1) ✓; same-CRS Box (Task 1 Step 6) ✓; cross-CRS footprint mean (Tasks 3-4) ✓; Jacobian neighbour-difference (Task 4 Step 1) ✓; ≤1px→bilinear (Task 2) ✓; mask/nodata all-invalid→fill (Task 2 tests) ✓; dispatch hoist + bench control (Tasks 4-5) ✓; all unit + integration + perf tests from spec ✓.
- **Type consistency:** `sample_average(self, ArrayView2<f64>, Option<ArrayView2<bool>>, f64×4, fill f64) -> f64` used identically in Task 2 (def), Task 4 (call), Task 5 (bench). `row_src_coords` signature identical in Task 3 (def) and Task 4 (calls).
- **Known approximation (documented in spec non-goals):** footprint is an axis-aligned box from the neighbour-difference Jacobian, not a reprojected polygon — accepted because PROJ error dominates.
