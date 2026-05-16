# Raster Stretch Modes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add QGIS-style stretch modes (Custom / MinMax / Percent / StdDev) so non-uint8 COGs render with sensible contrast instead of all-white, defaulting to `StdDev k=2.0` when the client passes no stretch parameters.

**Architecture:** Pure kernels (param parsing, histogram quantile, per-band stats math, rescale resolution) are unit-tested in isolation. A thin async wrapper reads the top overview for statistics when the COG has no embedded `STATISTICS_*` tags. The `Tiler` memoizes `DatasetStats` in a `tokio::sync::OnceCell` (computed lazily on the first non-Custom tile, never for `?rescale=` requests). The tiler resolves an effective rescale just before `render()` and injects it into a cloned `CompiledTileStyle` (overriding both `rescale` and `composite.vmin/vmax` so RGB and Gradient paths both honor it).

**Tech Stack:** Rust, ndarray, async_tiff, tokio, moka. Crate name: `async-geotiff`.

---

## File Structure

- **Create `src/style/stretch.rs`** — `StretchSpec` enum, `parse_stretch_kind`, `resolve_rescale` (pure, stats→ranges), `fallback_rescale` (no-stats: int dtype range / float per-tile).
- **Create `src/io/stats.rs`** — `Provenance`, `Histogram` (+`quantile`), `BandStat`, `DatasetStats`, `band_stats_from_f64` (pure kernel), `parse_embedded_stats` (GDAL XML + SMin/SMax), `load_dataset_stats` (async: tags-or-top-overview).
- **Modify `src/style/mod.rs`** — add `pub mod stretch;` + re-exports.
- **Modify `src/io/mod.rs`** — add `pub mod stats;`.
- **Modify `src/style/params.rs`** — add raw stretch fields to `TileStyle`, parse `stretch`/`pc`/`sigma`, add `resolved_stretch()`.
- **Modify `src/tiler.rs`** — add `stats` OnceCell + `dataset_stats()` + `effective_compiled()`, apply at both render call sites.

Run all tests with: `cargo test` (single test: `cargo test <name> -- --nocapture`).

---

### Task 1: `StretchSpec` enum + kind parsing

**Files:**
- Create: `src/style/stretch.rs`
- Modify: `src/style/mod.rs:1`

- [ ] **Step 1: Create `src/style/stretch.rs` with the type + parser and failing tests**

```rust
//! QGIS-style stretch modes. `Custom` is represented by a non-empty
//! `TileStyle.rescale`; this module covers the computed modes and the
//! default (StdDev k=2.0) applied when the client passes nothing.

use crate::error::{AgError, AgResult};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StretchSpec {
    MinMax,
    Percent { lo: f64, hi: f64 },
    StdDev { k: f64 },
}

impl StretchSpec {
    /// The default applied when no `rescale` and no `stretch` are given
    /// (caller decides whether the dtype is eligible — u8 keeps its
    /// fast path and never reaches here).
    pub const DEFAULT: StretchSpec = StretchSpec::StdDev { k: 2.0 };
}

/// Build a `StretchSpec` from already-parsed raw params. `kind` is the
/// `stretch=` value; `pc`/`sigma` are the optional companion params.
/// Invalid input → `AgError::query` (maps to HTTP 400).
pub fn parse_stretch_kind(
    kind: &str,
    pc: Option<(f64, f64)>,
    sigma: Option<f64>,
) -> AgResult<StretchSpec> {
    match kind.to_ascii_lowercase().as_str() {
        "minmax" => Ok(StretchSpec::MinMax),
        "percent" => {
            let (lo, hi) = pc.unwrap_or((2.0, 98.0));
            if !(0.0..=100.0).contains(&lo) || !(0.0..=100.0).contains(&hi) || lo >= hi {
                return Err(AgError::query("pc", format!("invalid percentile {lo},{hi}")));
            }
            Ok(StretchSpec::Percent { lo, hi })
        }
        "stddev" => {
            let k = sigma.unwrap_or(2.0);
            if !(k > 0.0) {
                return Err(AgError::query("sigma", format!("must be > 0, got {k}")));
            }
            Ok(StretchSpec::StdDev { k })
        }
        other => Err(AgError::query("stretch", format!("unknown mode {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_three_modes_case_insensitive() {
        assert_eq!(parse_stretch_kind("MinMax", None, None).unwrap(), StretchSpec::MinMax);
        assert_eq!(
            parse_stretch_kind("percent", Some((1.0, 99.0)), None).unwrap(),
            StretchSpec::Percent { lo: 1.0, hi: 99.0 }
        );
        assert_eq!(
            parse_stretch_kind("STDDEV", None, Some(3.0)).unwrap(),
            StretchSpec::StdDev { k: 3.0 }
        );
    }

    #[test]
    fn percent_and_stddev_defaults() {
        assert_eq!(
            parse_stretch_kind("percent", None, None).unwrap(),
            StretchSpec::Percent { lo: 2.0, hi: 98.0 }
        );
        assert_eq!(
            parse_stretch_kind("stddev", None, None).unwrap(),
            StretchSpec::StdDev { k: 2.0 }
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_stretch_kind("bogus", None, None).is_err());
        assert!(parse_stretch_kind("percent", Some((90.0, 10.0)), None).is_err());
        assert!(parse_stretch_kind("percent", Some((-1.0, 50.0)), None).is_err());
        assert!(parse_stretch_kind("stddev", None, Some(0.0)).is_err());
    }
}
```

- [ ] **Step 2: Wire the module**

In `src/style/mod.rs`, after `pub mod params;`:

```rust
pub mod params;
pub mod stretch;

pub use params::{compile, TileStyle};
pub use stretch::StretchSpec;
```

