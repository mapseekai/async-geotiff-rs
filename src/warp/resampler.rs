//! Resampler kernels for warp interpolation.

use ndarray::ArrayView2;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Resampling {
    #[default]
    Nearest,
    Bilinear,
    /// Catmull-Rom bicubic (4-tap). Sharper than bilinear, sharper than
    /// Mitchell, default for most XYZ tile servers.
    Cubic,
    /// Mitchell-Netravali bicubic (4-tap, B=1/3 C=1/3). Smoother than
    /// Catmull-Rom — slightly blurrier but less ringing on edges.
    /// Matches titiler's `resampling=cubic_spline`.
    CubicSpline,
    /// Lanczos3 windowed sinc (6-tap). Highest quality, slowest of the
    /// SIMD kernels. Matches titiler's `resampling=lanczos`.
    Lanczos,
    /// Box-filter mean over the source footprint a destination pixel
    /// covers: correct averaging antialiasing when downsampling, but
    /// degrades to nearest-neighbour when upsampling (same behaviour as
    /// GDAL / titiler `resampling=average`).
    Average,
}

impl Resampling {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "nearest" => Some(Self::Nearest),
            "bilinear" => Some(Self::Bilinear),
            "cubic" | "bicubic" => Some(Self::Cubic),
            "cubic_spline" | "mitchell" => Some(Self::CubicSpline),
            "lanczos" | "lanczos3" => Some(Self::Lanczos),
            "average" => Some(Self::Average),
            _ => None,
        }
    }

    /// Sample value at fractional (col, row) coordinates.
    ///
    /// At image edges the bilinear neighbours are clamped to the last valid
    /// index instead of returning `fill`. This prevents the "white seams"
    /// that appear between adjacent XYZ tiles when a sample lands on the
    /// last column/row of the read window.
    #[inline]
    pub fn sample(self, view: ArrayView2<f64>, col: f64, row: f64, fill: f64) -> f64 {
        self.sample_masked(view, None, col, row, fill)
    }

    /// Like [`sample`] but bilinear/cubic skip neighbours marked invalid in
    /// `valid` (and renormalises the remaining weights). When all 4
    /// neighbours are invalid, returns `fill`. Matches GDAL `gdalwarp -wo
    /// SAMPLE_GRID=YES` semantics for nodata-aware interpolation.
    #[inline]
    pub fn sample_masked(
        self,
        view: ArrayView2<f64>,
        valid: Option<ArrayView2<bool>>,
        col: f64,
        row: f64,
        fill: f64,
    ) -> f64 {
        let (h, w) = (view.shape()[0] as i64, view.shape()[1] as i64);
        if w == 0 || h == 0 {
            return fill;
        }
        if col < -0.5 || row < -0.5 || col > (w - 1) as f64 + 0.5 || row > (h - 1) as f64 + 0.5 {
            return fill;
        }
        let cf = col.clamp(0.0, (w - 1) as f64);
        let rf = row.clamp(0.0, (h - 1) as f64);

        // Fast path: a band view sliced out of a C-order `(bands,H,W)`
        // array is standard-contiguous, so `as_slice()` is `Some` and we
        // can index the flat buffer directly — one fused mul-add instead
        // of ndarray's per-access stride dot-product + bounds check. Falls
        // back to strided `[[r,c]]` indexing when the view is non-contiguous
        // (e.g. a `permuted_axes` zero-copy reorder).
        let w_us = w as usize;
        let contig = view.as_slice();
        let get = |r: usize, c: usize| -> f64 {
            match contig {
                // SAFETY: callers derive `r`/`c` from `rf`/`cf`, which are
                // clamped to `[0, h-1]` / `[0, w-1]`, so `r*w_us + c` is
                // always `< h*w == contig.len()`.
                Some(s) => unsafe { *s.get_unchecked(r * w_us + c) },
                None => view[[r, c]],
            }
        };
        // NB: a matching flat-index fast path for the validity mask was
        // benchmarked and reverted — it produced 0.1% change because the
        // masked path is dominated by the reweighting branches + divide,
        // not the mask reads. Kept strided to avoid complexity for no gain.

        match self {
            Self::Nearest => {
                let c = cf.round().clamp(0.0, (w - 1) as f64) as usize;
                let r = rf.round().clamp(0.0, (h - 1) as f64) as usize;
                if let Some(m) = valid {
                    if !m[[r, c]] {
                        return fill;
                    }
                }
                get(r, c)
            }
            // Cross-CRS sampler: all non-nearest kernels share the 4-tap
            // bilinear path (PROJ error dominates; the SIMD wide kernels
            // are exercised only in the same-CRS / subtile-affine fast path).
            Self::Cubic | Self::Bilinear | Self::CubicSpline | Self::Lanczos | Self::Average => {
                let c0 = cf.floor() as usize;
                let r0 = rf.floor() as usize;
                let c1 = (c0 + 1).min(w as usize - 1);
                let r1 = (r0 + 1).min(h as usize - 1);
                let dc = cf - c0 as f64;
                let dr = rf - r0 as f64;

                let w00 = (1.0 - dc) * (1.0 - dr);
                let w01 = dc * (1.0 - dr);
                let w10 = (1.0 - dc) * dr;
                let w11 = dc * dr;
                let v00 = get(r0, c0);
                let v01 = get(r0, c1);
                let v10 = get(r1, c0);
                let v11 = get(r1, c1);

                match valid {
                    None => {
                        let top = v00 * (1.0 - dc) + v01 * dc;
                        let bot = v10 * (1.0 - dc) + v11 * dc;
                        top * (1.0 - dr) + bot * dr
                    }
                    Some(m) => {
                        // Reweighted bilinear: drop invalid neighbours,
                        // rescale remaining weights to sum to 1.
                        let (m00, m01, m10, m11) =
                            (m[[r0, c0]], m[[r0, c1]], m[[r1, c0]], m[[r1, c1]]);
                        let mut wsum = 0.0;
                        let mut vsum = 0.0;
                        if m00 {
                            wsum += w00;
                            vsum += w00 * v00;
                        }
                        if m01 {
                            wsum += w01;
                            vsum += w01 * v01;
                        }
                        if m10 {
                            wsum += w10;
                            vsum += w10 * v10;
                        }
                        if m11 {
                            wsum += w11;
                            vsum += w11 * v11;
                        }
                        if wsum > 0.0 {
                            vsum / wsum
                        } else {
                            fill
                        }
                    }
                }
            }
        }
    }

    /// Box-mean over the source footprint `[sc-rx, sc+rx] x [sr-ry, sr+ry]`.
    /// `rx`/`ry` are half-footprint sizes in source pixels (from the warp
    /// Jacobian). When the footprint is below one source pixel the result
    /// degenerates to bilinear (no pixel blocks on upsample). Averages only
    /// `valid` pixels; all-invalid (or empty box) returns `fill`.
    #[inline]
    #[allow(clippy::too_many_arguments)]
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
        // PROJ can emit NaN/inf for unmapped pixels; treat as no data.
        if !sc.is_finite() || !sr.is_finite() {
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
        let mut count = 0usize;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use ndarray::array;

    #[test]
    fn nearest_at_pixel_corners() {
        let a = array![[1.0, 2.0], [3.0, 4.0]];
        assert_eq!(Resampling::Nearest.sample(a.view(), 0.0, 0.0, 0.0), 1.0);
        assert_eq!(Resampling::Nearest.sample(a.view(), 1.0, 1.0, 0.0), 4.0);
    }

    #[test]
    fn bilinear_midpoint_averages() {
        let a = array![[0.0, 10.0], [0.0, 10.0]];
        assert_relative_eq!(Resampling::Bilinear.sample(a.view(), 0.5, 0.0, 0.0), 5.0);
    }

    #[test]
    fn out_of_bounds_returns_fill() {
        let a = array![[1.0]];
        assert_eq!(Resampling::Nearest.sample(a.view(), 5.0, 5.0, -1.0), -1.0);
    }

    #[test]
    fn parse_string() {
        assert_eq!(Resampling::parse("bilinear"), Some(Resampling::Bilinear));
        assert_eq!(Resampling::parse("cubic"), Some(Resampling::Cubic));
        assert_eq!(Resampling::parse("bicubic"), Some(Resampling::Cubic));
        assert_eq!(Resampling::parse("foo"), None);
        assert_eq!(Resampling::parse("average"), Some(Resampling::Average));
    }

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
    fn average_nan_coords_return_fill() {
        let a = ndarray::Array2::from_elem((4, 4), 5.0);
        let v = Resampling::Average.sample_average(
            a.view(), None, f64::NAN, 2.0, 2.0, 2.0, -9.0);
        assert_eq!(v, -9.0);
    }

    #[test]
    fn average_degenerates_to_bilinear() {
        let a = array![[0.0, 10.0], [0.0, 10.0]];
        let avg = Resampling::Average.sample_average(a.view(), None, 0.5, 0.0, 0.3, 0.3, 0.0);
        let bil = Resampling::Bilinear.sample_masked(a.view(), None, 0.5, 0.0, 0.0);
        assert_eq!(avg, bil); // sub-1px footprint delegates to bilinear
    }
}
