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

const WARP_EDGE_EPS: f64 = 0.5;
const WARP_GRID_SIZE: usize = 32;
const SUBTILE_MAX_ERR_PX: f64 = 0.33;

/// Source-pixel coords `(sc, sr)` for every dst column of dst row `r`,
/// using the warp grid. Extracted so the average kernel can difference
/// row `r` against row `r+1` for the vertical Jacobian.
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
        out.push((sc, sr));
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
    // transparent fill for out-of-bounds source coords. EDGE_EPS matches
    // the resampler's sub-pixel seam-halo tolerance so fully-interior
    // tiles still take the SIMD path.
    if &src.crs == dst_crs {
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
    crate::warp::projection::convert_batch(dst_crs, &src.crs, &mut grid_dst)?;

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
    if !matches!(resampling, Resampling::Mode) {
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
    let src_mask_view = src.mask.as_ref().map(|m| m.view());

    // Process each row in parallel. Output band slices are disjoint along
    // the row dimension, so we collect per-row (mask_row, [band_row,...])
    // results and copy into `out`/`mask` after the parallel section.
    use rayon::prelude::*;
    let is_average = matches!(resampling, Resampling::Average);
    let is_mode = matches!(resampling, Resampling::Mode);
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
                // Footprint Jacobian by finite difference of source coords.
                // Forward difference where a forward neighbour exists,
                // backward difference at the trailing row/column — a clamped
                // one-sided diff would collapse to zero and silently degrade
                // the edge strip to bilinear (visible as tile-edge seams).
                let r_fwd = (r + 1).min(dst_h - 1);
                let r_bwd = r.saturating_sub(1);
                let coords_r_fwd = &coord_rows[r_fwd];
                let coords_r_bwd = (r_bwd != r).then_some(&coord_rows[r_bwd]);
                for c in 0..dst_w {
                    let (sc, sr) = coords[c];

                    // Column step: source displacement per one dst-column.
                    // (dx_col is the on-axis term, dy_col the shear term;
                    //  both feed the axis-aligned footprint below.)
                    let (col_a, col_b) = if c + 1 < dst_w {
                        (coords[c + 1], coords[c])
                    } else if c > 0 {
                        (coords[c], coords[c - 1])
                    } else {
                        (coords[c], coords[c]) // dst_w == 1: no neighbour
                    };
                    let dx_col = (col_a.0 - col_b.0).abs();
                    let dy_col = (col_a.1 - col_b.1).abs();

                    // Row step: source displacement per one dst-row.
                    let (row_a, row_b) = if r + 1 < dst_h {
                        (coords_r_fwd[c], coords[c])
                    } else if let Some(cb) = coords_r_bwd {
                        (coords[c], cb[c])
                    } else {
                        (coords[c], coords[c]) // dst_h == 1: no neighbour
                    };
                    let dx_row = (row_a.0 - row_b.0).abs();
                    let dy_row = (row_a.1 - row_b.1).abs();

                    // Axis-aligned footprint half-extent: max source-x (and
                    // source-y) displacement contributed by either step.
                    let rx = (dx_col.max(dx_row) * 0.5).max(1e-6);
                    let ry = (dy_col.max(dy_row) * 0.5).max(1e-6);

                    mask_row[c] = sample_valid(src.mask.as_ref(), src_w, src_h, sc, sr);
                    for (b, band_view) in band_views.iter().enumerate() {
                        band_rows[b][c] = if is_mode {
                            resampling.sample_mode(*band_view, src_mask_view, sc, sr, rx, ry, fill)
                        } else {
                            resampling.sample_average(
                                *band_view,
                                src_mask_view,
                                sc,
                                sr,
                                rx,
                                ry,
                                fill,
                            )
                        };
                    }
                }
            } else {
                for (c, &(sc, sr)) in coords.iter().enumerate() {
                    mask_row[c] = sample_valid(src.mask.as_ref(), src_w, src_h, sc, sr);
                    for (b, band_view) in band_views.iter().enumerate() {
                        band_rows[b][c] =
                            resampling.sample_masked(*band_view, src_mask_view, sc, sr, fill);
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

    Ok(ImageData {
        pixels: PixelData::F64(out),
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
/// Mirrors GDAL `gdalwarp`'s default "approx_transformer" heuristic: do
/// one PROJ sweep at the corners, validate against an interior grid, and
/// promote to the rectilinear fast path when error is sub-pixel.
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

    // The samples at the four grid corners give us the linear approximation
    // src = lerp(corners; di, dj). Check that this linear model approximates
    // every interior grid point within SUBTILE_MAX_ERR_PX.
    let mut max_err2 = 0.0_f64;
    for i in 0..=n {
        let di = i as f64 / n as f64;
        for j in 0..=n {
            let dj = j as f64 / n as f64;
            let predict_x = p00.0 * (1.0 - di) * (1.0 - dj)
                + p0n.0 * (1.0 - di) * dj
                + pn0.0 * di * (1.0 - dj)
                + pnn.0 * di * dj;
            let predict_y = p00.1 * (1.0 - di) * (1.0 - dj)
                + p0n.1 * (1.0 - di) * dj
                + pn0.1 * di * (1.0 - dj)
                + pnn.1 * di * dj;
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
    // coords. WARP_EDGE_EPS keeps the same sub-pixel seam-halo tolerance
    // as the resampler edge handling, so fully-interior tiles still take
    // this SIMD fast path.
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

    use fast_image_resize as fr;
    let alg = match resampling {
        Resampling::Nearest => fr::ResizeAlg::Nearest,
        Resampling::Bilinear => fr::ResizeAlg::Convolution(fr::FilterType::Bilinear),
        Resampling::Cubic => fr::ResizeAlg::Convolution(fr::FilterType::CatmullRom),
        Resampling::CubicSpline => fr::ResizeAlg::Convolution(fr::FilterType::Mitchell),
        Resampling::Lanczos => fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3),
        Resampling::Average => fr::ResizeAlg::Convolution(fr::FilterType::Box),
        Resampling::Mode => fr::ResizeAlg::Nearest, // unreachable: Mode bypasses fr (routed to scalar majority path)
    };
    let options = fr::ResizeOptions::new()
        .resize_alg(alg)
        .crop(crop_left, crop_top, crop_w, crop_h);
    // Dispatch on source dtype — same fan-out as warp_affine.
    let pixels = match &src.pixels {
        PixelData::U8(a) => PixelData::U8(resize_bands(
            a,
            fr::PixelType::U8,
            dst_w,
            dst_h,
            &options,
            |b| b.iter().copied().collect::<Vec<u8>>(),
            |bytes| bytes.to_vec(),
        )?),
        PixelData::U16(a) => PixelData::U16(resize_bands(
            a,
            fr::PixelType::U16,
            dst_w,
            dst_h,
            &options,
            |b| {
                let mut buf = vec![0u8; b.len() * 2];
                bytemuck::cast_slice_mut::<u8, u16>(&mut buf)
                    .copy_from_slice(b.as_slice().expect("u16 band contiguous"));
                buf
            },
            |bytes| bytemuck::cast_slice::<u8, u16>(bytes).to_vec(),
        )?),
        PixelData::F32(a) => PixelData::F32(resize_bands(
            a,
            fr::PixelType::F32,
            dst_w,
            dst_h,
            &options,
            |b| {
                let mut buf = vec![0u8; b.len() * 4];
                bytemuck::cast_slice_mut::<u8, f32>(&mut buf)
                    .copy_from_slice(b.as_slice().expect("f32 band contiguous"));
                buf
            },
            |bytes| bytemuck::cast_slice::<u8, f32>(bytes).to_vec(),
        )?),
        other => {
            let f32_arr = other.to_f64().mapv(|v| v as f32);
            PixelData::F32(resize_bands(
                &f32_arr,
                fr::PixelType::F32,
                dst_w,
                dst_h,
                &options,
                |band| {
                    let mut buf = vec![0u8; band.len() * 4];
                    bytemuck::cast_slice_mut::<u8, f32>(&mut buf)
                        .copy_from_slice(band.as_slice().expect("f32 band contiguous"));
                    buf
                },
                |bytes| bytemuck::cast_slice::<u8, f32>(bytes).to_vec(),
            )?)
        }
    };

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
/// kernels directly, F32 uses the float kernel. F64/I16/I32/U32 are
/// cast once to F32 (matching GDAL's default working type) so the SIMD
/// path stays in scope.
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
    use fast_image_resize as fr;

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

    let alg = match resampling {
        Resampling::Nearest => fr::ResizeAlg::Nearest,
        Resampling::Bilinear => fr::ResizeAlg::Convolution(fr::FilterType::Bilinear),
        Resampling::Cubic => fr::ResizeAlg::Convolution(fr::FilterType::CatmullRom),
        Resampling::CubicSpline => fr::ResizeAlg::Convolution(fr::FilterType::Mitchell),
        Resampling::Lanczos => fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3),
        Resampling::Average => fr::ResizeAlg::Convolution(fr::FilterType::Box),
        Resampling::Mode => fr::ResizeAlg::Nearest, // unreachable: Mode bypasses fr (routed to scalar majority path)
    };
    let options = fr::ResizeOptions::new().resize_alg(alg).crop(
        crop_left,
        crop_top,
        crop_w_clamped,
        crop_h_clamped,
    );
    let pixels = match &src.pixels {
        PixelData::U8(a) => PixelData::U8(resize_bands(
            a,
            fr::PixelType::U8,
            dst_w,
            dst_h,
            &options,
            |b| b.iter().copied().collect::<Vec<u8>>(),
            |bytes| bytes.to_vec(),
        )?),
        PixelData::U16(a) => PixelData::U16(resize_bands(
            a,
            fr::PixelType::U16,
            dst_w,
            dst_h,
            &options,
            |b| {
                // Native u16 → bytes
                let mut buf = vec![0u8; b.len() * 2];
                bytemuck::cast_slice_mut::<u8, u16>(&mut buf)
                    .copy_from_slice(b.as_slice().expect("u16 band contiguous"));
                buf
            },
            |bytes| bytemuck::cast_slice::<u8, u16>(bytes).to_vec(),
        )?),
        PixelData::F32(a) => PixelData::F32(resize_bands(
            a,
            fr::PixelType::F32,
            dst_w,
            dst_h,
            &options,
            |b| {
                let mut buf = vec![0u8; b.len() * 4];
                bytemuck::cast_slice_mut::<u8, f32>(&mut buf)
                    .copy_from_slice(b.as_slice().expect("f32 band contiguous"));
                buf
            },
            |bytes| bytemuck::cast_slice::<u8, f32>(bytes).to_vec(),
        )?),
        // I16/I32/U32/F64 cast to F32 once — fast_image_resize has no
        // native signed-int or f64 SIMD kernel, but f32 keeps SIMD on.
        other => {
            let f32_arr = other.to_f64().mapv(|v| v as f32);
            PixelData::F32(resize_bands(
                &f32_arr,
                fr::PixelType::F32,
                dst_w,
                dst_h,
                &options,
                |band| {
                    let mut buf = vec![0u8; band.len() * 4];
                    bytemuck::cast_slice_mut::<u8, f32>(&mut buf)
                        .copy_from_slice(band.as_slice().expect("f32 band contiguous"));
                    buf
                },
                |bytes| bytemuck::cast_slice::<u8, f32>(bytes).to_vec(),
            )?)
        }
    };

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
    to_bytes: impl Fn(&ndarray::ArrayView2<T>) -> Vec<u8> + Send + Sync,
    from_bytes: impl Fn(&[u8]) -> Vec<T> + Send + Sync,
) -> AgResult<Array3<T>> {
    use fast_image_resize as fr;
    use ndarray::Axis;
    use rayon::prelude::*;
    let bands = src_arr.shape()[0];
    let src_h = src_arr.shape()[1];
    let src_w = src_arr.shape()[2];

    // Run each band's fast_image_resize call in parallel; each rayon worker
    // owns its own Resizer (construction is microseconds, plenty cheaper
    // than the SIMD convolution it amortises across).
    let band_outputs: Vec<AgResult<Vec<T>>> = (0..bands)
        .into_par_iter()
        .map(|b| {
            let band = src_arr.index_axis(Axis(0), b);
            let src_bytes = to_bytes(&band);
            let src_img =
                fr::images::Image::from_vec_u8(src_w as u32, src_h as u32, src_bytes, pixel_type)
                    .map_err(|e| AgError::Warp(format!("fast_image_resize src: {e}")))?;
            let mut dst_img = fr::images::Image::new(dst_w as u32, dst_h as u32, pixel_type);
            let mut local = fr::Resizer::new();
            local
                .resize(&src_img, &mut dst_img, Some(options))
                .map_err(|e| AgError::Warp(format!("fast_image_resize: {e}")))?;
            Ok(from_bytes(dst_img.buffer()))
        })
        .collect();

    let mut out = Array3::<T>::from_elem((bands, dst_h, dst_w), T::default());
    for (b, result) in band_outputs.into_iter().enumerate() {
        let dst_typed = result?;
        let mut out_band = out.index_axis_mut(Axis(0), b);
        out_band
            .as_slice_mut()
            .expect("out_band contiguous")
            .copy_from_slice(&dst_typed);
    }
    Ok(out)
}

fn sample_valid(
    mask: Option<&Array2<bool>>,
    width: usize,
    height: usize,
    col: f64,
    row: f64,
) -> bool {
    // NaN guard: PROJ at antimeridian / pole can return NaN. Without the
    // `is_finite` check the `as i64` cast silently produces 0, marking the
    // tile corner as a "valid" sample at (0, 0).
    if !col.is_finite() || !row.is_finite() {
        return false;
    }
    let c = col.round() as i64;
    let r = row.round() as i64;
    if c < 0 || r < 0 || c >= width as i64 || r >= height as i64 {
        return false;
    }
    mask.map(|m| m[[r as usize, c as usize]]).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array3;

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

    /// Integration test that provably exercises the scalar `is_average` fallback branch.
    ///
    /// Control-flow reasoning:
    ///   1. src CRS (EPSG:4326) ≠ dst CRS (EPSG:3857) → same-CRS fast path skipped.
    ///   2. The dst bbox in EPSG:3857 is chosen to map partly *outside* the source
    ///      image bounds when projected back to EPSG:4326 pixel space.  Concretely,
    ///      the source covers lon 0–8°, lat 0–8°, but the dst bbox extends to
    ///      ~lon −1° and ~lon 15° (in EPSG:3857 meters), so the grid corners in
    ///      src-pixel space have min_x < 0 and max_x > src_w.  `try_subtile_affine`
    ///      detects this at the out-of-bounds guard (lines 348-354) and returns
    ///      `Ok(None)`, falling through to the scalar loop.
    ///
    /// Path oracle:
    ///   The scalar fallback always returns `PixelData::F64(out)`.
    ///   `try_subtile_affine` converts F64 sources to F32 and returns
    ///   `PixelData::F32`.  Asserting `PixelData::F64` therefore FAILS if the
    ///   implementation ever silently routes through the fast path instead.
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

        // ── Path oracle ──────────────────────────────────────────────────────
        // The scalar fallback returns F64; the SIMD fast path converts F64→F32.
        // This assertion is the coverage guard: it fails if the test silently
        // takes the fast path.
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
        let a = ndarray::Array3::from_shape_fn((1, 4, 4), |(_, _, c)| if c < 2 { 1.0 } else { 9.0 });
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
        // Path oracle: scalar fallback returns F64; fr fast-paths cast F64→F32.
        // This assertion fails if Mode silently takes an fr fast-path (F32).
        assert!(
            matches!(out.pixels, PixelData::F64(_)),
            "expected scalar majority path (F64) but Mode took an fr fast-path (F32)"
        );
        let f = out.pixels.to_f64();
        for v in f.iter() {
            assert!(*v == 1.0 || *v == 9.0, "mode produced a non-class value {v}");
        }
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
