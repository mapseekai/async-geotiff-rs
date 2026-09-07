//! Coarse-grid warp approximation.
//!
//! Sample a 32×32 grid in destination pixel space through PROJ once (batched);
//! bilinear-interpolate intermediate dst→src pixel coords; sample source pixels
//! with the chosen kernel.

use ndarray::{Array2, Array3};

use crate::bounds::{Bbox, GeoTransform};
use crate::crs::Crs;
use crate::error::{AgError, AgResult};
use crate::pixel::{ImageData, PixelData};
use crate::warp::resampler::Resampling;

const WARP_EDGE_EPS: f64 = 1e-9;
const WARP_GRID_SIZE: usize = 32;
const SUBTILE_MAX_ERR_PX: f64 = 0.33;

/// Source-pixel coords `(sc, sr)` for every dst column of dst row `r`,
/// using the warp grid, converted from corner coordinates to sample centers.
fn row_src_coords(
    r: usize,
    dst_h: usize,
    dst_w: usize,
    col_table: &[(usize, usize, f64)],
    grid_src_pix: &[(f64, f64)],
) -> Vec<(f64, f64)> {
    let gi = (r as f64 + 0.5) * (WARP_GRID_SIZE as f64) / (dst_h as f64);
    let i0 = (gi.floor() as usize).min(WARP_GRID_SIZE - 1);
    let i1 = i0 + 1;
    let di = gi - i0 as f64;
    let row_i0 = i0 * (WARP_GRID_SIZE + 1);
    let row_i1 = i1 * (WARP_GRID_SIZE + 1);
    let mut out = Vec::with_capacity(dst_w);
    for &(j0, j1, dj) in col_table.iter() {
        let p00 = grid_src_pix[row_i0 + j0];
        let p01 = grid_src_pix[row_i0 + j1];
        let p10 = grid_src_pix[row_i1 + j0];
        let p11 = grid_src_pix[row_i1 + j1];
        let w00 = (1.0 - di) * (1.0 - dj);
        let w01 = (1.0 - di) * dj;
        let w10 = di * (1.0 - dj);
        let w11 = di * dj;
        let sc = p00.0 * w00 + p01.0 * w01 + p10.0 * w10 + p11.0 * w11;
        let sr = p00.1 * w00 + p01.1 * w01 + p10.1 * w10 + p11.1 * w11;
        // GeoTransform uses pixel corners; samplers use integer centers.
        out.push((sc - 0.5, sr - 0.5));
    }
    out
}

