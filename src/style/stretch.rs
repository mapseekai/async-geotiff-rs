//! QGIS-style stretch modes. `Custom` is represented by a non-empty
//! `TileStyle.rescale`; this module covers the computed modes and the
//! default (StdDev k=2.0) applied when the client passes nothing.

use crate::error::{AgError, AgResult};
use crate::io::stats::DatasetStats;
use crate::pixel::PixelData;

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
            // Require finite and positive; this also rejects NaN/inf,
            // which `k <= 0.0` alone would let through.
            if !(k.is_finite() && k > 0.0) {
                return Err(AgError::query("sigma", format!("must be > 0, got {k}")));
            }
            Ok(StretchSpec::StdDev { k })
        }
        other => Err(AgError::query("stretch", format!("unknown mode {other}"))),
    }
}

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
            let (lo, mut hi) = match spec {
                StretchSpec::MinMax => (s.min, s.max),
                StretchSpec::Percent { lo, hi } => {
                    (s.hist.quantile(*lo), s.hist.quantile(*hi))
                }
                StretchSpec::StdDev { k } => (
                    (s.mean - k * s.stddev).max(s.min),
                    (s.mean + k * s.stddev).min(s.max),
                ),
            };
            if hi <= lo || !hi.is_finite() {
                hi = lo + 1.0;
            }
            (lo, hi)
        })
        .collect()
}

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
            if hi <= lo {
                (0.0, 1.0)
            } else {
                (lo, hi)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::stats::{BandStat, DatasetStats, Histogram, Provenance};

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
        assert!(parse_stretch_kind("percent", Some((50.0, 50.0)), None).is_err()); // lo == hi
        assert!(parse_stretch_kind("percent", Some((0.0, 100.0)), None).is_ok());  // boundary ok
    }

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
        // Band index 5 is out of range for a 1-band dataset → must fall
        // back to the last (only) band's stats, not panic.
        let stats = ds(1.0, 9.0, 5.0, 1.0);
        let r = resolve_rescale(&StretchSpec::MinMax, &stats, &[0, 5]);
        assert_eq!(r.len(), 2);
        // Out-of-range band 5 reused last band → same range as band 0.
        assert_eq!(r[0], r[1]);
        assert_eq!(r[1], (1.0, 9.0));
    }

    #[test]
    fn fallback_integer_uses_dtype_range() {
        use crate::pixel::PixelData;
        use ndarray::Array3;
        let p = PixelData::U16(Array3::from_elem((1, 2, 2), 500u16));
        assert_eq!(fallback_rescale(&p, None, &[0]), vec![(0.0, 65535.0)]);
    }

    #[test]
    fn fallback_float_uses_per_tile_minmax_excluding_nodata() {
        use crate::pixel::PixelData;
        use ndarray::Array3;
        let p = PixelData::F32(
            Array3::from_shape_vec((1, 1, 4), vec![-9999.0f32, 1.0, 5.0, 9.0]).unwrap(),
        );
        let r = fallback_rescale(&p, Some(-9999.0), &[0]);
        assert_eq!(r, vec![(1.0, 9.0)]);
    }
}
