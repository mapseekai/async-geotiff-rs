//! Resampler kernels for warp interpolation.

use ndarray::ArrayView2;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Resampling {
    #[default]
    Nearest,
    Bilinear,
}

impl Resampling {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "nearest" => Some(Self::Nearest),
            "bilinear" => Some(Self::Bilinear),
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
        let (h, w) = (view.shape()[0] as i64, view.shape()[1] as i64);
        if w == 0 || h == 0 {
            return fill;
        }
        // Out-of-bounds outside the convex hull of pixel centres → fill.
        if col < -0.5 || row < -0.5 || col > (w - 1) as f64 + 0.5 || row > (h - 1) as f64 + 0.5 {
            return fill;
        }
        let cf = col.clamp(0.0, (w - 1) as f64);
        let rf = row.clamp(0.0, (h - 1) as f64);
        match self {
            Self::Nearest => {
                let c = cf.round().clamp(0.0, (w - 1) as f64) as usize;
                let r = rf.round().clamp(0.0, (h - 1) as f64) as usize;
                view[[r, c]]
            }
            Self::Bilinear => {
                let c0 = cf.floor() as usize;
                let r0 = rf.floor() as usize;
                let c1 = (c0 + 1).min(w as usize - 1);
                let r1 = (r0 + 1).min(h as usize - 1);
                let dc = cf - c0 as f64;
                let dr = rf - r0 as f64;
                let v00 = view[[r0, c0]];
                let v01 = view[[r0, c1]];
                let v10 = view[[r1, c0]];
                let v11 = view[[r1, c1]];
                let top = v00 * (1.0 - dc) + v01 * dc;
                let bot = v10 * (1.0 - dc) + v11 * dc;
                top * (1.0 - dr) + bot * dr
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
        assert_eq!(Resampling::parse("foo"), None);
    }
}