pub fn warp(
    src: &ImageData,
    dst_crs: &Crs,
    dst_bbox: Bbox,
    dst_w: usize,
    dst_h: usize,
    resampling: Resampling,
) -> AgResult<ImageData> {
    let source_shape = (src.pixels.bands(), src.pixels.height(), src.pixels.width());
    if dst_w == 0
        || dst_h == 0
        || source_shape.0 == 0
        || source_shape.1 == 0
        || source_shape.2 == 0
        || src
            .mask
            .as_ref()
            .is_some_and(|m| m.dim() != (source_shape.1, source_shape.2))
        || ![dst_bbox.xmin, dst_bbox.ymin, dst_bbox.xmax, dst_bbox.ymax]
            .iter()
            .all(|v| v.is_finite())
        || dst_bbox.width() <= 0.0
        || dst_bbox.height() <= 0.0
    {
        return Err(AgError::Warp(
            "invalid source shape, mask, or target grid".into(),
        ));
    }
    let dst_transform = GeoTransform::from_gdal(&[
        dst_bbox.xmin,
        dst_bbox.width() / dst_w as f64,
        0.0,
        dst_bbox.ymax,
        0.0,
        -dst_bbox.height() / dst_h as f64,
    ]);

    let src_inv = src
        .transform
        .inverse()
        .ok_or_else(|| AgError::Warp("src GeoTransform not invertible".into()))?;

    // Fast path: same CRS — SIMD-accelerated crop+resize via
    // fast_image_resize. Only when the tile maps *fully inside* the source
    // image: warp_affine clamps its CropBox to the image and resizes that
    // into the full dst tile, so a boundary tile (mapping partly outside
    // the COG) would stretch the valid strip across the whole tile. Such
    // tiles fall through to the scalar grid path below, which returns
    // transparent fill for out-of-bounds source coords. WARP_EDGE_EPS
    // permits only numerical rounding, not a cropped half-pixel halo.
    let origin = src.transform.forward(0.0, 0.0);
    let col_axis = src.transform.forward(1.0, 0.0);
    let row_axis = src.transform.forward(0.0, 1.0);
    let north_up = col_axis.0 > origin.0
        && row_axis.1 < origin.1
        && col_axis.1 == origin.1
        && row_axis.0 == origin.0;
    let can_resize = matches!(src.pixels, PixelData::U8(_) | PixelData::U16(_))
        && matches!(resampling, Resampling::Nearest)
        && src.nodata.is_none();
    if &src.crs == dst_crs && north_up && can_resize {
        let (lx, ty) = src_inv.forward(dst_bbox.xmin, dst_bbox.ymax);
        let (rx, by) = src_inv.forward(dst_bbox.xmax, dst_bbox.ymin);
        let (x0, x1) = (lx.min(rx), lx.max(rx));
        let (y0, y1) = (ty.min(by), ty.max(by));
        let sw = src.pixels.width() as f64;
        let sh = src.pixels.height() as f64;
        let fully_in_bounds = x0 >= -WARP_EDGE_EPS
            && y0 >= -WARP_EDGE_EPS
            && x1 <= sw + WARP_EDGE_EPS
            && y1 <= sh + WARP_EDGE_EPS;
        if fully_in_bounds && !matches!(resampling, Resampling::Mode) {
            return warp_affine(
                src,
                dst_crs,
                dst_bbox,
                dst_w,
                dst_h,
                resampling,
                src_inv,
                dst_transform,
            );
        }
    }

    // Cross-CRS path: coarse 32×32 grid through PROJ + bilinear interpolation
    // of intermediate dst→src pixel coords. The Projector is fetched from a
    // thread-local cache (see `projection::with_cached`) so back-to-back
    // tile requests on the same worker thread skip PROJ context init.
    let mut grid_dst: Vec<(f64, f64)> =
        Vec::with_capacity((WARP_GRID_SIZE + 1) * (WARP_GRID_SIZE + 1));
    for i in 0..=WARP_GRID_SIZE {
        for j in 0..=WARP_GRID_SIZE {
            let dx = dst_bbox.xmin + (j as f64) * dst_bbox.width() / (WARP_GRID_SIZE as f64);
            let dy = dst_bbox.ymax - (i as f64) * dst_bbox.height() / (WARP_GRID_SIZE as f64);
            grid_dst.push((dx, dy));
        }
    }
    crate::warp::projection::convert_batch_partial(dst_crs, &src.crs, &mut grid_dst)?;

    let grid_src_pix: Vec<(f64, f64)> = grid_dst
        .into_iter()
        .map(|(x, y)| src_inv.forward(x, y))
        .collect();

    // -- 方案2 fast path --
    // If the whole-tile mapping is well-approximated by an axis-aligned
    // rectangle in src pixel space (true for 4326↔3857 in mid-latitudes
    // over one XYZ tile), reuse the same-CRS warp_affine SIMD path with a
    // CropBox derived from the grid corners. We accept the path when the
    // max residual at the interior grid samples is below 0.5 src pixels —
    // i.e. sub-pixel error, invisible after resampling.
    if can_resize && (&src.crs != dst_crs || north_up) && !matches!(resampling, Resampling::Mode) {
        if let Some(out) = try_subtile_affine(
            src,
            dst_crs,
            dst_bbox,
            dst_w,
            dst_h,
            resampling,
            dst_transform,
            &grid_src_pix,
            WARP_GRID_SIZE,
        )? {
            return Ok(out);
        }
    }

    // -- Scalar fallback (cross-CRS, not approximable by single rectangle) --
    // Optimisations vs. the previous nested triple loop:
    //   1. Per-column (sc, sr, j0/dj) are precomputed once per row, not per
    //      band — the original layout recomputed grid coords inside the
    //      band loop and re-took `src_f64.index_axis(b)` on every pixel.
    //   2. Per-band views (`src_f64.index_axis`) are hoisted outside the
    //      pixel loop — one view per band per row.
    //   3. Output rows are independent → `par_chunks_mut` over rayon.
    //   4. The mask is computed once per pixel (not per band) and read from
    //      `src.mask` only at the row level.
    let bands = src.pixels.bands();
    let src_f64 = src.pixels.as_f64();
    let fill = src.nodata.unwrap_or(0.0);
    let src_h = src_f64.shape()[1];
    let src_w = src_f64.shape()[2];

    // Output: planar (bands, h, w) flat allocation. Slice per band so each
    // rayon row writes contiguous memory.
    let mut out = Array3::<f64>::zeros((bands, dst_h, dst_w));
    let mut mask = Array2::<bool>::from_elem((dst_h, dst_w), true);

    // Per-column precomputation: j0/j1/dj table reused for every row.
    // Pixel-center sampling: dst column c spans world width 1/dst_w of the
    // tile; its centre maps to fractional grid index
    // `(c + 0.5) / dst_w * WARP_GRID_SIZE`.
    // Earlier (c / dst_w) put the sample at the pixel corner, causing a
    // 0.5-px global offset versus gdalwarp / rio-tiler.
    let col_table: Vec<(usize, usize, f64)> = (0..dst_w)
        .map(|c| {
            let gj = (c as f64 + 0.5) * (WARP_GRID_SIZE as f64) / (dst_w as f64);
            let j0 = (gj.floor() as usize).min(WARP_GRID_SIZE - 1);
            (j0, j0 + 1, gj - j0 as f64)
        })
        .collect();

    // Hoist band views out of the hot loop.
    let band_views: Vec<ndarray::ArrayView2<f64>> = (0..bands)
        .map(|b| src_f64.index_axis(ndarray::Axis(0), b))
        .collect();
    let needs_numeric_mask =
        src.nodata.is_some() || matches!(src.pixels, PixelData::F32(_) | PixelData::F64(_));
    let numeric_mask = needs_numeric_mask.then(|| {
        Array2::from_shape_fn((src_h, src_w), |(r, c)| {
            src.mask.as_ref().is_none_or(|m| m[[r, c]])
                && (0..bands).all(|b| {
                    let v = src_f64[[b, r, c]];
                    v.is_finite() && src.nodata.is_none_or(|nd| nd.is_nan() || v != nd)
                })
        })
    });
    let source_mask = numeric_mask.as_ref().or(src.mask.as_ref());
    let src_mask_view = source_mask.map(|m| m.view());

    // Process each row in parallel. Output band slices are disjoint along
    // the row dimension, so we collect per-row (mask_row, [band_row,...])
    // results and copy into `out`/`mask` after the parallel section.
    use rayon::prelude::*;
    let is_average = matches!(resampling, Resampling::Average);
    let is_mode = matches!(resampling, Resampling::Mode);
    let average_corners = if is_average {
        let mut points = Vec::with_capacity((dst_w + 1) * (dst_h + 1));
        for r in 0..=dst_h {
            for c in 0..=dst_w {
                points.push(dst_transform.forward(c as f64, r as f64));
            }
        }
        crate::warp::projection::convert_batch_partial(dst_crs, &src.crs, &mut points)?;
        for point in &mut points {
            let (c, r) = src_inv.forward(point.0, point.1);
            *point = (c - 0.5, r - 0.5);
        }
        Some(points)
    } else {
        None
    };
    let coord_rows: Vec<Vec<(f64, f64)>> = (0..dst_h)
        .map(|r| row_src_coords(r, dst_h, dst_w, &col_table, &grid_src_pix))
        .collect();
    let rows: Vec<(usize, Vec<bool>, Vec<Vec<f64>>)> = (0..dst_h)
        .into_par_iter()
        .map(|r| {
            let coords = &coord_rows[r];
            let mut mask_row = vec![true; dst_w];
            let mut band_rows: Vec<Vec<f64>> = (0..bands).map(|_| vec![fill; dst_w]).collect();

            if is_average || is_mode {
                // Differentiate the same bilinear control cell used for the
                // center coordinate. This works even when either target axis
                // has a single pixel (there is no neighbor to difference).
                let gi = (r as f64 + 0.5) * WARP_GRID_SIZE as f64 / dst_h as f64;
                let i0 = (gi.floor() as usize).min(WARP_GRID_SIZE - 1);
                let di = gi - i0 as f64;
                for c in 0..dst_w {
                    let (sc, sr) = coords[c];
                    let (j0, j1, dj) = col_table[c];
                    let p00 = grid_src_pix[i0 * (WARP_GRID_SIZE + 1) + j0];
                    let p01 = grid_src_pix[i0 * (WARP_GRID_SIZE + 1) + j1];
                    let p10 = grid_src_pix[(i0 + 1) * (WARP_GRID_SIZE + 1) + j0];
                    let p11 = grid_src_pix[(i0 + 1) * (WARP_GRID_SIZE + 1) + j1];
                    let sx = WARP_GRID_SIZE as f64 / dst_w as f64;
                    let sy = WARP_GRID_SIZE as f64 / dst_h as f64;
                    let dx_col = ((p01.0 - p00.0) * (1.0 - di) + (p11.0 - p10.0) * di) * sx;
                    let dy_col = ((p01.1 - p00.1) * (1.0 - di) + (p11.1 - p10.1) * di) * sx;
                    let dx_row = ((p10.0 - p00.0) * (1.0 - dj) + (p11.0 - p01.0) * dj) * sy;
                    let dy_row = ((p10.1 - p00.1) * (1.0 - dj) + (p11.1 - p01.1) * dj) * sy;
                    // Axis-aligned envelope of the local pixel parallelogram.
                    let rx = ((dx_col.abs() + dx_row.abs()) * 0.5).max(1e-6);
                    let ry = ((dy_col.abs() + dy_row.abs()) * 0.5).max(1e-6);
                    for (b, band_view) in band_views.iter().enumerate() {
                        let value = if is_mode {
                            resampling.sample_mode(
                                *band_view,
                                src_mask_view,
                                sc,
                                sr,
                                rx,
                                ry,
                                f64::NAN,
                            )
                        } else {
                            let corners = average_corners.as_ref().unwrap();
                            let top = r * (dst_w + 1) + c;
                            let bottom = (r + 1) * (dst_w + 1) + c;
                            resampling.sample_average_polygon(
                                *band_view,
                                src_mask_view,
                                [
                                    corners[top],
                                    corners[top + 1],
                                    corners[bottom + 1],
                                    corners[bottom],
                                ],
                                f64::NAN,
                            )
                        };
                        mask_row[c] &= value.is_finite();
                        band_rows[b][c] = if value.is_finite() { value } else { fill };
                    }
                }
            } else {
                for (c, &(sc, sr)) in coords.iter().enumerate() {
                    for (b, band_view) in band_views.iter().enumerate() {
                        let value =
                            resampling.sample_masked(*band_view, src_mask_view, sc, sr, f64::NAN);
                        mask_row[c] &= value.is_finite();
                        band_rows[b][c] = if value.is_finite() { value } else { fill };
                    }
                }
            }
            (r, mask_row, band_rows)
        })
        .collect();

    // Scatter back into output buffers (cheap; rows already contiguous).
    for (r, mask_row, band_rows) in rows {
        for (c, m) in mask_row.into_iter().enumerate() {
            mask[[r, c]] = m;
        }
        for (b, row_vals) in band_rows.into_iter().enumerate() {
            let mut dst_band = out.index_axis_mut(ndarray::Axis(0), b);
            let mut dst_row = dst_band.row_mut(r);
            let dst_slice = dst_row
                .as_slice_mut()
                .expect("row_mut of Array3 is contiguous along last axis");
            dst_slice.copy_from_slice(&row_vals);
        }
    }

    // Preserve the native display dtype across geometry paths. Otherwise an
    // interpolated U8 RGB image would unexpectedly enter float auto-stretch.
    // Wide integer/F64 sources retain F64 rather than losing bits via F32.
    let pixels = match src.pixels {
        PixelData::U8(_) => PixelData::U8(out.mapv(|v| v.round().clamp(0.0, 255.0) as u8)),
        PixelData::U16(_) => PixelData::U16(out.mapv(|v| v.round().clamp(0.0, 65535.0) as u16)),
        PixelData::F32(_) => PixelData::F32(out.mapv(|v| v as f32)),
        _ => PixelData::F64(out),
    };
    Ok(ImageData {
        pixels,
        mask: Some(mask),
        bbox: dst_bbox,
        crs: dst_crs.clone(),
        transform: dst_transform,
        nodata: src.nodata,
    })
}

