//! Direct EPSG:4326 ↔ EPSG:3857 conversion, bypassing PROJ.
//!
//! For the WebMercator viewer case (90%+ of XYZ traffic), the source COG is
//! often EPSG:4326 lon/lat or EPSG:3857 metres and the destination tile grid
//! is EPSG:3857. PROJ's `new_known_crs` + `convert_array` pipeline costs
//! ~100-300 ns per point including FFI crossings and pipeline interpretation.
//! For our 33×33 control grid that's ~150-300 µs per cross-CRS tile.
//!
//! The closed-form Web Mercator formula is well-defined and stable; reading
//! straight from the EPSG dataset (which PROJ also implements) gives:
//!
//! ```text
//! 4326 → 3857:   x = R · λ,           y = R · ln(tan(π/4 + φ/2))
//! 3857 → 4326:   λ = x / R,           φ = 2·atan(exp(y/R)) − π/2
//!     where R = 6378137 (sphere radius, EPSG:3857 uses spherical maths
//!     on top of the WGS-84 datum) and (λ, φ) are in radians.
//! ```
//!
//! Latitudes outside ±85.0511287° map to ±∞ on the y axis; we clamp before
//! `tan().ln()` to match PROJ's behaviour at the WebMercator pole limits.

use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};

/// Equatorial radius used by EPSG:3857 (spherical Mercator on WGS-84 datum).
const R: f64 = 6_378_137.0;
const DEG_TO_RAD: f64 = PI / 180.0;
const RAD_TO_DEG: f64 = 180.0 / PI;
/// `atan(sinh(π))` in degrees — the latitude that maps to y = ±R·π in
/// WebMercator, beyond which the projection is undefined.
const MAX_LAT_DEG: f64 = 85.051_128_779_806_59;

/// In-place batch conversion `(lon°, lat°) → (x_m, y_m)` (4326 → 3857).
///
/// Inputs are interpreted in PROJ's `new_known_crs("EPSG:4326", ...)` axis
/// order, i.e. `(x = lon, y = lat)` — which `crs.rs` already documents and
/// the rest of the codebase already follows.
pub fn lonlat_to_mercator(pts: &mut [(f64, f64)]) {
    for p in pts.iter_mut() {
        let lon = p.0;
        let lat = p.1.clamp(-MAX_LAT_DEG, MAX_LAT_DEG);
        let lat_rad = lat * DEG_TO_RAD;
        p.0 = R * lon * DEG_TO_RAD;
        p.1 = R * (FRAC_PI_4 + lat_rad * 0.5).tan().ln();
    }
}

/// In-place batch conversion `(x_m, y_m) → (lon°, lat°)` (3857 → 4326).
pub fn mercator_to_lonlat(pts: &mut [(f64, f64)]) {
    for p in pts.iter_mut() {
        let x = p.0;
        let y = p.1;
        p.0 = (x / R) * RAD_TO_DEG;
        p.1 = (2.0 * (y / R).exp().atan() - FRAC_PI_2) * RAD_TO_DEG;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn origin_maps_to_origin() {
        let mut pts = [(0.0, 0.0)];
        lonlat_to_mercator(&mut pts);
        assert_relative_eq!(pts[0].0, 0.0, epsilon = 1e-9);
        assert_relative_eq!(pts[0].1, 0.0, epsilon = 1e-9);
    }

    #[test]
    fn equator_x_scales_with_lon() {
        let mut pts = [(180.0, 0.0)];
        lonlat_to_mercator(&mut pts);
        assert_relative_eq!(pts[0].0, R * PI, epsilon = 1e-6);
        assert_relative_eq!(pts[0].1, 0.0, epsilon = 1e-6);
    }

    #[test]
    fn pole_clamps_to_max_lat() {
        let mut pts = [(0.0, 89.9)];
        lonlat_to_mercator(&mut pts);
        // y at MAX_LAT_DEG is R*π (the WebMercator "pole" cap).
        assert_relative_eq!(pts[0].1, R * PI, epsilon = 1.0);
    }

    #[test]
    fn roundtrip_preserves_points() {
        let mut pts = [
            (0.0, 0.0),
            (116.4, 39.9),    // Beijing
            (-73.98, 40.75),  // NYC
            (151.21, -33.87), // Sydney
            (179.99, 70.0),
        ];
        let original = pts;
        lonlat_to_mercator(&mut pts);
        mercator_to_lonlat(&mut pts);
        for (got, want) in pts.iter().zip(original.iter()) {
            assert_relative_eq!(got.0, want.0, epsilon = 1e-9);
            assert_relative_eq!(got.1, want.1, epsilon = 1e-9);
        }
    }

    #[cfg(feature = "proj")]
    #[test]
    fn matches_proj_within_1mm() {
        use crate::crs::Crs;
        use proj::Proj;
        let p = Proj::new_known_crs("EPSG:4326", "EPSG:3857", None).unwrap();
        let mut samples = vec![
            (0.0, 0.0),
            (116.4, 39.9),
            (-73.98, 40.75),
            (151.21, -33.87),
            (-122.42, 37.77),
            (10.0, 60.0),
            (-30.0, -45.0),
        ];
        let mut expected = samples.clone();
        p.convert_array(&mut expected).unwrap();
        lonlat_to_mercator(&mut samples);
        for (got, want) in samples.iter().zip(expected.iter()) {
            // 1 mm tolerance — PROJ's spherical Mercator uses the same R.
            assert_relative_eq!(got.0, want.0, epsilon = 1e-3);
            assert_relative_eq!(got.1, want.1, epsilon = 1e-3);
        }
        let _ = Crs::epsg(4326);
    }

    #[cfg(feature = "proj")]
    #[test]
    fn inverse_matches_proj_within_1nrad() {
        use proj::Proj;
        let p = Proj::new_known_crs("EPSG:3857", "EPSG:4326", None).unwrap();
        let mut samples = vec![(0.0, 0.0), (R * PI, R * PI / 2.0), (-1e7, 5e6), (2e6, -3e6)];
        let mut expected = samples.clone();
        p.convert_array(&mut expected).unwrap();
        mercator_to_lonlat(&mut samples);
        for (got, want) in samples.iter().zip(expected.iter()) {
            assert_relative_eq!(got.0, want.0, epsilon = 1e-7);
            assert_relative_eq!(got.1, want.1, epsilon = 1e-7);
        }
    }
}