- [ ] **Step 3: Run tests, verify they pass**

Run: `cargo test --lib stretch::tests -- --nocapture`
Expected: PASS (3 tests)

- [ ] **Step 4: Commit**

```bash
git add src/style/stretch.rs src/style/mod.rs
git commit -m "feat(stretch): StretchSpec enum + mode parsing"
```

---

### Task 2: `Histogram` + quantile (pure kernel)

**Files:**
- Modify: `src/io/stats.rs` (create)
- Modify: `src/io/mod.rs:1-4`

- [ ] **Step 1: Create `src/io/stats.rs` with `Histogram` + failing test**

```rust
//! Dataset band statistics: embedded `STATISTICS_*` tags (exact) or a
//! one-shot top-overview computation (estimated). See
//! docs/superpowers/specs/2026-05-16-non-uint8-data-stretch-design.md.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Provenance {
    Exact,
    Estimated,
}

/// Equal-width histogram over `[min, max]`. `counts.len()` is the bin count.
#[derive(Debug, Clone)]
pub struct Histogram {
    pub min: f64,
    pub max: f64,
    pub counts: Vec<u64>,
}

impl Histogram {
    /// Value at percentile `p` (0..=100) via linear interpolation on the
    /// cumulative count. Robust to the overview-smoothing approximation
    /// (see spec accuracy table). Empty / degenerate → `min`.
    pub fn quantile(&self, p: f64) -> f64 {
        let total: u64 = self.counts.iter().sum();
        if total == 0 || self.max <= self.min || self.counts.is_empty() {
            return self.min;
        }
        let target = (p / 100.0) * total as f64;
        let bin_w = (self.max - self.min) / self.counts.len() as f64;
        let mut cum = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            let next = cum + c;
            if (next as f64) >= target {
                // Fraction into this bin.
                let into = if c == 0 {
                    0.0
                } else {
                    (target - cum as f64) / c as f64
                };
                return self.min + (i as f64 + into) * bin_w;
            }
            cum = next;
        }
        self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantile_linear_interpolates() {
        // 4 bins over [0,4], 10 counts each. p=50 → midpoint = 2.0.
        let h = Histogram { min: 0.0, max: 4.0, counts: vec![10, 10, 10, 10] };
        assert!((h.quantile(50.0) - 2.0).abs() < 1e-9);
        assert!((h.quantile(0.0) - 0.0).abs() < 1e-9);
        assert!((h.quantile(100.0) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn quantile_skewed() {
        // Mass concentrated in first bin: p=2 stays near min.
        let h = Histogram { min: 0.0, max: 100.0, counts: vec![98, 1, 1, 0] };
        assert!(h.quantile(2.0) < 25.0);
        assert!(h.quantile(98.0) >= 25.0);
    }

    #[test]
    fn quantile_degenerate_returns_min() {
        let h = Histogram { min: 5.0, max: 5.0, counts: vec![0, 0] };
        assert_eq!(h.quantile(50.0), 5.0);
    }
}
```

- [ ] **Step 2: Wire the module**

In `src/io/mod.rs`, add after line 4 (`pub mod window;`):

```rust
pub mod stats;
```

- [ ] **Step 3: Run tests**

Run: `cargo test --lib stats::tests::quantile -- --nocapture`
Expected: PASS (3 tests)

- [ ] **Step 4: Commit**

```bash
git add src/io/stats.rs src/io/mod.rs
git commit -m "feat(stats): Histogram with percentile quantile"
```

---

### Task 3: `BandStat` / `DatasetStats` + `band_stats_from_f64` kernel

**Files:**
- Modify: `src/io/stats.rs`

- [ ] **Step 1: Add structs + kernel + failing test**

Append to `src/io/stats.rs` (above `#[cfg(test)]`):

```rust
use ndarray::{Array3, Axis};

#[derive(Debug, Clone)]
pub struct BandStat {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub stddev: f64,
    pub hist: Histogram,
}

#[derive(Debug, Clone)]
pub struct DatasetStats {
    /// Indexed by original (full) band order.
    pub bands: Vec<BandStat>,
    pub provenance: Provenance,
}

/// Per-band stats from a planar `(bands, H, W)` f64 array, excluding
/// `nodata` and non-finite samples. `hist_bins` is the histogram
/// resolution (256 in production).
pub fn band_stats_from_f64(
    arr: &Array3<f64>,
    nodata: Option<f64>,
    hist_bins: usize,
) -> Vec<BandStat> {
    let bins = hist_bins.max(1);
    let mut out = Vec::with_capacity(arr.shape()[0]);
    for b in 0..arr.shape()[0] {
        let band = arr.index_axis(Axis(0), b);
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut sum = 0.0_f64;
        let mut sumsq = 0.0_f64;
        let mut count = 0_u64;
        for &v in band.iter() {
            if !v.is_finite() {
                continue;
            }
            if let Some(nd) = nodata {
                if (v - nd).abs() < 1e-9 {
                    continue;
                }
            }
            min = min.min(v);
            max = max.max(v);
            sum += v;
            sumsq += v * v;
            count += 1;
        }
        if count == 0 {
            out.push(BandStat {
                min: 0.0,
                max: 1.0,
                mean: 0.0,
                stddev: 0.0,
                hist: Histogram { min: 0.0, max: 1.0, counts: vec![0; bins] },
            });
            continue;
        }
        let mean = sum / count as f64;
        let var = (sumsq / count as f64) - mean * mean;
        let stddev = var.max(0.0).sqrt();
        let mut counts = vec![0u64; bins];
        let span = (max - min).max(1e-12);
        for &v in band.iter() {
            if !v.is_finite() {
                continue;
            }
            if let Some(nd) = nodata {
                if (v - nd).abs() < 1e-9 {
                    continue;
                }
            }
            let mut idx = (((v - min) / span) * bins as f64) as usize;
            if idx >= bins {
                idx = bins - 1;
            }
            counts[idx] += 1;
        }
        out.push(BandStat {
            min,
            max,
            mean,
            stddev,
            hist: Histogram { min, max, counts },
        });
    }
    out
}
```