/// 方案2 — Whole-tile affine approximation for cross-CRS warps.
///
/// Returns `Ok(Some(...))` and short-circuits when the cross-CRS mapping
/// over this tile is well-approximated by an axis-aligned rectangle in src
/// pixel space (the common case for 4326↔3857 within one XYZ tile away
/// from the poles). Returns `Ok(None)` and lets the caller fall through to
/// the per-pixel scalar sampler when the approximation fails.
///
/// Validates the rectangular approximation against the control grid; this
/// is a bounded approximation, not a claim of exact GDAL transformer parity.
#[allow(clippy::too_many_arguments)]
fn try_subtile_affine(
    src: &ImageData,
    dst_crs: &Crs,
    dst_bbox: Bbox,
    dst_w: usize,
    dst_h: usize,
    resampling: Resampling,
    dst_transform: GeoTransform,
    grid_src_pix: &[(f64, f64)],
    n: usize,
) -> AgResult<Option<ImageData>> {
    let idx = |i: usize, j: usize| i * (n + 1) + j;
    let p00 = grid_src_pix[idx(0, 0)];
    let p0n = grid_src_pix[idx(0, n)];
    let pn0 = grid_src_pix[idx(n, 0)];
    let pnn = grid_src_pix[idx(n, n)];

    // Crop/resize only represents increasing, axis-aligned source axes.
    // A bilinear four-corner fit can be exact for shear and still be an
    // incorrect model for the rectangular crop actually sent to resize.
    if !grid_src_pix
        .iter()
        .all(|&(x, y)| x.is_finite() && y.is_finite())
        || p0n.0 <= p00.0
        || pn0.1 <= p00.1
    {
        return Ok(None);
    }

    // Compare every control point with the axis-aligned rectangle actually
    // supplied to resize, rather than with a potentially sheared quadrilateral.
    let mut max_err2 = 0.0_f64;
    for i in 0..=n {
        let di = i as f64 / n as f64;
        for j in 0..=n {
            let dj = j as f64 / n as f64;
            let predict_x = p00.0 + (p0n.0 - p00.0) * dj;
            let predict_y = p00.1 + (pn0.1 - p00.1) * di;
            let actual = grid_src_pix[idx(i, j)];
            let dx = predict_x - actual.0;
            let dy = predict_y - actual.1;
            let err2 = dx * dx + dy * dy;
            if err2 > max_err2 {
                max_err2 = err2;
            }
        }
    }
    if max_err2.sqrt() > SUBTILE_MAX_ERR_PX {
        return Ok(None);
    }

    // Additionally check the mapping is axis-aligned (no shear/rotation):
    // along the top edge the src_y should be ~constant; along the left edge
    // the src_x should be ~constant.
    let top_y_drift = (p00.1 - p0n.1).abs();
    let left_x_drift = (p00.0 - pn0.0).abs();
    if top_y_drift > SUBTILE_MAX_ERR_PX || left_x_drift > SUBTILE_MAX_ERR_PX {
        return Ok(None);
    }

    // Build the CropBox: src-pixel bbox of the 4 corner mappings.
    let min_x = p00.0.min(p0n.0).min(pn0.0).min(pnn.0);
    let max_x = p00.0.max(p0n.0).max(pn0.0).max(pnn.0);
    let min_y = p00.1.min(p0n.1).min(pn0.1).min(pnn.1);
    let max_y = p00.1.max(p0n.1).max(pn0.1).max(pnn.1);
    if max_x <= min_x || max_y <= min_y {
        return Ok(None);
    }

    let src_h = src.pixels.height() as f64;
    let src_w = src.pixels.width() as f64;

    // Boundary correctness: if the tile maps partly outside the source
    // image, clamping the CropBox to the image and resizing it into the
    // full dst tile stretches the valid strip across the whole tile
    // (smeared edges on COG-perimeter tiles at deep zoom). Fall back to the
    // scalar sampler, which returns transparent fill for out-of-bounds src
    // coords. Only floating-point rounding tolerance is allowed here:
    // clamping a half-pixel halo would change the extent being resized.
    if min_x < -WARP_EDGE_EPS
        || min_y < -WARP_EDGE_EPS
        || max_x > src_w + WARP_EDGE_EPS
        || max_y > src_h + WARP_EDGE_EPS
    {
        return Ok(None);
    }

    let crop_left = min_x.max(0.0);
    let crop_top = min_y.max(0.0);
    let crop_right = max_x.min(src_w);
    let crop_bot = max_y.min(src_h);
    let crop_w = (crop_right - crop_left).max(1e-9);
    let crop_h = (crop_bot - crop_top).max(1e-9);

    let pixels = resize_nearest_pixels(
        src,
        dst_w,
        dst_h,
        (crop_left, crop_top, crop_w, crop_h),
        resampling,
    )?;

    tracing::debug!(
        max_err_px = max_err2.sqrt(),
        crop = ?(crop_left, crop_top, crop_w, crop_h),
        "cross-CRS subtile-affine fast path"
    );

    let mask = resample_mask_nearest(
        src.mask.as_ref(),
        (crop_left, crop_top, crop_w, crop_h),
        dst_w,
        dst_h,
    )?;

    Ok(Some(ImageData {
        pixels,
        mask,
        bbox: dst_bbox,
        crs: dst_crs.clone(),
        transform: dst_transform,
        nodata: src.nodata,
    }))
}

