//! Resampler kernels for warp interpolation.

use ndarray::ArrayView2;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Resampling {
    #[default]
    Nearest,
    Bilinear,
    /// Catmull-Rom bicubic (16-tap). Sharper than bilinear with negligible
    /// extra cost in `fast_image_resize`'s SIMD path; for the cross-CRS
    /// fallback sampler, falls back to bilinear (cubic is overkill there
    /// because PROJ error already dominates).
    Cubic,
}

impl Resampling {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "nearest" => Some(Self::Nearest),
            "bilinear" => Some(Self::Bilinear),
            "cubic" | "bicubic" => Some(Self::Cubic),
            _ => None,
        }
    }

    /// Sample value at fractional (col, row) coordinates.
    ///
    /// At image edges the bilinear neighbours are clamped to the last valid
    /// index instead of returning `fill`. This prevents the "white seams"
    /// that appear between adjacent XYZ tiles when a sample lands on the
    /// last column/row of the read window.
    pub fn sample(self, view: ArrayView2<f64>, col: f64, row: f64, fill: f64) -> f64 {
        self.sample_masked(view, None, col, row, fill)
    }

    /// Like [`sample`] but bilinear/cubic skip neighbours marked invalid in
    /// `valid` (and renormalises the remaining weights). When all 4
    /// neighbours are invalid, returns `fill`. Matches GDAL `gdalwarp -wo
    /// SAMPLE_GRID=YES` semantics for nodata-aware interpolation.
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
        match self {
            Self::Nearest => {
                let c = cf.round().clamp(0.0, (w - 1) as f64) as usize;
                let r = rf.round().clamp(0.0, (h - 1) as f64) as usize;
                if let Some(m) = valid {
                    if !m[[r, c]] {
                        return fill;
                    }
                }
                view[[r, c]]
            }
            // Cross-CRS cubic uses the bilinear weight kernel (PROJ error
            // dominates; a true 16-tap scalar loop adds cost for no quality).
            Self::Cubic | Self::Bilinear => {
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
                let v00 = view[[r0, c0]];
                let v01 = view[[r0, c1]];
                let v10 = view[[r1, c0]];
                let v11 = view[[r1, c1]];

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
    }
}