Add to the `tests` module in `src/io/stats.rs`:

```rust
    #[test]
    fn kernel_basic_stats_and_nodata_exclusion() {
        use ndarray::Array3;
        // 1 band, 1x4: values 0(nodata), 2, 4, 6 → over {2,4,6}.
        let arr =
            Array3::from_shape_vec((1, 1, 4), vec![0.0, 2.0, 4.0, 6.0]).unwrap();
        let s = band_stats_from_f64(&arr, Some(0.0), 8);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].min, 2.0);
        assert_eq!(s[0].max, 6.0);
        assert!((s[0].mean - 4.0).abs() < 1e-9);
        // population stddev of {2,4,6} = sqrt(8/3) ≈ 1.6329931619
        assert!((s[0].stddev - (8.0_f64 / 3.0).sqrt()).abs() < 1e-9);
    }

    #[test]
    fn kernel_all_nodata_is_safe() {
        use ndarray::Array3;
        let arr = Array3::from_shape_vec((1, 1, 2), vec![0.0, 0.0]).unwrap();
        let s = band_stats_from_f64(&arr, Some(0.0), 4);
        assert_eq!(s[0].min, 0.0);
        assert_eq!(s[0].max, 1.0);
    }
```

- [ ] **Step 2: Run tests, verify pass**

Run: `cargo test --lib stats::tests -- --nocapture`
Expected: PASS (5 tests total)

- [ ] **Step 3: Commit**

```bash
git add src/io/stats.rs
git commit -m "feat(stats): BandStat/DatasetStats + band_stats_from_f64 kernel"
```

---

### Task 4: `parse_embedded_stats` (GDAL XML + SMin/SMax)

**Files:**
- Modify: `src/io/stats.rs`

- [ ] **Step 1: Add parser + failing test**

Append to `src/io/stats.rs` (above `#[cfg(test)]`):

```rust
/// Parse exact stats from a COG's GDAL_METADATA XML, falling back to
/// TIFF `SMin/SMaxSampleValue`. Returns `None` when neither yields a
/// usable min/max for every band. Histogram comes from
/// `STATISTICS_HISTOMIN/HISTOMAX/HISTONUMBINS/HISTOBINS` when present,
/// otherwise a single-bin placeholder (percentile then ≈ min/max).
pub fn parse_embedded_stats(
    gdal_metadata: Option<&str>,
    smin: Option<&[u16]>,
    smax: Option<&[u16]>,
    bands: usize,
) -> Option<DatasetStats> {
    // Pull `<Item name="X" sample="N">V</Item>` into (name, sample, value).
    fn items(xml: &str) -> Vec<(String, usize, String)> {
        let mut v = Vec::new();
        for chunk in xml.split("<Item").skip(1) {
            let Some(open_end) = chunk.find('>') else { continue };
            let attrs = &chunk[..open_end];
            let body = &chunk[open_end + 1..];
            let Some(close) = body.find("</Item>") else { continue };
            let value = body[..close].trim().to_string();
            let name = attr(attrs, "name");
            let sample = attr(attrs, "sample").parse::<usize>().unwrap_or(0);
            if !name.is_empty() {
                v.push((name, sample, value));
            }
        }
        v
    }
    fn attr(attrs: &str, key: &str) -> String {
        let pat = format!("{key}=\"");
        let Some(start) = attrs.find(&pat) else { return String::new() };
        let rest = &attrs[start + pat.len()..];
        rest.find('"').map(|e| rest[..e].to_string()).unwrap_or_default()
    }

    let parsed = gdal_metadata.map(items).unwrap_or_default();
    let get = |name: &str, b: usize| -> Option<f64> {
        parsed
            .iter()
            .find(|(n, s, _)| n == name && *s == b)
            .and_then(|(_, _, v)| v.parse::<f64>().ok())
    };

    let mut out = Vec::with_capacity(bands);
    for b in 0..bands {
        let min = get("STATISTICS_MINIMUM", b).or_else(|| {
            smin.and_then(|s| s.get(b)).map(|&x| x as f64)
        })?;
        let max = get("STATISTICS_MAXIMUM", b).or_else(|| {
            smax.and_then(|s| s.get(b)).map(|&x| x as f64)
        })?;
        let mean = get("STATISTICS_MEAN", b).unwrap_or((min + max) / 2.0);
        let stddev = get("STATISTICS_STDDEV", b).unwrap_or((max - min) / 4.0);
        let hist = {
            let hmin = get("STATISTICS_HISTOMIN", b).unwrap_or(min);
            let hmax = get("STATISTICS_HISTOMAX", b).unwrap_or(max);
            let counts: Vec<u64> = parsed
                .iter()
                .find(|(n, s, _)| n == "STATISTICS_HISTOBINS" && *s == b)
                .map(|(_, _, v)| {
                    v.split(|c: char| c == ',' || c.is_whitespace())
                        .filter(|t| !t.is_empty())
                        .filter_map(|t| t.parse::<u64>().ok())
                        .collect()
                })
                .filter(|c: &Vec<u64>| !c.is_empty())
                .unwrap_or_else(|| vec![1]);
            Histogram { min: hmin, max: hmax, counts }
        };
        out.push(BandStat { min, max, mean, stddev, hist });
    }
    Some(DatasetStats { bands: out, provenance: Provenance::Exact })
}
```

