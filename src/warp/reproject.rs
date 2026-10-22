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
    warp_impl(src, None, dst_crs, dst_bbox, dst_w, dst_h, resampling)
}

/// Selected bands share geometry, but retain independent interpolation validity.
#[allow(clippy::too_many_arguments)]
pub(crate) fn warp_selected(
    src: &ImageData,
    selected: &[usize],
    dst_crs: &Crs,
    dst_bbox: Bbox,
    dst_w: usize,
    dst_h: usize,
    resampling: Resampling,
) -> AgResult<ImageData> {
    if selected.is_empty() || selected.iter().any(|&b| b >= src.pixels.bands()) {
        return Err(AgError::Warp("invalid selected bands".into()));
    }
    warp_impl(
        src,
        Some(selected),
        dst_crs,
        dst_bbox,
        dst_w,
        dst_h,
        resampling,
    )
}

#[allow(clippy::too_many_arguments)]
fn warp_impl(
    src: &ImageData,
    selected: Option<&[usize]>,
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
    let can_resize = selected.is_none()
        && matches!(src.pixels, PixelData::U8(_) | PixelData::U16(_))
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
    let grid_src_pix = source_control_grid(src_inv, &src.crs, dst_crs, dst_bbox)?;

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

    // Geometry is shared; selected bands are converted and sampled one at a
    // time to bound source-sized F64 scratch and preserve per-band NoData.
    let all_bands: Vec<_> = (0..src.pixels.bands()).collect();
    let selected_bands = selected.unwrap_or(&all_bands);
    let bands = selected_bands.len();
    let fill = src.nodata.unwrap_or(0.0);
    let (src_h, src_w) = (src.pixels.height(), src.pixels.width());

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

    use rayon::prelude::*;
    let is_average = matches!(resampling, Resampling::Average);
    let footprint_corners = matches!(resampling, Resampling::Average | Resampling::Mode)
        .then(|| source_pixel_corners(src_inv, &src.crs, dst_crs, dst_transform, dst_w, dst_h))
        .transpose()?;
    let coord_rows: Vec<Vec<(f64, f64)>> = (0..dst_h)
        .map(|r| row_src_coords(r, dst_h, dst_w, &col_table, &grid_src_pix))
        .collect();
    // Nearest needs only the sampled integer value and its validity, not a
    // source-sized F64 band and numeric mask. Keep interpolation on F64 below.
    if selected.is_some() && resampling == Resampling::Nearest {
        let native = match &src.pixels {
            PixelData::U8(a) => {
                let (pixels, mask) = sample_nearest_native(
                    a,
                    selected_bands,
                    &coord_rows,
                    src.mask.as_ref(),
                    src.nodata,
                    fill.round().clamp(0.0, 255.0) as u8,
                );
                Some((PixelData::U8(pixels), mask))
            }
            PixelData::U16(a) => {
                let (pixels, mask) = sample_nearest_native(
                    a,
                    selected_bands,
                    &coord_rows,
                    src.mask.as_ref(),
                    src.nodata,
                    fill.round().clamp(0.0, 65535.0) as u16,
                );
                Some((PixelData::U16(pixels), mask))
            }
            _ => None,
        };
        if let Some((pixels, mask)) = native {
            return Ok(ImageData {
                pixels,
                mask: Some(mask),
                bbox: dst_bbox,
                crs: dst_crs.clone(),
                transform: dst_transform,
                nodata: None,
            });
        }
    }
    let src_f64 = selected.is_none().then(|| src.pixels.as_f64());
    let needs_numeric_mask =
        src.nodata.is_some() || matches!(src.pixels, PixelData::F32(_) | PixelData::F64(_));
    let joint_mask = src_f64.as_ref().filter(|_| needs_numeric_mask).map(|a| {
        Array2::from_shape_fn((src_h, src_w), |(r, c)| {
            src.mask.as_ref().is_none_or(|m| m[[r, c]])
                && (0..bands).all(|b| {
                    let v = a[[b, r, c]];
                    v.is_finite() && src.nodata.is_none_or(|nd| nd.is_nan() || v != nd)
                })
        })
    });
    let mut out = Array3::<f64>::zeros((bands, dst_h, dst_w));
    let mut mask = Array2::<bool>::from_elem((dst_h, dst_w), true);
    for (output_band, &source_band) in selected_bands.iter().enumerate() {
        let converted = if src_f64.is_some() || matches!(src.pixels, PixelData::F64(_)) {
            None
        } else {
            Some(src.pixels.band_to_f64(source_band))
        };
        let band_view = if let Some(a) = &src_f64 {
            a.index_axis(ndarray::Axis(0), source_band)
        } else if let PixelData::F64(a) = &src.pixels {
            a.index_axis(ndarray::Axis(0), source_band)
        } else {
            converted.as_ref().expect("non-F64 band conversion").view()
        };
        let numeric_mask = (selected.is_some() && needs_numeric_mask).then(|| {
            Array2::from_shape_fn((src_h, src_w), |(r, c)| {
                let v = band_view[[r, c]];
                src.mask.as_ref().is_none_or(|m| m[[r, c]])
                    && v.is_finite()
                    && src.nodata.is_none_or(|nd| nd.is_nan() || v != nd)
            })
        });
        let source_mask = numeric_mask
            .as_ref()
            .or(joint_mask.as_ref())
            .or(src.mask.as_ref());
        let src_mask_view = source_mask.map(|m| m.view());
        let mut dst_band = out.index_axis_mut(ndarray::Axis(0), output_band);
        dst_band
            .as_slice_mut()
            .expect("contiguous output band")
            .par_chunks_mut(dst_w)
            .zip(
                mask.as_slice_mut()
                    .expect("contiguous output mask")
                    .par_chunks_mut(dst_w),
            )
            .enumerate()
            .for_each(|(r, (band_row, mask_row))| {
                let coords = &coord_rows[r];
                for (c, &(sc, sr)) in coords.iter().enumerate() {
                    let value = if let Some(corners) = &footprint_corners {
                        let top = r * (dst_w + 1) + c;
                        let bottom = (r + 1) * (dst_w + 1) + c;
                        let footprint = [
                            corners[top],
                            corners[top + 1],
                            corners[bottom + 1],
                            corners[bottom],
                        ];
                        if is_average {
                            resampling.sample_average_polygon(
                                band_view,
                                src_mask_view,
                                footprint,
                                f64::NAN,
                            )
                        } else {
                            resampling.sample_mode_polygon(
                                band_view,
                                src_mask_view,
                                footprint,
                                f64::NAN,
                            )
                        }
                    } else {
                        resampling.sample_masked(band_view, src_mask_view, sc, sr, f64::NAN)
                    };
                    mask_row[c] &= value.is_finite();
                    band_row[c] = if value.is_finite() { value } else { fill };
                }
            });
    }

    // Preserve the native display dtype across geometry paths. Otherwise an
    // interpolated U8 RGB image would unexpectedly enter float auto-stretch.
    // Wide integer/F64 sources retain F64 rather than losing bits via F32.
    let pixels = if selected.is_some() && resampling != Resampling::Nearest {
        PixelData::F64(out)
    } else {
        match src.pixels {
            PixelData::U8(_) => PixelData::U8(out.mapv(|v| v.round().clamp(0.0, 255.0) as u8)),
            PixelData::U16(_) => PixelData::U16(out.mapv(|v| v.round().clamp(0.0, 65535.0) as u16)),
            PixelData::F32(_) => PixelData::F32(out.mapv(|v| v as f32)),
            _ => PixelData::F64(out),
        }
    };
    Ok(ImageData {
        pixels,
        mask: Some(mask),
        bbox: dst_bbox,
        crs: dst_crs.clone(),
        transform: dst_transform,
        nodata: if selected.is_some() { None } else { src.nodata },
    })
}

