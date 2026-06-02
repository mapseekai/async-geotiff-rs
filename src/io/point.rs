//! `CogReader::point` — sample full-resolution pixel value(s) at a coordinate.

use std::sync::Arc;

use crate::bounds::GeoTransform;
use crate::crs::Crs;
use crate::error::{AgError, AgResult};
use crate::io::cog::CogReader;
use crate::warp::Projector;

/// Pixel value(s) sampled at a coordinate.
#[derive(Debug, Clone, PartialEq)]
pub struct PointValue {
    /// One value per requested band, in `bidx` order (or all bands in native
    /// order when `bidx` is empty). Raw decoded values cast to f64 — scale/
    /// offset is NOT applied.
    pub values: Vec<f64>,
    /// `false` when the pixel is masked or any returned band equals the dataset
    /// nodata sentinel; `true` otherwise.
    pub valid: bool,
}

/// Reproject `(x, y)` from `coord_crs` to `src_crs`, map to an integer level-0
/// pixel via `transform.inverse()`, and bounds-check against `(width, height)`.
/// Returns `Ok(None)` if the point is outside the extent or the projection's
/// valid domain. `Projector` is `!Send`; this fn is fully synchronous so the
/// projector never crosses an await.
fn coord_to_pixel(
    x: f64,
    y: f64,
    coord_crs: &Crs,
    src_crs: &Crs,
    transform: GeoTransform,
    width: u32,
    height: u32,
) -> AgResult<Option<(i64, i64)>> {
    // 1) Reproject to source CRS (skip when equal). Projector is !Send but we
    //    never await here, so it is created and dropped within this fn.
    let (sx, sy) = if coord_crs == src_crs {
        (x, y)
    } else {
        let mut pts = [(x, y)];
        match Projector::new(coord_crs, src_crs)?.convert(&mut pts) {
            Ok(()) => pts[0],
            Err(AgError::ProjectionDomain(_)) => return Ok(None),
            Err(e) => return Err(e),
        }
    };

    // 2) World → fractional pixel.
    let inv = transform
        .inverse()
        .ok_or_else(|| AgError::Warp("source GeoTransform not invertible".into()))?;
    let (col_f, row_f) = inv.forward(sx, sy);

    // A non-finite projected coord (e.g. a pathological projection result not
    // flagged as ProjectionDomain, or a NaN input) would cast to pixel 0 via
    // `NaN as i64`, so treat it as outside coverage.
    if !col_f.is_finite() || !row_f.is_finite() {
        return Ok(None);
    }

    // 3) Floor to integer pixel, bounds-check.
    let col = col_f.floor() as i64;
    let row = row_f.floor() as i64;
    if col < 0 || row < 0 || col >= i64::from(width) || row >= i64::from(height) {
        return Ok(None);
    }
    Ok(Some((col, row)))
}