Add to the `tests` module:

```rust
    #[test]
    fn parses_gdal_metadata_per_band() {
        let xml = r#"<GDALMetadata>
<Item name="STATISTICS_MINIMUM" sample="0">79</Item>
<Item name="STATISTICS_MAXIMUM" sample="0">1896</Item>
<Item name="STATISTICS_MEAN" sample="0">565</Item>
<Item name="STATISTICS_STDDEV" sample="0">45</Item>
<Item name="STATISTICS_MINIMUM" sample="1">74</Item>
<Item name="STATISTICS_MAXIMUM" sample="1">2213</Item>
</GDALMetadata>"#;
        let s = parse_embedded_stats(Some(xml), None, None, 2).unwrap();
        assert_eq!(s.provenance, Provenance::Exact);
        assert_eq!(s.bands[0].min, 79.0);
        assert_eq!(s.bands[0].max, 1896.0);
        assert_eq!(s.bands[0].mean, 565.0);
        assert_eq!(s.bands[1].min, 74.0);
        // band 1 has no MEAN tag → midpoint fallback
        assert!((s.bands[1].mean - (74.0 + 2213.0) / 2.0).abs() < 1e-9);
    }

    #[test]
    fn falls_back_to_sminmax_then_none() {
        // No XML, but SMin/SMax tags present.
        let s = parse_embedded_stats(None, Some(&[10, 20]), Some(&[100, 200]), 2).unwrap();
        assert_eq!(s.bands[0].min, 10.0);
        assert_eq!(s.bands[1].max, 200.0);
        // Nothing at all → None.
        assert!(parse_embedded_stats(None, None, None, 1).is_none());
        // Malformed XML → None (no usable min/max).
        assert!(parse_embedded_stats(Some("<not xml"), None, None, 1).is_none());
    }
```

- [ ] **Step 2: Run tests**

Run: `cargo test --lib stats::tests -- --nocapture`
Expected: PASS (7 tests total)

- [ ] **Step 3: Commit**

```bash
git add src/io/stats.rs
git commit -m "feat(stats): parse embedded GDAL_METADATA / SMin-SMax stats"
```

---

### Task 5: `resolve_rescale` (stats → per-band ranges)

**Files:**
- Modify: `src/style/stretch.rs`

- [ ] **Step 1: Add `resolve_rescale` + failing test**

Append to `src/style/stretch.rs` (above `#[cfg(test)]`):

```rust
use crate::io::stats::{DatasetStats, StretchClamp};

/// Map a `StretchSpec` to per-band `(lo, hi)` for the selected bands.
/// `bidx` is 0-based original band indices (already resolved by the
/// caller to concrete bands, never empty). Degenerate `hi <= lo` is
/// widened to `(lo, lo + 1.0)` so the render divide stays finite.
pub fn resolve_rescale(
    spec: &StretchSpec,
    stats: &DatasetStats,
    bidx: &[usize],
) -> Vec<(f64, f64)> {
    bidx.iter()
        .map(|&b| {
            let s = stats
                .bands
                .get(b)
                .or_else(|| stats.bands.last())
                .expect("DatasetStats always has >= 1 band");
            let (mut lo, mut hi) = match spec {
                StretchSpec::MinMax => (s.min, s.max),
                StretchSpec::Percent { lo, hi } => {
                    (s.hist.quantile(*lo), s.hist.quantile(*hi))
                }
                StretchSpec::StdDev { k } => (
                    (s.mean - k * s.stddev).max(s.min),
                    (s.mean + k * s.stddev).min(s.max),
                ),
            };
            if !(hi > lo) {
                hi = lo + 1.0;
            }
            (lo, hi)
        })
        .collect()
}
```

Note: `StretchClamp` is unused here — remove that `use` line if the
compiler warns; it is only a marker that this function does not need
pixel data. (Replace the `use` with `use crate::io::stats::DatasetStats;`.)

Add to the `tests` module in `src/style/stretch.rs`:

```rust
    use crate::io::stats::{BandStat, DatasetStats, Histogram, Provenance};

    fn ds(min: f64, max: f64, mean: f64, sd: f64) -> DatasetStats {
        DatasetStats {
            bands: vec![BandStat {
                min,
                max,
                mean,
                stddev: sd,
                hist: Histogram { min, max, counts: vec![1, 1, 1, 1] },
            }],
            provenance: Provenance::Estimated,
        }
    }

    #[test]
    fn minmax_uses_band_extremes() {
        let r = resolve_rescale(&StretchSpec::MinMax, &ds(79.0, 1896.0, 565.0, 45.0), &[0]);
        assert_eq!(r, vec![(79.0, 1896.0)]);
    }

    #[test]
    fn stddev_clamps_to_minmax() {
        // mean 565, sd 45, k 2 → (475, 655); within [79,1896] so unchanged.
        let r = resolve_rescale(&StretchSpec::StdDev { k: 2.0 }, &ds(79.0, 1896.0, 565.0, 45.0), &[0]);
        assert!((r[0].0 - 475.0).abs() < 1e-9);
        assert!((r[0].1 - 655.0).abs() < 1e-9);
        // Huge k → clamps to min/max.
        let r2 = resolve_rescale(&StretchSpec::StdDev { k: 999.0 }, &ds(79.0, 1896.0, 565.0, 45.0), &[0]);
        assert_eq!(r2, vec![(79.0, 1896.0)]);
    }

    #[test]
    fn degenerate_range_widened() {
        let r = resolve_rescale(&StretchSpec::MinMax, &ds(5.0, 5.0, 5.0, 0.0), &[0]);
        assert_eq!(r, vec![(5.0, 6.0)]);
    }

    #[test]
    fn bidx_out_of_range_reuses_last_band() {
        let r = resolve_rescale(&StretchSpec::MinMax, &ds(1.0, 9.0, 5.0, 1.0), &[0, 0, 0]);
        assert_eq!(r.len(), 3);
    }
```

