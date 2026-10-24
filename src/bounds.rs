use raffine::Affine;

/// An axis-aligned rectangle in the coordinate units of its associated CRS.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bbox {
    /// Minimum x coordinate (west in longitude/latitude order).
    pub xmin: f64,
    /// Minimum y coordinate (south in longitude/latitude order).
    pub ymin: f64,
    /// Maximum x coordinate (east in longitude/latitude order).
    pub xmax: f64,
    /// Maximum y coordinate (north in longitude/latitude order).
    pub ymax: f64,
}

impl Bbox {
    /// Store rectangle coordinates without reordering or validating them.
    pub fn new(xmin: f64, ymin: f64, xmax: f64, ymax: f64) -> Self {
        Self {
            xmin,
            ymin,
            xmax,
            ymax,
        }
    }
    /// Return `xmax - xmin` in CRS units.
    pub fn width(&self) -> f64 {
        self.xmax - self.xmin
    }
    /// Return `ymax - ymin` in CRS units.
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
    /// Construct from GDAL coefficients `[x_origin, x_scale, x_skew, y_origin, y_skew, y_scale]`.
    pub fn from_gdal(gt: &[f64; 6]) -> Self {
        Self(Affine::from_gdal(gt))
    }

    /// Transform pixel coordinates, with integer coordinates at pixel corners.
    #[inline]
    pub fn forward(&self, col: f64, row: f64) -> (f64, f64) {
        self.0 * (col, row)
    }

    /// Shift the pixel-space origin by the given column and row offsets.
    pub fn translated(&self, col_off: f64, row_off: f64) -> Self {
        Self(self.0 * Affine::translation(col_off, row_off))
    }

    /// Scale pixel-space column and row coordinates before mapping to the CRS.
    pub fn scaled(&self, sx: f64, sy: f64) -> Self {
        Self(self.0 * Affine::scale(sx, Some(sy)))
    }

    /// Return the world-to-pixel transform, or `None` for a singular transform.
    pub fn inverse(&self) -> Option<GeoTransform> {
        (!self.0).ok().map(GeoTransform)
    }

    /// Pixel window covering the bbox.
    /// Returns `(col_off, row_off, width, height)`, or `None` when the transform
    /// is not invertible or the pixel endpoints cannot be represented as `i64`.
    pub fn window_for_bbox(&self, bbox: &Bbox) -> Option<(i64, i64, u64, u64)> {
        let inv = self.inverse()?;
        let (c0, r0) = inv.forward(bbox.xmin, bbox.ymax);
        let (c1, r1) = inv.forward(bbox.xmax, bbox.ymin);
        let (c2, r2) = inv.forward(bbox.xmin, bbox.ymin);
        let (c3, r3) = inv.forward(bbox.xmax, bbox.ymax);
        if [c0, r0, c1, r1, c2, r2, c3, r3]
            .iter()
            .any(|v| !v.is_finite())
        {
            return None;
        }
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
        let endpoints = [cmin.floor(), rmin.floor(), cmax.ceil(), rmax.ceil()];
        // i64::MAX rounds up to 2^63 as f64, so the upper bound is exclusive.
        if endpoints
            .iter()
            .any(|&v| v < i64::MIN as f64 || v >= i64::MAX as f64)
        {
            return None;
        }
        let [col_off, row_off, col_end, row_end] = endpoints.map(|v| v as i64);
        let width = col_end.abs_diff(col_off);
        let height = row_end.abs_diff(row_off);
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
    fn window_rejects_non_finite_and_unrepresentable_endpoints() {
        let gt = GeoTransform::from_gdal(&[0.0, 1e-18, 0.0, 0.0, 0.0, -1e-18]);
        assert!(gt
            .window_for_bbox(&Bbox::new(-180.0, -90.0, 0.0, 90.0))
            .is_none());
        let identity = GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, i64::MAX as f64] {
            assert!(identity
                .window_for_bbox(&Bbox::new(0.0, 0.0, value, 1.0))
                .is_none());
        }
    }

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