/// Same-CRS warp: pure crop+resize via `fast_image_resize` (SIMD: SSE4/AVX2/NEON).
///
/// Dispatches on the source `PixelData` variant so each band is fed to
/// `fast_image_resize` in its native dtype — U8/U16 use the integer SIMD
/// kernels directly, F32 uses the float kernel. Other dtypes and all
/// interpolating kernels use the shared F64 scalar sampler.
#[allow(clippy::too_many_arguments)]
fn warp_affine(
    src: &ImageData,
    dst_crs: &Crs,
    dst_bbox: Bbox,
    dst_w: usize,
    dst_h: usize,
    resampling: Resampling,
    src_inv: GeoTransform,
    dst_transform: GeoTransform,
) -> AgResult<ImageData> {
    let (left_a, top_a) = src_inv.forward(dst_bbox.xmin, dst_bbox.ymax);
    let (right_a, bot_a) = src_inv.forward(dst_bbox.xmax, dst_bbox.ymin);
    let crop_left = left_a.min(right_a).max(0.0);
    let crop_top = top_a.min(bot_a).max(0.0);
    let crop_w = (right_a - left_a).abs();
    let crop_h = (bot_a - top_a).abs();

    let bands = src.pixels.bands();
    let src_h = src.pixels.height();
    let src_w = src.pixels.width();
    if src_w == 0 || src_h == 0 || crop_w <= 0.0 || crop_h <= 0.0 {
        return Err(AgError::Warp("empty crop or source".into()));
    }

    let crop_right = (crop_left + crop_w).min(src_w as f64);
    let crop_bot = (crop_top + crop_h).min(src_h as f64);
    let crop_w_clamped = (crop_right - crop_left).max(1e-9);
    let crop_h_clamped = (crop_bot - crop_top).max(1e-9);

    let pixels = resize_nearest_pixels(
        src,
        dst_w,
        dst_h,
        (crop_left, crop_top, crop_w_clamped, crop_h_clamped),
        resampling,
    )?;

    let mask = resample_mask_nearest(
        src.mask.as_ref(),
        (crop_left, crop_top, crop_w_clamped, crop_h_clamped),
        dst_w,
        dst_h,
    )?;

    let _ = bands;
    Ok(ImageData {
        pixels,
        mask,
        bbox: dst_bbox,
        crs: dst_crs.clone(),
        transform: dst_transform,
        nodata: src.nodata,
    })
}

/// One native-dtype executor for both verified crop geometries. Interpolating
/// kernels deliberately use the common scalar implementation for identical
/// mask, precision and kernel semantics at interior and boundary pixels.
fn resize_nearest_pixels(
    src: &ImageData,
    width: usize,
    height: usize,
    crop: (f64, f64, f64, f64),
    resampling: Resampling,
) -> AgResult<PixelData> {
    use fast_image_resize as fr;
    if resampling != Resampling::Nearest {
        return Err(AgError::Warp("native resize supports nearest only".into()));
    }
    let options = fr::ResizeOptions::new()
        .resize_alg(fr::ResizeAlg::Nearest)
        .crop(crop.0, crop.1, crop.2, crop.3);
    match &src.pixels {
        PixelData::U8(a) => Ok(PixelData::U8(resize_bands(
            a,
            fr::PixelType::U8,
            width,
            height,
            &options,
        )?)),
        PixelData::U16(a) => Ok(PixelData::U16(resize_bands(
            a,
            fr::PixelType::U16,
            width,
            height,
            &options,
        )?)),
        PixelData::F32(a) => Ok(PixelData::F32(resize_bands(
            a,
            fr::PixelType::F32,
            width,
            height,
            &options,
        )?)),
        _ => Err(AgError::Warp(
            "dtype requires precision-preserving scalar warp".into(),
        )),
    }
}