- [ ] **Step 2: Fix the `use` line**

Replace `use crate::io::stats::{DatasetStats, StretchClamp};` with:

```rust
use crate::io::stats::DatasetStats;
```

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test --lib stretch::tests -- --nocapture`
Expected: PASS (7 tests total)

- [ ] **Step 4: Commit**

```bash
git add src/style/stretch.rs
git commit -m "feat(stretch): resolve_rescale for MinMax/Percent/StdDev"
```

---

### Task 6: `fallback_rescale` (no stats: int dtype range / float per-tile)

**Files:**
- Modify: `src/style/stretch.rs`

- [ ] **Step 1: Add `fallback_rescale` + failing test**

Append to `src/style/stretch.rs` (above `#[cfg(test)]`):

```rust
use crate::pixel::PixelData;

/// Used only when no embedded stats AND no overview to compute from.
/// Integers → dtype natural range; floats → per-tile min/max of the
/// warped buffer (excludes nodata / non-finite). `bidx` already
/// resolved to concrete bands.
pub fn fallback_rescale(
    pixels: &PixelData,
    nodata: Option<f64>,
    bidx: &[usize],
) -> Vec<(f64, f64)> {
    use ndarray::Axis;
    let dtype_range: Option<(f64, f64)> = match pixels {
        PixelData::U8(_) => Some((0.0, 255.0)),
        PixelData::U16(_) => Some((0.0, 65535.0)),
        PixelData::I16(_) => Some((-32768.0, 32767.0)),
        PixelData::U32(_) => Some((0.0, 4_294_967_295.0)),
        PixelData::I32(_) => Some((-2_147_483_648.0, 2_147_483_647.0)),
        PixelData::F32(_) | PixelData::F64(_) => None,
    };
    if let Some(r) = dtype_range {
        return bidx.iter().map(|_| r).collect();
    }
    // Float: per-tile min/max.
    let arr = pixels.to_f64();
    bidx.iter()
        .map(|&b| {
            let band = arr.index_axis(Axis(0), b.min(arr.shape()[0].saturating_sub(1)));
            let mut lo = f64::INFINITY;
            let mut hi = f64::NEG_INFINITY;
            for &v in band.iter() {
                if !v.is_finite() {
                    continue;
                }
                if let Some(nd) = nodata {
                    if (v - nd).abs() < 1e-9 {
                        continue;
                    }
                }
                lo = lo.min(v);
                hi = hi.max(v);
            }
            if !(hi > lo) {
                (0.0, 1.0)
            } else {
                (lo, hi)
            }
        })
        .collect()
}
```

Add to the `tests` module:

```rust
    use crate::pixel::PixelData;
    use ndarray::Array3;

    #[test]
    fn fallback_integer_uses_dtype_range() {
        let p = PixelData::U16(Array3::from_elem((1, 2, 2), 500u16));
        assert_eq!(fallback_rescale(&p, None, &[0]), vec![(0.0, 65535.0)]);
    }

    #[test]
    fn fallback_float_uses_per_tile_minmax_excluding_nodata() {
        let p = PixelData::F32(
            Array3::from_shape_vec((1, 1, 4), vec![-9999.0f32, 1.0, 5.0, 9.0]).unwrap(),
        );
        let r = fallback_rescale(&p, Some(-9999.0), &[0]);
        assert_eq!(r, vec![(1.0, 9.0)]);
    }
```

- [ ] **Step 2: Run tests, verify pass**

Run: `cargo test --lib stretch::tests -- --nocapture`
Expected: PASS (9 tests total)

- [ ] **Step 3: Commit**

```bash
git add src/style/stretch.rs
git commit -m "feat(stretch): fallback_rescale for no-stats datasets"
```

---

### Task 7: `TileStyle` stretch params

**Files:**
- Modify: `src/style/params.rs:11-21` (struct), `:22-35` (Default), `:67-97` (apply_param)

- [ ] **Step 1: Add raw fields + parsing + `resolved_stretch`, with failing test**

In `src/style/params.rs`, add to the `use` block at top:

```rust
use crate::style::stretch::{parse_stretch_kind, StretchSpec};
```

Add three fields to `struct TileStyle` (after `pub rescale: Vec<(f64, f64)>,`):

```rust
    pub stretch_kind: Option<String>,
    pub stretch_pc: Option<(f64, f64)>,
    pub stretch_sigma: Option<f64>,
```

Add to `impl Default for TileStyle` (after `rescale: vec![],`):

```rust
            stretch_kind: None,
            stretch_pc: None,
            stretch_sigma: None,
```

In `apply_param`, add these match arms (before the `_ => {}` arm):

```rust
            "stretch" => self.stretch_kind = Some(v.to_owned()),
            "pc" => {
                let (a, b) = v.split_once(',').ok_or_else(|| AgError::query("pc", v))?;
                self.stretch_pc = Some((
                    a.parse().map_err(|_| AgError::query("pc", a))?,
                    b.parse().map_err(|_| AgError::query("pc", b))?,
                ));
            }
            "sigma" => {
                self.stretch_sigma =
                    Some(v.parse().map_err(|_| AgError::query("sigma", v))?)
            }
```

