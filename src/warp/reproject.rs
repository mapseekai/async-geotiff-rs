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

    // Fast path: same CRS — SIMD-accelerated crop+resize via fast_image_resize.
    if &src.crs == dst_crs {
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

    // Cross-CRS path: coarse 32×32 grid through PROJ + bilinear interpolation
    // of intermediate dst→src pixel coords. The Projector is fetched from a
    // thread-local cache (see `projection::with_cached`) so back-to-back
    // tile requests on the same worker thread skip PROJ context init.
    const N: usize = 32;
    let mut grid_dst: Vec<(f64, f64)> = Vec::with_capacity((N + 1) * (N + 1));
    for i in 0..=N {
        for j in 0..=N {
            let dx = dst_bbox.xmin + (j as f64) * dst_bbox.width() / (N as f64);
            let dy = dst_bbox.ymax - (i as f64) * dst_bbox.height() / (N as f64);
            grid_dst.push((dx, dy));
        }
    }
    crate::warp::projection::with_cached(dst_crs, &src.crs, |p| p.convert(&mut grid_dst))?;

    let grid_src_pix: Vec<(f64, f64)> = grid_dst
        .into_iter()
        .map(|(x, y)| src_inv.forward(x, y))
        .collect();

    let bands = src.pixels.bands();
    let src_f64 = src.pixels.to_f64();
    let fill = src.nodata.unwrap_or(0.0);
    let src_h = src_f64.shape()[1];
    let src_w = src_f64.shape()[2];

    let mut out = Array3::<f64>::zeros((bands, dst_h, dst_w));
    let mut mask = Array2::<bool>::from_elem((dst_h, dst_w), true);
    for r in 0..dst_h {
        let gi = (r as f64) * (N as f64) / (dst_h as f64);
        let i0 = (gi.floor() as usize).min(N - 1);
        let i1 = i0 + 1;
        let di = gi - i0 as f64;
        for c in 0..dst_w {
            let gj = (c as f64) * (N as f64) / (dst_w as f64);
            let j0 = (gj.floor() as usize).min(N - 1);
            let j1 = j0 + 1;
            let dj = gj - j0 as f64;
            let p00 = grid_src_pix[i0 * (N + 1) + j0];
            let p01 = grid_src_pix[i0 * (N + 1) + j1];
            let p10 = grid_src_pix[i1 * (N + 1) + j0];
            let p11 = grid_src_pix[i1 * (N + 1) + j1];
            let sc = p00.0 * (1.0 - di) * (1.0 - dj)
                + p01.0 * (1.0 - di) * dj
                + p10.0 * di * (1.0 - dj)
                + p11.0 * di * dj;
            let sr = p00.1 * (1.0 - di) * (1.0 - dj)
                + p01.1 * (1.0 - di) * dj
                + p10.1 * di * (1.0 - dj)
                + p11.1 * di * dj;
            mask[[r, c]] = sample_valid(src.mask.as_ref(), src_w, src_h, sc, sr);
            let valid_view = src.mask.as_ref().map(|m| m.view());
            for b in 0..bands {
                let band_view = src_f64.index_axis(ndarray::Axis(0), b);
                out[[b, r, c]] =
                    resampling.sample_masked(band_view, valid_view, sc, sr, fill);
            }
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

/// Same-CRS warp: pure crop+resize via `fast_image_resize` (SIMD: SSE4/AVX2/NEON).
///
/// Builds the source CropBox by mapping `dst_bbox` corners back through the
/// source GeoTransform; passes per-band F32 buffers to the resizer.
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
    use ndarray::Axis;

    // CropBox in source pixel space (covering exactly dst_bbox).
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

    // Clamp crop to source extent so fast_image_resize does not reject it.
    let crop_right = (crop_left + crop_w).min(src_w as f64);
    let crop_bot = (crop_top + crop_h).min(src_h as f64);
    let crop_w_clamped = (crop_right - crop_left).max(1e-9);
    let crop_h_clamped = (crop_bot - crop_top).max(1e-9);

    let alg = match resampling {
        Resampling::Nearest => fr::ResizeAlg::Nearest,
        Resampling::Bilinear => fr::ResizeAlg::Convolution(fr::FilterType::Bilinear),
        // Catmull-Rom bicubic — same SIMD codepath in fast_image_resize as
        // bilinear, just a wider 4-tap kernel.
        Resampling::Cubic => fr::ResizeAlg::Convolution(fr::FilterType::CatmullRom),
    };
    let options = fr::ResizeOptions::new()
        .resize_alg(alg)
        .crop(crop_left, crop_top, crop_w_clamped, crop_h_clamped);
    let mut resizer = fr::Resizer::new();

    let src_f64 = src.pixels.to_f64();
    let mut out = Array3::<f32>::zeros((bands, dst_h, dst_w));

    for b in 0..bands {
        // Build an f32 buffer (Vec<u8> reinterpreted) per band.
        let band = src_f64.index_axis(Axis(0), b);
        let mut src_buf = vec![0u8; src_w * src_h * std::mem::size_of::<f32>()];
        {
            let f32_view: &mut [f32] = bytemuck::cast_slice_mut(&mut src_buf);
            for (slot, value) in f32_view.iter_mut().zip(band.iter()) {
                *slot = *value as f32;
            }
        }
        let src_img = fr::images::Image::from_vec_u8(
            src_w as u32,
            src_h as u32,
            src_buf,
            fr::PixelType::F32,
        )
        .map_err(|e| AgError::Warp(format!("fast_image_resize src: {e}")))?;

        let mut dst_img =
            fr::images::Image::new(dst_w as u32, dst_h as u32, fr::PixelType::F32);
        resizer
            .resize(&src_img, &mut dst_img, Some(&options))
            .map_err(|e| AgError::Warp(format!("fast_image_resize: {e}")))?;

        let dst_buf = dst_img.into_vec();
        let dst_f32: &[f32] = bytemuck::cast_slice(&dst_buf);
        let mut out_band = out.index_axis_mut(Axis(0), b);
        let out_slice = out_band
            .as_slice_mut()
            .expect("out_band must be contiguous");
        out_slice.copy_from_slice(dst_f32);
    }

    Ok(ImageData {
        pixels: PixelData::F32(out),
        mask: None,
        bbox: dst_bbox,
        crs: dst_crs.clone(),
        transform: dst_transform,
        nodata: src.nodata,
    })
}

fn sample_valid(
    mask: Option<&Array2<bool>>,
    width: usize,
    height: usize,
    col: f64,
    row: f64,
) -> bool {
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
}