/// Resample an `Array2<bool>` validity mask through the same crop+resize
/// transform as the data bands, using `fast_image_resize`'s nearest-neighbour
/// SIMD path on a u8 (0/255) representation. Returns `None` when `src_mask`
/// is `None`; this is the helper that closes the "mask dropped on fast
/// path" correctness gap between the SIMD warp and the scalar fallback.
fn resample_mask_nearest(
    src_mask: Option<&ndarray::Array2<bool>>,
    crop: (f64, f64, f64, f64),
    dst_w: usize,
    dst_h: usize,
) -> AgResult<Option<ndarray::Array2<bool>>> {
    use fast_image_resize as fr;
    let Some(mask) = src_mask else {
        return Ok(None);
    };
    let (h, w) = mask.dim();
    let bytes: Vec<u8> = mask.iter().map(|&v| if v { 255u8 } else { 0u8 }).collect();
    let src_img = fr::images::Image::from_vec_u8(w as u32, h as u32, bytes, fr::PixelType::U8)
        .map_err(|e| AgError::Warp(format!("mask resize src: {e}")))?;
    let mut dst_img = fr::images::Image::new(dst_w as u32, dst_h as u32, fr::PixelType::U8);
    // Always nearest for masks — bilinear/cubic smudges sharp boundaries.
    let mask_opts = fr::ResizeOptions::new()
        .resize_alg(fr::ResizeAlg::Nearest)
        .crop(crop.0, crop.1, crop.2, crop.3);
    let mut resizer = fr::Resizer::new();
    resizer
        .resize(&src_img, &mut dst_img, Some(&mask_opts))
        .map_err(|e| AgError::Warp(format!("mask resize: {e}")))?;
    let dst_bytes = dst_img.into_vec();
    let bools: Vec<bool> = dst_bytes.iter().map(|&v| v != 0).collect();
    let out = ndarray::Array2::from_shape_vec((dst_h, dst_w), bools)
        .map_err(|e| AgError::Warp(format!("mask reshape: {e}")))?;
    Ok(Some(out))
}

