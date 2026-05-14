//! Coarse-grid warp approximation.
//!
//! Sample a 32×32 grid in destination pixel space through PROJ once (batched);
//! bilinear-interpolate intermediate dst→src pixel coords; sample source pixels
//! with the chosen kernel.

use ndarray::Array3;

use crate::bounds::{Bbox, GeoTransform};
use crate::crs::Crs;
use crate::error::{AgError, AgResult};
use crate::pixel::{ImageData, PixelData};
use crate::warp::projection::Projector;
use crate::warp::resampler::Resampling;

pub fn warp(
    src: &ImageData,
    dst_crs: &Crs,
    dst_bbox: Bbox,
    dst_w: usize,
    dst_h: usize,
    resampling: Resampling,
) -> AgResult<ImageData> {
    if &src.crs == dst_crs {
        // Fast path: caller should have skipped warp. Defensive clone.
        return Ok(src.clone());
    }

    let proj = Projector::new(dst_crs, &src.crs)?;

    const N: usize = 32;
    let mut grid_dst: Vec<(f64, f64)> = Vec::with_capacity((N + 1) * (N + 1));
    for i in 0..=N {
        for j in 0..=N {
            let dc = (j as f64) * (dst_w as f64) / (N as f64);
            let dr = (i as f64) * (dst_h as f64) / (N as f64);
            let dx = dst_bbox.xmin + dc / dst_w as f64 * dst_bbox.width();
            let dy = dst_bbox.ymax - dr / dst_h as f64 * dst_bbox.height();
            grid_dst.push((dx, dy));
        }
    }
    proj.convert(&mut grid_dst)?;

    let src_inv = src
        .transform
        .inverse()
        .ok_or_else(|| AgError::Warp("src GeoTransform not invertible".into()))?;
    let grid_src_pix: Vec<(f64, f64)> = grid_dst
        .into_iter()
        .map(|(x, y)| src_inv.forward(x, y))
        .collect();

    let bands = src.pixels.bands();
    let src_f64 = src.pixels.to_f64();
    let fill = src.nodata.unwrap_or(0.0);

    let mut out = Array3::<f64>::zeros((bands, dst_h, dst_w));
    for r in 0..dst_h {
        for c in 0..dst_w {
            let gi = (r as f64) * (N as f64) / (dst_h as f64);
            let gj = (c as f64) * (N as f64) / (dst_w as f64);
            let i0 = (gi.floor() as usize).min(N);
            let j0 = (gj.floor() as usize).min(N);
            let i1 = (i0 + 1).min(N);
            let j1 = (j0 + 1).min(N);
            let di = gi - i0 as f64;
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
            for b in 0..bands {
                let band_view = src_f64.index_axis(ndarray::Axis(0), b);
                out[[b, r, c]] = resampling.sample(band_view, sc, sr, fill);
            }
        }
    }

    let dst_transform = GeoTransform::from_gdal(&[
        dst_bbox.xmin,
        dst_bbox.width() / dst_w as f64,
        0.0,
        dst_bbox.ymax,
        0.0,
        -dst_bbox.height() / dst_h as f64,
    ]);

    Ok(ImageData {
        pixels: PixelData::F64(out),
        mask: None,
        bbox: dst_bbox,
        crs: dst_crs.clone(),
        transform: dst_transform,
        nodata: src.nodata,
    })
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
        let out = warp(
            &src,
            &Crs::epsg(3857),
            src.bbox,
            4,
            4,
            Resampling::Nearest,
        )
        .unwrap();
        assert_eq!(out.crs, src.crs);
    }
}