fn source_control_grid(
    src_inverse: GeoTransform,
    src_crs: &Crs,
    dst_crs: &Crs,
    dst_bbox: Bbox,
) -> AgResult<Vec<(f64, f64)>> {
    let mut points = Vec::with_capacity((WARP_GRID_SIZE + 1) * (WARP_GRID_SIZE + 1));
    for r in 0..=WARP_GRID_SIZE {
        for c in 0..=WARP_GRID_SIZE {
            points.push((
                dst_bbox.xmin + c as f64 * dst_bbox.width() / WARP_GRID_SIZE as f64,
                dst_bbox.ymax - r as f64 * dst_bbox.height() / WARP_GRID_SIZE as f64,
            ));
        }
    }
    crate::warp::projection::convert_batch_partial(dst_crs, src_crs, &mut points)?;
    for point in &mut points {
        *point = src_inverse.forward(point.0, point.1);
    }
    Ok(points)
}

/// Exact target pixel edges shared by area and categorical footprint sampling.
fn source_pixel_corners(
    src_inverse: GeoTransform,
    src_crs: &Crs,
    dst_crs: &Crs,
    dst_transform: GeoTransform,
    dst_w: usize,
    dst_h: usize,
) -> AgResult<Vec<(f64, f64)>> {
    let corner_count = dst_w
        .checked_add(1)
        .and_then(|w| dst_h.checked_add(1).and_then(|h| w.checked_mul(h)))
        .ok_or_else(|| AgError::Warp("pixel corner dimensions overflow".into()))?;
    let mut points = Vec::with_capacity(corner_count);
    for r in 0..=dst_h {
        for c in 0..=dst_w {
            points.push(dst_transform.forward(c as f64, r as f64));
        }
    }
    crate::warp::projection::convert_batch_partial(dst_crs, src_crs, &mut points)?;
    for point in &mut points {
        let (c, r) = src_inverse.forward(point.0, point.1);
        *point = (c - 0.5, r - 0.5);
    }
    Ok(points)
}