/// Per-band loop driving `fast_image_resize`, parallelised over bands via
/// rayon. Each thread gets its own `Resizer` (cheap to construct), so RGB
/// or RGBA tiles see a 3-4x speedup on multi-core machines.
#[allow(clippy::too_many_arguments)]
fn resize_bands<T: bytemuck::Pod + Default + Send + Sync>(
    src_arr: &Array3<T>,
    pixel_type: fast_image_resize::PixelType,
    dst_w: usize,
    dst_h: usize,
    options: &fast_image_resize::ResizeOptions,
) -> AgResult<Array3<T>> {
    use fast_image_resize as fr;
    use ndarray::Axis;
    use rayon::prelude::*;
    let bands = src_arr.shape()[0];
    let src_h = src_arr.shape()[1];
    let src_w = src_arr.shape()[2];
    let dst_band_len = dst_w
        .checked_mul(dst_h)
        .ok_or_else(|| AgError::Warp("resize destination dimensions overflow".into()))?;
    if dst_band_len == 0 {
        return Err(AgError::Warp("empty resize destination".into()));
    }
    let output_len = bands
        .checked_mul(dst_band_len)
        .ok_or_else(|| AgError::Warp("resize output dimensions overflow".into()))?;
    let mut output = vec![T::default(); output_len];

    // Run each band's fast_image_resize call in parallel; each rayon worker
    // owns its own Resizer (construction is microseconds, plenty cheaper
    // than the SIMD convolution it amortises across).
    output
        .par_chunks_mut(dst_band_len)
        .enumerate()
        .try_for_each(|(b, dst_band)| -> AgResult<()> {
            let band = src_arr.index_axis(Axis(0), b);
            let fallback;
            let src_pixels = if let Some(contiguous) = band.as_slice() {
                contiguous
            } else {
                fallback = band.iter().copied().collect::<Vec<T>>();
                &fallback
            };
            let src_img = fr::images::ImageRef::new(
                src_w as u32,
                src_h as u32,
                bytemuck::cast_slice(src_pixels),
                pixel_type,
            )
            .map_err(|e| AgError::Warp(format!("fast_image_resize src: {e}")))?;
            let mut dst_img = fr::images::Image::from_slice_u8(
                dst_w as u32,
                dst_h as u32,
                bytemuck::cast_slice_mut(dst_band),
                pixel_type,
            )
            .map_err(|e| AgError::Warp(format!("fast_image_resize dst: {e}")))?;
            let mut local = fr::Resizer::new();
            local
                .resize(&src_img, &mut dst_img, Some(options))
                .map_err(|e| AgError::Warp(format!("fast_image_resize: {e}")))?;
            Ok(())
        })?;
    Array3::from_shape_vec((bands, dst_h, dst_w), output)
        .map_err(|e| AgError::Warp(format!("resize output reshape: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array3;

    #[test]
    fn average_warp_integrates_rotated_pixel_footprint() {
        let mut src = dummy_src(Crs::epsg(3857));
        src.pixels = PixelData::F64(Array3::from_shape_fn((1, 3, 3), |(_, r, c)| {
            if r == 1 && c == 1 {
                100.0
            } else {
                10.0
            }
        }));
        src.transform = GeoTransform::from_gdal(&[-1.0, 0.5, 0.5, 0.5, 0.5, -0.5]);
        let out = warp(
            &src,
            &src.crs,
            Bbox::new(0.0, 0.0, 1.0, 1.0),
            1,
            1,
            Resampling::Average,
        )
        .unwrap();
        assert!((out.pixels.to_f64()[[0, 0, 0]] - 55.0).abs() < 1e-9);
    }

    #[test]
    fn boundary_warp_samples_pixel_centers() {
        let mut src = dummy_src(Crs::epsg(3857));
        src.pixels = PixelData::F64(Array3::from_shape_fn((1, 4, 4), |(_, r, c)| {
            (r * 40 + c * 10) as f64
        }));
        // One extra column forces the scalar path. The first four columns
        // still have exactly the source pixel centers.
        for kernel in [Resampling::Nearest, Resampling::Bilinear] {
            let out = warp(&src, &src.crs, Bbox::new(0.0, 0.0, 5.0, 4.0), 5, 4, kernel).unwrap();
            let values = out.pixels.to_f64();
            assert_eq!(values[[0, 0, 0]], 0.0, "{kernel:?}");
            assert_eq!(values[[0, 3, 3]], 150.0, "{kernel:?}");
            assert!(out.mask.as_ref().unwrap()[[3, 3]]);
            assert!(!out.mask.as_ref().unwrap()[[0, 4]]);
        }
    }

    #[test]
    fn warp_rejects_empty_target_grid() {
        let src = dummy_src(Crs::epsg(3857));
        assert!(warp(&src, &src.crs, src.bbox, 0, 4, Resampling::Nearest).is_err());
    }

    #[test]
    fn average_single_pixel_covers_source() {
        let mut src = dummy_src(Crs::epsg(3857));
        src.pixels = PixelData::F64(Array3::from_shape_fn((1, 4, 4), |(_, r, c)| {
            (r * 4 + c) as f64
        }));
        let out = warp(&src, &src.crs, src.bbox, 1, 1, Resampling::Average).unwrap();
        assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 7.5);
    }

    #[test]
    fn interpolated_valid_support_sets_output_mask() {
        let mut src = dummy_src(Crs::epsg(3857));
        src.pixels = PixelData::F64(Array3::from_elem((1, 4, 4), 100.0));
        src.mask = Some(Array2::from_shape_fn((4, 4), |(_, c)| c != 0));
        let out = warp(
            &src,
            &src.crs,
            Bbox::new(0.25, 2.0, 1.25, 3.0),
            1,
            1,
            Resampling::Bilinear,
        )
        .unwrap();
        assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 100.0);
        assert!(out.mask.unwrap()[[0, 0]]);
    }

    #[test]
    fn zero_weight_nan_neighbor_does_not_pollute_warp() {
        let mut src = dummy_src(Crs::epsg(3857));
        src.pixels = PixelData::F32(Array3::from_shape_fn((1, 4, 4), |(_, _, c)| {
            if c == 1 {
                f32::NAN
            } else {
                100.0
            }
        }));
        let out = warp(&src, &src.crs, src.bbox, 4, 4, Resampling::Bilinear).unwrap();
        assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 100.0);
    }

    #[test]
    fn boundary_nearest_does_not_resize_clipped_extent() {
        let mut src = dummy_src(Crs::epsg(3857));
        src.pixels = PixelData::U8(Array3::from_shape_fn((1, 4, 4), |(_, _, c)| c as u8));
        let out = warp(
            &src,
            &src.crs,
            Bbox::new(-0.5, 0.0, 4.5, 4.0),
            5,
            4,
            Resampling::Nearest,
        )
        .unwrap();
        assert_eq!(
            out.pixels.to_f64().slice(ndarray::s![0, 0, ..]).to_vec(),
            vec![0.0, 1.0, 2.0, 3.0, 3.0]
        );
    }

    #[test]
    fn warp_rejects_mask_shape_mismatch() {
        let mut src = dummy_src(Crs::epsg(3857));
        src.mask = Some(Array2::from_elem((1, 1), true));
        assert!(warp(&src, &src.crs, src.bbox, 4, 4, Resampling::Nearest).is_err());
    }

    #[cfg(feature = "proj")]
    #[test]
    fn warp_keeps_valid_part_of_projection_domain() {
        let mut src = dummy_src(Crs::epsg(3413));
        src.pixels = PixelData::F64(Array3::from_elem((1, 4, 4), 100.0));
        src.transform = GeoTransform::from_gdal(&[-2e6, 1e6, 0.0, 2e6, 0.0, -1e6]);
        let out = warp(
            &src,
            &Crs::epsg(4326),
            Bbox::new(-10.0, 80.0, 10.0, 100.0),
            10,
            10,
            Resampling::Nearest,
        )
        .unwrap();
        assert!(out.mask.unwrap().iter().any(|&v| v));
    }

    #[test]
    fn flipped_source_preserves_world_orientation() {
        let mut src = dummy_src(Crs::epsg(3857));
        src.pixels = PixelData::U8(Array3::from_shape_fn((1, 4, 4), |(_, _, c)| c as u8 * 10));
        src.transform = GeoTransform::from_gdal(&[4.0, -1.0, 0.0, 4.0, 0.0, -1.0]);
        let out = warp(&src, &src.crs, src.bbox, 4, 4, Resampling::Nearest).unwrap();
        assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 30.0);
        assert_eq!(out.pixels.to_f64()[[0, 0, 3]], 0.0);
    }

    #[test]
    fn nearest_warp_preserves_wide_numeric_precision() {
        let mut src = dummy_src(Crs::epsg(3857));
        src.pixels = PixelData::U32(Array3::from_elem((1, 4, 4), 16_777_217));
        let out = warp(&src, &src.crs, src.bbox, 4, 4, Resampling::Nearest).unwrap();
        assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 16_777_217.0);
    }

    #[test]
    fn bilinear_warp_excludes_invalid_neighbors() {
        for numeric_nodata in [false, true] {
            let mut src = dummy_src(Crs::epsg(3857));
            src.pixels = PixelData::F32(Array3::from_shape_fn((1, 4, 4), |(_, _, c)| {
                if c == 0 {
                    -9999.0
                } else {
                    100.0
                }
            }));
            if numeric_nodata {
                src.nodata = Some(-9999.0);
            } else {
                src.mask = Some(Array2::from_shape_fn((4, 4), |(_, c)| c != 0));
            }
            // Source center-index coordinate 0.75: nearest is valid, but
            // interpolation still overlaps the invalid first column.
            let out = warp(
                &src,
                &src.crs,
                Bbox::new(0.75, 2.0, 1.75, 3.0),
                1,
                1,
                Resampling::Bilinear,
            )
            .unwrap();
            assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 100.0);
            assert!(out.mask.unwrap()[[0, 0]]);
        }
    }

    #[test]
    fn interior_and_boundary_paths_use_same_kernel() {
        let mut src = dummy_src(Crs::epsg(3857));
        src.pixels = PixelData::U8(Array3::from_shape_fn((1, 4, 4), |(_, _, c)| {
            if c == 1 {
                100
            } else {
                0
            }
        }));
        for kernel in [
            Resampling::Bilinear,
            Resampling::Cubic,
            Resampling::CubicSpline,
            Resampling::Lanczos,
            Resampling::Average,
        ] {
            let interior = warp(&src, &src.crs, src.bbox, 8, 8, kernel).unwrap();
            let boundary =
                warp(&src, &src.crs, Bbox::new(0.0, 0.0, 5.0, 4.0), 10, 8, kernel).unwrap();
            assert!(
                (interior.pixels.to_f64()[[0, 3, 3]] - boundary.pixels.to_f64()[[0, 3, 3]]).abs()
                    < 1e-9,
                "{kernel:?}"
            );
        }
    }

    #[test]
    fn scalar_warp_preserves_native_display_dtype() {
        let mut src = dummy_src(Crs::epsg(3857));
        src.pixels = PixelData::U8(Array3::from_elem((3, 4, 4), 100));
        let out = warp(&src, &src.crs, src.bbox, 8, 8, Resampling::Bilinear).unwrap();
        assert!(
            matches!(out.pixels, PixelData::U8(_)),
            "U8 styles must not acquire float auto-stretch semantics"
        );
    }

    #[test]
    fn sheared_source_uses_spatial_sampling() {
        let mut src = dummy_src(Crs::epsg(3857));
        src.pixels = PixelData::U8(Array3::from_shape_fn((1, 4, 8), |(_, r, c)| {
            (r * 10 + c) as u8
        }));
        src.transform = GeoTransform::from_gdal(&[0.0, 1.0, 1.0, 4.0, 0.0, -1.0]);
        let out = warp(
            &src,
            &src.crs,
            Bbox::new(4.0, 0.0, 8.0, 4.0),
            4,
            4,
            Resampling::Nearest,
        )
        .unwrap();
        assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 4.0);
        assert_eq!(out.pixels.to_f64()[[0, 3, 0]], 31.0);
    }

    fn identity_resize_options() -> fast_image_resize::ResizeOptions {
        fast_image_resize::ResizeOptions::new().resize_alg(fast_image_resize::ResizeAlg::Nearest)
    }

    #[test]
    fn resize_bands_preserves_native_multiband_values() {
        let options = identity_resize_options();

        let u8_src = Array3::from_shape_fn((2, 3, 4), |(b, y, x)| (b * 40 + y * 4 + x) as u8);
        let u8_out =
            resize_bands(&u8_src, fast_image_resize::PixelType::U8, 4, 3, &options).unwrap();
        assert_eq!(u8_out, u8_src);

        let u16_src = Array3::from_shape_fn((2, 3, 4), |(b, y, x)| (b * 1000 + y * 40 + x) as u16);
        let u16_out =
            resize_bands(&u16_src, fast_image_resize::PixelType::U16, 4, 3, &options).unwrap();
        assert_eq!(u16_out, u16_src);

        let f32_src = Array3::from_shape_fn((2, 3, 4), |(b, y, x)| {
            b as f32 * 100.0 + y as f32 * 10.0 + x as f32 + 0.25
        });
        let f32_out =
            resize_bands(&f32_src, fast_image_resize::PixelType::F32, 4, 3, &options).unwrap();
        assert_eq!(f32_out, f32_src);
    }

    #[test]
    fn resize_bands_accepts_non_standard_layout() {
        let src = Array3::from_shape_fn((2, 2, 3), |(b, y, x)| (b * 20 + y * 3 + x) as u8)
            .permuted_axes([0, 2, 1]);
        assert!(!src.is_standard_layout());

        let out = resize_bands(
            &src,
            fast_image_resize::PixelType::U8,
            src.shape()[2],
            src.shape()[1],
            &identity_resize_options(),
        )
        .unwrap();

        assert_eq!(out, src);
    }

    fn dummy_src(crs: Crs) -> ImageData {
        ImageData {
            pixels: PixelData::F64(Array3::<f64>::zeros((1, 4, 4))),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 4.0, 4.0),
            crs,
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 4.0, 0.0, -1.0]),
            nodata: None,
        }
    }

    #[test]
    fn same_crs_fast_path_clones() {
        let src = dummy_src(Crs::epsg(3857));
        let out = warp(&src, &Crs::epsg(3857), src.bbox, 4, 4, Resampling::Nearest).unwrap();
        assert_eq!(out.crs, src.crs);
    }

    /// Average over a cross-CRS tile that extends outside the source.
    /// The kernel must preserve valid source values and F64 precision.
    #[test]
    fn average_cross_crs_scalar_path_box_mean() {
        // 8×8 ramp: pixel (y,x) = y*8+x, range [0, 63].
        let arr = Array3::<f64>::from_shape_fn((1, 8, 8), |(_, y, x)| (y * 8 + x) as f64);
        let src = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            // EPSG:4326 bbox covering lon 0–8°, lat 0–8°.
            bbox: Bbox::new(0.0, 0.0, 8.0, 8.0),
            crs: Crs::epsg(4326),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 8.0, 0.0, -1.0]),
            nodata: None,
        };

        // Dst bbox in EPSG:3857 covering roughly lon −1°..15°, lat 0°..9°.
        // This deliberately extends outside the source image on both x-edges,
        // forcing try_subtile_affine to return None (out-of-bounds guard).
        //   lon −1° ≈ −111_195 m,  lon 15° ≈ 1_669_792 m
        //   lat  0° = 0 m,          lat  9° ≈ 1_003_750 m
        let dst_bbox = Bbox::new(-111_195.0, 0.0, 1_669_792.0, 1_003_750.0);
        let out = warp(&src, &Crs::epsg(3857), dst_bbox, 4, 4, Resampling::Average).unwrap();

        // F64 source precision must be retained.
        assert!(
            matches!(out.pixels, PixelData::F64(_)),
            "expected scalar fallback (F64 output) but got a different variant"
        );

        // ── Box-mean invariant ───────────────────────────────────────────────
        // Every finite output value must lie in the source value range [0, 63].
        // Out-of-bounds src coords yield fill = nodata.unwrap_or(0.0) = 0.0,
        // which is already inside [0, 63].
        let vals: Vec<f64> = out.pixels.as_f64().iter().copied().collect();
        for &v in vals.iter() {
            assert!(
                (0.0..=63.0).contains(&v),
                "avg {v} out of source range [0, 63]"
            );
        }
        // Guard against silent degradation to fill/constant: at least one
        // interior pixel must be a genuine mid-range average.
        let interior = vals
            .iter()
            .copied()
            .filter(|&v| v > 5.0 && v < 58.0)
            .count();
        assert!(
            interior > 0,
            "expected interior averaged values; got none (averaging may be broken)"
        );
        assert_eq!(out.pixels.height(), 4);
        assert_eq!(out.pixels.width(), 4);
    }

    #[test]
    fn mode_downsample_preserves_class_not_mean() {
        // 4x4 single band: left half class 1, right half class 9.
        let a =
            ndarray::Array3::from_shape_fn((1, 4, 4), |(_, _, c)| if c < 2 { 1.0 } else { 9.0 });
        let src = ImageData {
            pixels: PixelData::F64(a),
            mask: None,
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 4.0, 0.0, -1.0]),
            nodata: None,
            bbox: Bbox::new(0.0, 0.0, 4.0, 4.0),
        };
        // Downsample 4x4 → 2x2, same CRS. Each output pixel covers a pure
        // 2x2 block (all class 1 or all class 9) → mean (5.0) must NEVER appear.
        let out = warp(&src, &Crs::epsg(3857), src.bbox, 2, 2, Resampling::Mode).unwrap();
        // Preserve F64 categorical values.
        // Value assertions below establish the majority semantics.
        assert!(
            matches!(out.pixels, PixelData::F64(_)),
            "expected F64 categorical output"
        );
        let f = out.pixels.to_f64();
        for v in f.iter() {
            assert!(
                *v == 1.0 || *v == 9.0,
                "mode produced a non-class value {v}"
            );
        }
    }

    /// Cross-CRS categorical output contains only original source classes.
    #[test]
    fn mode_cross_crs_preserves_class() {
        // 16×8 single-band categorical source in EPSG:4326:
        //   left 8 columns (lon 0–4°) = class 1
        //   right 8 columns (lon 4–8°) = class 9
        // Warped cross-CRS to EPSG:3857 at 4×4 resolution.
        //
        // The dst bbox is the proper EPSG:3857 rectangle for lon 1–7°, lat 1–7°
        // (a strict subset of the source), so every grid corner projects back
        // inside the 16×8 source pixel space and `try_subtile_affine` *would*
        // return Some(…) for other resamplers — but Mode bypasses it.
        //
        // lon 1° in EPSG:3857 ≈    111 319 m
        // lon 7° in EPSG:3857 ≈    779 236 m
        // lat 1° in EPSG:3857 ≈    111 325 m
        // lat 7° in EPSG:3857 ≈    781 863 m
        let arr =
            Array3::<f64>::from_shape_fn((1, 8, 16), |(_, _, x)| if x < 8 { 1.0 } else { 9.0 });
        let src = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            // lon 0–8°, lat 0–8° in EPSG:4326.
            // GeoTransform: origin at top-left (lon=0, lat=8), 0.5° per pixel.
            bbox: Bbox::new(0.0, 0.0, 8.0, 8.0),
            crs: Crs::epsg(4326),
            transform: GeoTransform::from_gdal(&[0.0, 0.5, 0.0, 8.0, 0.0, -1.0]),
            nodata: None,
        };
        // Dst bbox in EPSG:3857 covering lon 1–7°, lat 1–7° — fully inside src.
        let dst_bbox = Bbox::new(111_319.0, 111_325.0, 779_236.0, 781_863.0);
        let fill = src.nodata.unwrap_or(0.0); // 0.0
        let out = warp(&src, &Crs::epsg(3857), dst_bbox, 4, 4, Resampling::Mode).unwrap();

        assert_eq!(out.pixels.height(), 4);
        assert_eq!(out.pixels.width(), 4);

        // ── Path oracle ──────────────────────────────────────────────────────
        // Preserve the source precision.
        // This trips the moment the bypass is removed (Mode silently takes fr).
        assert!(
            matches!(out.pixels, PixelData::F64(_)),
            "expected F64 categorical output; \
             the `if !matches!(resampling, Resampling::Mode)` bypass may be missing"
        );

        // ── Value oracle ─────────────────────────────────────────────────────
        // Every non-fill output pixel must be exactly one of the two class
        // values. An fr/Nearest path could blend across the class boundary and
        // produce values strictly between 1.0 and 9.0.
        let vals: Vec<f64> = out.pixels.as_f64().iter().copied().collect();
        for &v in vals.iter() {
            if v == fill {
                continue; // edge/nodata fill is acceptable
            }
            assert!(
                v == 1.0 || v == 9.0,
                "mode produced inter-class value {v} (expected only 1.0 or 9.0)"
            );
        }

        // Guard: both classes must appear in the output (not all fill).
        let has_class1 = vals.contains(&1.0);
        let has_class9 = vals.contains(&9.0);
        assert!(has_class1, "class 1 absent from Mode cross-CRS output");
        assert!(has_class9, "class 9 absent from Mode cross-CRS output");
    }

    #[test]
    fn average_cross_crs_downsample_within_source_range() {
        let arr = Array3::<f64>::from_shape_fn((1, 8, 8), |(_, y, x)| (y * 8 + x) as f64);
        let src = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 8.0, 8.0),
            crs: Crs::epsg(4326),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 8.0, 0.0, -1.0]),
            nodata: None,
        };
        let out = warp(&src, &Crs::epsg(3857), src.bbox, 4, 4, Resampling::Average).unwrap();
        assert_eq!(out.pixels.height(), 4);
        assert_eq!(out.pixels.width(), 4);
        let vals: Vec<f64> = out.pixels.as_f64().iter().copied().collect();
        for &v in vals.iter() {
            assert!((0.0..=63.0).contains(&v), "avg {v} out of source range");
        }
        // Guard against silent degradation to fill/constant: at least one
        // interior pixel must be a genuine mid-range average.
        let interior = vals
            .iter()
            .copied()
            .filter(|&v| v > 5.0 && v < 58.0)
            .count();
        assert!(
            interior > 0,
            "expected interior averaged values; got none (averaging may be broken)"
        );
    }
}