impl CogReader {
    /// Sample the full-resolution (level 0) pixel value(s) at `(x, y)`.
    ///
    /// `coord_crs` is the CRS of the input coordinate (pass `Crs::epsg(4326)`
    /// for lon/lat); it is reprojected to the dataset's source CRS internally.
    /// `bidx` is a 1-indexed band selection; an empty slice returns all bands
    /// in native order. Values are raw decoded samples cast to f64 (no scale/
    /// offset applied).
    ///
    /// Returns `Ok(None)` when the coordinate is outside the dataset extent or
    /// the projection's valid domain. Returns `Err(AgError::query)` when a band
    /// index is 0 or exceeds the band count.
    pub async fn point(
        &self,
        x: f64,
        y: f64,
        coord_crs: &Crs,
        bidx: &[u32],
    ) -> AgResult<Option<PointValue>> {
        let (width, height) = self.level_dimensions(0)?;

        // Validate band selection up front (before any read). 1-indexed.
        let total = self.data_ifd(0)?.samples_per_pixel() as usize;
        let sel: Vec<usize> = if bidx.is_empty() {
            (0..total).collect()
        } else {
            let mut out = Vec::with_capacity(bidx.len());
            for &b in bidx {
                if b == 0 || b as usize > total {
                    return Err(AgError::query(
                        "bidx",
                        format!("band {b} out of range for {total} bands"),
                    ));
                }
                out.push(b as usize - 1);
            }
            out
        };

        // Reproject + map to pixel (synchronous; Projector dropped before await).
        let (col, row) = match coord_to_pixel(
            x,
            y,
            coord_crs,
            self.src_crs(),
            self.transform(),
            width,
            height,
        )? {
            Some(p) => p,
            None => return Ok(None),
        };

        // Read the 1x1 window at full resolution.
        let image: Arc<crate::pixel::ImageData> = self.read_window(0, col, row, 1, 1).await?;
        let pixels = image.pixels.to_f64(); // (bands, 1, 1)
        let bands = pixels.shape()[0];

        let mut values = Vec::with_capacity(sel.len());
        for &b in &sel {
            // Defense-in-depth: user `bidx` was already validated against
            // samples_per_pixel before the read. This guards only against a
            // decoded band count that disagrees with the IFD (corrupt file).
            if b >= bands {
                return Err(AgError::query(
                    "bidx",
                    format!("band {} out of range for {} decoded bands", b + 1, bands),
                ));
            }
            values.push(pixels[[b, 0, 0]]);
        }

        // Validity: mask says invalid, or any selected band hits nodata.
        let mut valid = true;
        if let Some(m) = &image.mask {
            if !m[[0, 0]] {
                valid = false;
            }
        }
        if valid {
            if let Some(nd) = image.nodata {
                let hit = if nd.is_nan() {
                    values.iter().any(|v| v.is_nan())
                } else {
                    values.iter().any(|v| (v - nd).abs() < 1e-9)
                };
                if hit {
                    valid = false;
                }
            }
        }

        Ok(Some(PointValue { values, valid }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bounds::GeoTransform;
    use crate::crs::Crs;

    // origin (0,0), 1 unit/pixel, north-up: world (X,Y) → pixel (col=X, row=-Y).
    fn tr() -> GeoTransform {
        GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 0.0, 0.0, -1.0])
    }

    #[test]
    fn maps_interior_coord_to_pixel_same_crs() {
        let crs = Crs::epsg(3857);
        let p = coord_to_pixel(3.5, -2.5, &crs, &crs, tr(), 10, 10).unwrap();
        assert_eq!(p, Some((3, 2)));
    }

    #[test]
    fn out_of_extent_returns_none() {
        let crs = Crs::epsg(3857);
        let p = coord_to_pixel(50.0, -50.0, &crs, &crs, tr(), 10, 10).unwrap();
        assert_eq!(p, None);
        let p2 = coord_to_pixel(-5.0, 5.0, &crs, &crs, tr(), 10, 10).unwrap();
        assert_eq!(p2, None);
    }

    #[test]
    fn pixel_at_origin_is_zero_zero() {
        let crs = Crs::epsg(3857);
        let p = coord_to_pixel(0.5, -0.5, &crs, &crs, tr(), 10, 10).unwrap();
        assert_eq!(p, Some((0, 0)));
    }

    #[test]
    fn non_finite_projected_coord_is_none() {
        let crs = Crs::epsg(3857);
        let p = coord_to_pixel(f64::NAN, 0.0, &crs, &crs, tr(), 10, 10).unwrap();
        assert_eq!(p, None);
    }

    // ---- integration tests against a local fixture ----

    async fn rgb_reader() -> CogReader {
        use async_tiff::reader::ObjectReader;
        use object_store::local::LocalFileSystem;
        use object_store::path::Path;
        let fs = std::sync::Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        CogReader::open_with_reader(ObjectReader::new(
            fs,
            Path::from("data/cog_uint8_rgb_nodata.tif"),
        ))
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn point_inside_returns_all_bands() {
        let r = rgb_reader().await;
        let [w, s, e, n] = r.wgs84_bounds().unwrap();
        let (lon, lat) = ((w + e) / 2.0, (s + n) / 2.0);
        let pv = r
            .point(lon, lat, &Crs::epsg(4326), &[])
            .await
            .unwrap()
            .expect("center point should be inside");
        assert_eq!(pv.values.len(), 3);
        for v in &pv.values {
            assert!((0.0..=255.0).contains(v), "u8 band value out of range: {v}");
        }
    }

    #[tokio::test]
    async fn point_bidx_selects_and_orders_bands() {
        let r = rgb_reader().await;
        let [w, s, e, n] = r.wgs84_bounds().unwrap();
        let (lon, lat) = ((w + e) / 2.0, (s + n) / 2.0);
        let all = r
            .point(lon, lat, &Crs::epsg(4326), &[])
            .await
            .unwrap()
            .unwrap();
        let one = r
            .point(lon, lat, &Crs::epsg(4326), &[1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(one.values.len(), 1);
        assert_eq!(one.values[0], all.values[0]);
        let rev = r
            .point(lon, lat, &Crs::epsg(4326), &[3, 1])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rev.values, vec![all.values[2], all.values[0]]);
    }

    #[tokio::test]
    async fn point_bad_band_index_is_query_error() {
        let r = rgb_reader().await;
        let [w, s, e, n] = r.wgs84_bounds().unwrap();
        let (lon, lat) = ((w + e) / 2.0, (s + n) / 2.0);
        assert!(matches!(
            r.point(lon, lat, &Crs::epsg(4326), &[0]).await,
            Err(AgError::Query { .. })
        ));
        assert!(matches!(
            r.point(lon, lat, &Crs::epsg(4326), &[99]).await,
            Err(AgError::Query { .. })
        ));
    }

    #[tokio::test]
    async fn point_reprojects_lonlat_onto_non_4326_dataset() {
        // Fixture is EPSG:3857; querying in lon/lat (4326) forces the
        // reprojection branch in `coord_to_pixel` (Projector::new + convert),
        // unlike the 4326 fixture where the equal-CRS short-circuit fires.
        use async_tiff::reader::ObjectReader;
        use object_store::local::LocalFileSystem;
        use object_store::path::Path;
        let fs = std::sync::Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        let r = CogReader::open_with_reader(ObjectReader::new(
            fs,
            Path::from("data/output_3857_cog.tif"),
        ))
        .await
        .unwrap();
        assert_ne!(r.src_crs(), &Crs::epsg(4326), "fixture should be non-4326");
        let [w, s, e, n] = r.wgs84_bounds().unwrap();
        let (lon, lat) = ((w + e) / 2.0, (s + n) / 2.0);
        let pv = r
            .point(lon, lat, &Crs::epsg(4326), &[])
            .await
            .unwrap()
            .expect("center lon/lat should reproject to an interior pixel");
        assert!(!pv.values.is_empty());
    }

    #[tokio::test]
    async fn point_outside_extent_is_none() {
        let r = rgb_reader().await;
        let [_w, _s, _e, n] = r.wgs84_bounds().unwrap();
        // A latitude well north of the dataset's northern edge (clamped ≤ 90).
        let lat = (n + 5.0).min(89.9);
        let pv = r.point(0.0, lat, &Crs::epsg(4326), &[]).await.unwrap();
        assert!(pv.is_none(), "point north of the extent must be None");
    }

    #[tokio::test]
    async fn point_validity_flag_is_consistent_and_masks_nodata() {
        let r = rgb_reader().await;
        let [w, s, e, n] = r.wgs84_bounds().unwrap();
        // Sample a 20x20 grid over the dataset. Every returned PointValue must
        // have 3 values. The fixture has nodata around its SW corner, so at
        // least one returned point must be invalid — this strictly exercises
        // the nodata→invalid masking path (a regression that always returned
        // valid=true would fail here).
        let mut saw_any = false;
        let mut saw_invalid = false;
        for i in 0..20 {
            for j in 0..20 {
                let lon = w + (e - w) * (i as f64 + 0.5) / 20.0;
                let lat = s + (n - s) * (j as f64 + 0.5) / 20.0;
                if let Some(pv) = r.point(lon, lat, &Crs::epsg(4326), &[]).await.unwrap() {
                    saw_any = true;
                    assert_eq!(pv.values.len(), 3);
                    if !pv.valid {
                        saw_invalid = true;
                    }
                }
            }
        }
        assert!(saw_any, "at least one interior grid point should be inside");
        assert!(
            saw_invalid,
            "fixture has nodata; at least one grid point must be masked invalid"
        );
    }
}