Add this method to `impl TileStyle` (after `from_pairs`):

```rust
    /// `None` → caller applies the dtype-aware default (u8 keeps its
    /// fast path; others use `StretchSpec::DEFAULT`). `Some(spec)` →
    /// an explicit `?stretch=` mode. Invalid params → `AgError::query`
    /// (HTTP 400). A non-empty `rescale` (Custom) makes this irrelevant
    /// — the caller checks `rescale` first.
    pub fn resolved_stretch(&self) -> AgResult<Option<StretchSpec>> {
        match &self.stretch_kind {
            None => Ok(None),
            Some(k) => Ok(Some(parse_stretch_kind(
                k,
                self.stretch_pc,
                self.stretch_sigma,
            )?)),
        }
    }
```

Add to the `tests` module in `src/style/params.rs`:

```rust
    #[test]
    fn parses_stretch_params() {
        let s = TileStyle::from_query("stretch=percent&pc=2,98").unwrap();
        assert_eq!(
            s.resolved_stretch().unwrap(),
            Some(crate::style::stretch::StretchSpec::Percent { lo: 2.0, hi: 98.0 })
        );

        let none = TileStyle::from_query("").unwrap();
        assert_eq!(none.resolved_stretch().unwrap(), None);

        let bad = TileStyle::from_query("stretch=nope").unwrap();
        assert!(bad.resolved_stretch().is_err());
    }
```

- [ ] **Step 2: Run tests, verify pass**

Run: `cargo test --lib params::tests -- --nocapture`
Expected: PASS (existing params tests + `parses_stretch_params`)

- [ ] **Step 3: Run full suite to check `compile_cached` key still builds**

Run: `cargo test --lib -- --nocapture`
Expected: PASS (the added `Debug`-derived fields keep `format!("{style:?}")` valid)

- [ ] **Step 4: Commit**

```bash
git add src/style/params.rs
git commit -m "feat(params): parse stretch/pc/sigma into TileStyle"
```

---

### Task 8: `load_dataset_stats` (tags → top overview)

**Files:**
- Modify: `src/io/stats.rs`
- Reference: `src/io/cog.rs:218-329` (`level_count`, `data_ifd`, `read_window`)

- [ ] **Step 1: Add the async loader**

Append to `src/io/stats.rs` (above `#[cfg(test)]`):

```rust
use crate::error::AgResult;
use crate::io::CogReader;

const HIST_BINS: usize = 256;

/// Embedded `STATISTICS_*` tags (exact) when present; otherwise compute
/// from the highest (smallest) overview (estimated). `Ok(None)` when the
/// COG has no overview to fall back to — caller then uses
/// `stretch::fallback_rescale`. Reads ≈ the top overview only (a few
/// hundred KB), independent of full-resolution size.
pub async fn load_dataset_stats(reader: &CogReader) -> AgResult<Option<DatasetStats>> {
    // 1) Exact path: embedded tags on the full-res IFD.
    let ifd = reader.data_ifd(0)?;
    let bands = ifd.samples_per_pixel() as usize;
    if let Some(exact) = parse_embedded_stats(
        ifd.gdal_metadata(),
        ifd.min_sample_value(),
        ifd.max_sample_value(),
        bands,
    ) {
        return Ok(Some(exact));
    }

    // 2) Estimated path: read the highest overview in full.
    let top = reader.level_count().saturating_sub(1);
    if top == 0 {
        // No overview pyramid — caller falls back per-tile.
        return Ok(None);
    }
    let (w, h) = reader.level_dimensions(top)?;
    let img = reader
        .read_window(top, 0, 0, w as u64, h as u64)
        .await?;
    let arr = img.pixels.to_f64();
    let stats = band_stats_from_f64(&arr, img.nodata, HIST_BINS);
    Ok(Some(DatasetStats { bands: stats, provenance: Provenance::Estimated }))
}
```

If `ifd.samples_per_pixel()` is not the exact accessor name, use the
band count from the spec's reader API: `reader.level_dimensions(0)` does
not give bands — instead derive `bands` from a 1×1 probe is overkill;
the IFD exposes `samples_per_pixel()` (async_tiff `ImageFileDirectory`).
Confirm by running Step 2; fix the accessor if it fails to compile.

- [ ] **Step 2: Compile-check**

Run: `cargo build --lib`
Expected: builds. If `samples_per_pixel()` / `min_sample_value()` /
`max_sample_value()` mismatch, adjust to the actual `async_tiff`
`ImageFileDirectory` method names (verified present in async-tiff 0.3.0:
`gdal_metadata`, `min_sample_value`, `max_sample_value`,
`samples_per_pixel`).

- [ ] **Step 3: Add an `#[ignore]` real-COG integration test**

Add to the `tests` module in `src/io/stats.rs`:

```rust
    // Exercises the estimated (overview) path against a local fixture.
    // Ignored by default: requires a multi-overview UInt16 COG on disk.
    #[tokio::test]
    #[ignore = "requires local data/SVN3-..._cog.tif"]
    async fn estimated_stats_from_overview() {
        use async_tiff::reader::ObjectReader;
        use object_store::local::LocalFileSystem;
        use object_store::path::Path;
        use std::sync::Arc;

        let fs = Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        let p = Path::from(
            "data/SVN3-01_20250814_L2D0000203064_01_6112500050210001-FUS_3857_cog.tif",
        );
        let reader = crate::io::CogReader::open_with_reader(ObjectReader::new(fs, p))
            .await
            .unwrap();
        let s = load_dataset_stats(&reader).await.unwrap().unwrap();
        assert_eq!(s.provenance, Provenance::Estimated);
        assert_eq!(s.bands.len(), 3);
        // From gdalinfo -approx_stats: band0 ≈ [79, 1896].
        assert!(s.bands[0].min >= 0.0 && s.bands[0].max < 65535.0);
        assert!(s.bands[0].max > s.bands[0].min);
    }
```