fn sample_nearest_native<T: Copy + Default + Into<f64> + Send + Sync>(
    source: &Array3<T>,
    selected: &[usize],
    coords: &[Vec<(f64, f64)>],
    source_mask: Option<&Array2<bool>>,
    nodata: Option<f64>,
    fill: T,
) -> (Array3<T>, Array2<bool>) {
    use rayon::prelude::*;
    let (height, width) = (coords.len(), coords[0].len());
    let (src_h, src_w) = (source.shape()[1], source.shape()[2]);
    let mut output = Array3::from_elem((selected.len(), height, width), fill);
    let mut mask = Array2::from_elem((height, width), true);
    for (b, &source_band) in selected.iter().enumerate() {
        let band = source.index_axis(ndarray::Axis(0), source_band);
        output
            .index_axis_mut(ndarray::Axis(0), b)
            .as_slice_mut()
            .expect("contiguous output band")
            .par_chunks_mut(width)
            .zip(
                mask.as_slice_mut()
                    .expect("contiguous output mask")
                    .par_chunks_mut(width),
            )
            .enumerate()
            .for_each(|(r, (row, mask_row))| {
                for (c, &(sc, sr)) in coords[r].iter().enumerate() {
                    let in_bounds = sc.is_finite()
                        && sr.is_finite()
                        && sc >= -0.5
                        && sr >= -0.5
                        && sc <= (src_w - 1) as f64 + 0.5
                        && sr <= (src_h - 1) as f64 + 0.5;
                    let valid = if in_bounds {
                        let x = sc.clamp(0.0, (src_w - 1) as f64).round() as usize;
                        let y = sr.clamp(0.0, (src_h - 1) as f64).round() as usize;
                        let value = band[[y, x]];
                        let valid = source_mask.is_none_or(|m| m[[y, x]])
                            && nodata.is_none_or(|nd| nd.is_nan() || value.into() != nd);
                        if valid {
                            row[c] = value;
                        }
                        valid
                    } else {
                        false
                    };
                    mask_row[c] &= valid;
                }
            });
    }
    (output, mask)
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
#[path = "reproject_test.rs"]
mod tests;
