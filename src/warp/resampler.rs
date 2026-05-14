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
    pub fn sample(self, view: ArrayView2<f64>, col: f64, row: f64, fill: f64) -> f64 {
        let (h, w) = (view.shape()[0] as i64, view.shape()[1] as i64);
        match self {
            Self::Nearest => {
                let c = col.round() as i64;
                let r = row.round() as i64;
                if c < 0 || r < 0 || c >= w || r >= h {
                    fill
                } else {
                    view[[r as usize, c as usize]]
                }
            }
            Self::Bilinear => {
                let c0 = col.floor() as i64;
                let r0 = row.floor() as i64;
                let c1 = c0 + 1;
                let r1 = r0 + 1;
                if c0 < 0 || r0 < 0 || c1 >= w || r1 >= h {
                    return fill;
                }
                let dc = col - c0 as f64;
                let dr = row - r0 as f64;
                let v00 = view[[r0 as usize, c0 as usize]];
                let v01 = view[[r0 as usize, c1 as usize]];
                let v10 = view[[r1 as usize, c0 as usize]];
                let v11 = view[[r1 as usize, c1 as usize]];
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