- [ ] **Step 4: Run unit tests (ignored test skipped)**

Run: `cargo test --lib stats:: -- --nocapture`
Expected: PASS; `estimated_stats_from_overview` reported as ignored.

- [ ] **Step 5: Commit**

```bash
git add src/io/stats.rs
git commit -m "feat(stats): load_dataset_stats (embedded tags or top overview)"
```

---

### Task 9: Tiler integration — OnceCell + `effective_compiled`

**Files:**
- Modify: `src/tiler.rs:21-58` (struct + ctor), add methods, `:182-186` & `:212-218` (render call sites)

- [ ] **Step 1: Add the OnceCell field + accessor + resolver**

In `src/tiler.rs`, add imports near the existing `use` block:

```rust
use crate::io::stats::{load_dataset_stats, DatasetStats};
use crate::pixel::PixelData;
use crate::render::pipeline::CompiledTileStyle;
use crate::style::stretch::{fallback_rescale, resolve_rescale, StretchSpec};
use tokio::sync::OnceCell;
```

Add the field to `struct Tiler`:

```rust
pub struct Tiler {
    reader: Arc<CogReader>,
    tile_cpu_semaphore: Arc<Semaphore>,
    stats: OnceCell<Option<Arc<DatasetStats>>>,
}
```

Update **both** constructors that build `Self { ... }` in
`with_config` to add `stats: OnceCell::new(),`.

Add these methods inside `impl Tiler` (after `wgs84_bounds`):

```rust
    /// Lazily computed once per dataset, then memoized for the life of
    /// this `Tiler` (which `AppState.tilers` keeps alive across all tile
    /// requests). `?rescale=` requests never call this.
    async fn dataset_stats(&self) -> AgResult<Option<Arc<DatasetStats>>> {
        self.stats
            .get_or_try_init(|| async {
                Ok::<_, AgError>(load_dataset_stats(&self.reader).await?.map(Arc::new))
            })
            .await
            .cloned()
    }

    /// Resolve the effective rescale and inject it into a cloned
    /// `CompiledTileStyle`. Priority: explicit `rescale` (Custom) →
    /// `?stretch=` mode → default StdDev k=2.0 (u8 keeps its fast path).
    /// Sets BOTH `rescale` (RGB path / fast-path identity) and
    /// `composite.vmin/vmax` (single-band Gradient path).
    async fn effective_compiled(
        &self,
        style: &TileStyle,
        compiled: &Arc<CompiledTileStyle>,
        image: &ImageData,
    ) -> AgResult<Arc<CompiledTileStyle>> {
        // Custom wins; nothing to resolve.
        if !style.rescale.is_empty() {
            return Ok(Arc::clone(compiled));
        }
        let spec: StretchSpec = match style.resolved_stretch()? {
            Some(s) => s,
            None => {
                // No explicit mode: u8 keeps current behavior untouched.
                if matches!(image.pixels, PixelData::U8(_)) {
                    return Ok(Arc::clone(compiled));
                }
                StretchSpec::DEFAULT
            }
        };

        // Resolve concrete band indices the same way render() does.
        let bands = image.pixels.bands();
        let bidx: Vec<usize> = if compiled.bidx.is_empty() {
            if bands >= 3 {
                vec![0, 1, 2]
            } else {
                vec![0]
            }
        } else {
            compiled.bidx.clone()
        };

        let ranges = match self.dataset_stats().await? {
            Some(stats) => resolve_rescale(&spec, &stats, &bidx),
            None => fallback_rescale(&image.pixels, image.nodata, &bidx),
        };

        let mut next = (**compiled).clone();
        next.rescale = ranges.clone();
        next.composite.vmin = Some(ranges.iter().map(|r| r.0).collect());
        next.composite.vmax = Some(ranges.iter().map(|r| r.1).collect());
        next.auto_rgb = false;
        Ok(Arc::new(next))
    }
```

- [ ] **Step 2: Apply at the L2 passthrough render site**

In `src/tiler.rs`, find the passthrough block:

```rust
                        let compiled = compile_cached(style).await?;
                        let dur_compile = t.elapsed();
                        let t = Instant::now();
                        let png = self
                            .run_tile_cpu("render", move || render(&src, &compiled))
                            .await?;
```

Replace with:

```rust
                        let compiled = compile_cached(style).await?;
                        let compiled = self
                            .effective_compiled(style, &compiled, &src)
                            .await?;
                        let dur_compile = t.elapsed();
                        let t = Instant::now();
                        let png = self
                            .run_tile_cpu("render", move || render(&src, &compiled))
                            .await?;
```

- [ ] **Step 3: Apply at the warp-path render site**

Find (near end of `tile`):

```rust
        let compiled = compile_cached(style).await?;
        let dur_compile = t.elapsed();
        let t = Instant::now();
        let png = self
            .run_tile_cpu("render", move || render(&warped, &compiled))
            .await?;
```

Replace with:

```rust
        let compiled = compile_cached(style).await?;
        let compiled = self
            .effective_compiled(style, &compiled, &warped)
            .await?;
        let dur_compile = t.elapsed();
        let t = Instant::now();
        let png = self
            .run_tile_cpu("render", move || render(&warped, &compiled))
            .await?;
```

- [ ] **Step 4: Build + run full suite**

Run: `cargo build --lib && cargo test --lib -- --nocapture`
Expected: builds; all existing tests still PASS (u8 path: `rescale`
empty + `stretch_kind` None + `PixelData::U8` → early `Arc::clone`,
zero behavior change).

