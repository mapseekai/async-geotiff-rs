use raffine::Affine;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bbox {
    pub xmin: f64,
    pub ymin: f64,
    pub xmax: f64,
    pub ymax: f64,
}

impl Bbox {
    pub fn new(xmin: f64, ymin: f64, xmax: f64, ymax: f64) -> Self {
        Self {
            xmin,
            ymin,
            xmax,
            ymax,
        }
    }
    pub fn width(&self) -> f64 {
        self.xmax - self.xmin
    }
    pub fn height(&self) -> f64 {
        self.ymax - self.ymin
    }

    /// Bbox enclosing all four transformed corners.
    pub fn from_corners(corners: &[(f64, f64); 4]) -> Self {
        let mut xmin = f64::INFINITY;
        let mut ymin = f64::INFINITY;
        let mut xmax = f64::NEG_INFINITY;
        let mut ymax = f64::NEG_INFINITY;
        for &(x, y) in corners {
            if x < xmin {
                xmin = x;
            }
            if y < ymin {
                ymin = y;
            }
            if x > xmax {
                xmax = x;
            }
            if y > ymax {
                ymax = y;
            }
        }
        Self {
            xmin,
            ymin,
            xmax,
            ymax,
        }
    }
}

/// Newtype wrapping `raffine::Affine` as a GeoTransform.
///
/// `from_gdal(&[c, a, b, f, d, e])` accepts the same 6-tuple format used by GDAL
/// and rasterio. Forward maps pixel → world; `inverse()` maps world → pixel.
#[derive(Debug, Clone, Copy)]
pub struct GeoTransform(pub Affine);

impl GeoTransform {
    pub fn from_gdal(gt: &[f64; 6]) -> Self {
        Self(Affine::from_gdal(gt))
    }

    #[inline]
    pub fn forward(&self, col: f64, row: f64) -> (f64, f64) {
        self.0 * (col, row)
    }

    pub fn translated(&self, col_off: f64, row_off: f64) -> Self {
        Self(self.0 * Affine::translation(col_off, row_off))
    }

    pub fn scaled(&self, sx: f64, sy: f64) -> Self {
        Self(self.0 * Affine::scale(sx, Some(sy)))
    }

    pub fn inverse(&self) -> Option<GeoTransform> {
        (!self.0).ok().map(GeoTransform)
    }

    /// Pixel window covering the bbox.
    /// Returns `(col_off, row_off, width, height)`.
    pub fn window_for_bbox(&self, bbox: &Bbox) -> Option<(i64, i64, u64, u64)> {
        let inv = self.inverse()?;
        let (c0, r0) = inv.forward(bbox.xmin, bbox.ymax);
        let (c1, r1) = inv.forward(bbox.xmax, bbox.ymin);
        let (c2, r2) = inv.forward(bbox.xmin, bbox.ymin);
        let (c3, r3) = inv.forward(bbox.xmax, bbox.ymax);
        // Integer pixel cover of the projected footprint is
        // [floor(min), ceil(max)). Using ceil(max-min) (the raw span)
        // undercounts by 1 px whenever both endpoints are fractional —
        // the normal post-reprojection case — leaving the tile's
        // right/bottom edge unread → transparent seams between tiles,
        // amplified at coarse overviews by the upsample factor.
        let cmin = c0.min(c1).min(c2).min(c3);
        let cmax = c0.max(c1).max(c2).max(c3);
        let rmin = r0.min(r1).min(r2).min(r3);
        let rmax = r0.max(r1).max(r2).max(r3);
        let col_off = cmin.floor() as i64;
        let row_off = rmin.floor() as i64;
        let width = (cmax.ceil() - cmin.floor()) as u64;
        let height = (rmax.ceil() - rmin.floor()) as u64;
        Some((col_off, row_off, width, height))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sheared_window_covers_all_four_corners() {
        let transform = super::GeoTransform::from_gdal(&[0.0, 1.0, 0.25, 0.0, 0.0, -1.0]);
        assert_eq!(
            transform.window_for_bbox(&super::Bbox::new(10.0, -20.0, 20.0, -10.0)),
            Some((5, 10, 13, 10))
        );
    }
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn bbox_from_corners_encloses_all() {
        let b = Bbox::from_corners(&[(0.0, 0.0), (10.0, 5.0), (-1.0, 3.0), (4.0, 8.0)]);
        assert_eq!(b, Bbox::new(-1.0, 0.0, 10.0, 8.0));
    }

    #[test]
    fn geotransform_forward_inverse_roundtrip() {
        // GDAL: origin (100, 200), pixel size (1, -1)
        let gt = GeoTransform::from_gdal(&[100.0, 1.0, 0.0, 200.0, 0.0, -1.0]);
        let (x, y) = gt.forward(0.0, 0.0);
        assert_relative_eq!(x, 100.0);
        assert_relative_eq!(y, 200.0);

        let inv = gt.inverse().unwrap();
        let (c, r) = inv.forward(100.0, 200.0);
        assert_relative_eq!(c, 0.0, epsilon = 1e-9);
        assert_relative_eq!(r, 0.0, epsilon = 1e-9);
    }

    #[test]
    fn window_for_bbox_clamps_correctly() {
        let gt = GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 100.0, 0.0, -1.0]);
        let bbox = Bbox::new(10.0, 50.0, 30.0, 90.0);
        let (c, r, w, h) = gt.window_for_bbox(&bbox).unwrap();
        assert_eq!((c, r, w, h), (10, 10, 20, 40));
    }
}