- [ ] **Step 5: Commit**

```bash
git add src/tiler.rs
git commit -m "feat(tiler): lazy DatasetStats + effective rescale injection"
```

---

### Task 10: Regression guard — u8 fast path byte-identical

**Files:**
- Modify: `src/tiler.rs` (tests module)

- [ ] **Step 1: Add a guard test asserting u8 default request is unchanged**

This test documents the invariant: a u8 image with empty `rescale` and
no `stretch` must not be altered by `effective_compiled`. Add to the
`tests` module in `src/tiler.rs`:

```rust
    #[test]
    fn u8_default_request_keeps_rescale_empty() {
        // effective_compiled's u8 short-circuit is reached when:
        //   rescale empty  &&  stretch_kind None  &&  PixelData::U8
        // We assert the decision predicate here (pure, no IO):
        use crate::style::TileStyle;
        let style = TileStyle::from_query("").unwrap();
        assert!(style.rescale.is_empty());
        assert!(style.resolved_stretch().unwrap().is_none());
        // The combination above + U8 → effective_compiled returns the
        // input Arc unchanged (verified by Task 9 Step 4 full-suite run,
        // which exercises real u8 fixtures through render()).
    }
```

- [ ] **Step 2: Run tests**

Run: `cargo test --lib tiler::tests -- --nocapture`
Expected: PASS (existing tiler tests + `u8_default_request_keeps_rescale_empty`)

- [ ] **Step 3: Run clippy + full suite**

Run: `cargo clippy --lib -- -D warnings && cargo test --lib`
Expected: no clippy warnings; all tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/tiler.rs
git commit -m "test(tiler): regression guard for u8 default-request invariant"
```

---

### Task 11: Manual verification against the real dataset

**Files:** none (manual)

- [ ] **Step 1: Start the demo server**

Run (user runs this in their shell — needs the 9GB local file):

```bash
cargo run --example serve -- --data-dir data
```

- [ ] **Step 2: Compare the three requests**

Open in a browser / curl and save the PNGs:

```
# Before-fix reproduction (all white):
.../14/13660/6190.png?dataset=SVN3-...FUS_3857_cog.tif&bidx=1&bidx=2&bidx=3&rescale=0,255&rescale=0,255&rescale=0,255

# New default (no stretch params → StdDev k=2.0, estimated from overview):
.../14/13660/6190.png?dataset=SVN3-...FUS_3857_cog.tif

# Explicit percentile:
.../14/13660/6190.png?dataset=SVN3-...FUS_3857_cog.tif&stretch=percent&pc=2,98
```

Expected: request 1 still white (explicit bad rescale honored —
priority correct); requests 2 and 3 show visible imagery with contrast.

- [ ] **Step 3: Confirm the no-regression case**

Request a known u8 RGB fixture tile with no params and confirm it
renders exactly as before (visually unchanged):

```
.../<z>/<x>/<y>.png?dataset=cog_uint8_rgb_mask.tif
```

- [ ] **Step 4: Final commit (docs/notes only if anything changed)**

```bash
git add -A && git commit -m "chore: verify stretch modes against real datasets" --allow-empty
```

---

## Self-Review

**1. Spec coverage:**
- Four modes (Custom/MinMax/Percent/StdDev) → Tasks 1, 5, 7, 9. ✔
- Param scheme + priority (`rescale` > `stretch` > default StdDev2.0) → Task 7 (`resolved_stretch`) + Task 9 (`effective_compiled` priority ladder). ✔
- u8 keeps fast path / no default → Task 9 Step 1 short-circuit + Task 10 guard. ✔
- Stats source: embedded tags (Exact) → overview (Estimated) → Task 4 + Task 8. ✔
- No overview → fallback dtype/per-tile → Task 6 + Task 9 (`None` arm). ✔
- Lazy OnceCell on Tiler, never for Custom → Task 9 (`dataset_stats`, early return before `dataset_stats()`). ✔
- Histogram percentile → Task 2. ✔
- nodata excluded from stats → Task 3 kernel + Task 6 float fallback. ✔
- Single-band Gradient honors stretch (vmin/vmax) → Task 9 Step 1 (sets `composite.vmin/vmax`). ✔
- Invalid params → 400 → `AgError::query` (Tasks 1, 7); server already maps `Err` → BAD_REQUEST. ✔
- `X-Stats-Provenance` response header — **deferred**: the spec lists it as informative; `DatasetStats.provenance` is computed and available (Task 8) but not surfaced via HTTP. Not on the critical path; tracked here as a known omission rather than adding a server-handler task that would touch response plumbing outside this subsystem.

**2. Placeholder scan:** No TBD/TODO. Every code step has complete code. The only conditional ("if `samples_per_pixel()` mismatches") names the verified async-tiff 0.3.0 accessors and gives the exact fix.

**3. Type consistency:** `StretchSpec` (Task 1) used identically in Tasks 5/7/9. `DatasetStats`/`BandStat`/`Histogram`/`Provenance` (Tasks 2-3) used with matching fields in Tasks 4/5/8/9. `resolve_rescale(&spec,&stats,&bidx)` and `fallback_rescale(&pixels,nodata,&bidx)` signatures match their call sites in Task 9. `effective_compiled` clones `CompiledTileStyle` (fields `rescale`, `composite.vmin/vmax`, `auto_rgb`, `bidx`) — all confirmed against `src/render/pipeline.rs` and `src/render/composite.rs`.

Decision logged: `X-Stats-Provenance` header intentionally deferred (see coverage note). All other spec requirements have implementing tasks.
