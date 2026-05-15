//! Fused render pipeline: rescale → color_formula → composite → encode PNG.
//!
//! Hot path uses ndarray matrix operations end-to-end:
//! - normalise per band as one `mapv` (broadcast scalar arithmetic over `Array2`),
//! - colormap = elementwise LUT index (still scalar per cell, single linear pass),
//! - RGB = per-band `assign()` into the (H, W, 4) RGBA `Array3`.
//! Validity mask is applied as a single `Zip` over the alpha channel.

use bytes::Bytes;
use ndarray::{s, Array2, Array3, ArrayView2, Axis, Zip};

use crate::error::{AgError, AgResult};
use crate::pixel::ImageData;
use crate::render::color_formula::Op;
use crate::render::composite::{Composite, HandleGet};
use crate::render::encode::encode_png;
use crate::warp::resampler::Resampling;

/// Precompiled render plan for the hot path.
#[derive(Debug, Clone)]
pub struct CompiledRenderSpec {
    pub composite: Composite,
    pub rescale: Vec<(f64, f64)>,
    pub formula: Vec<Op>,
    pub bidx: Vec<usize>, // 0-indexed after compile
    pub nodata: Option<f64>,
    pub tile_size: u32,
    pub resampling: Resampling,
    pub auto_rgb: bool,
}

pub fn render(image: &ImageData, spec: &CompiledRenderSpec) -> AgResult<Bytes> {
    let bands = image.pixels.bands();
    let h = image.pixels.height();
    let w = image.pixels.width();
    let n = h * w;

    let src = image.pixels.to_f64();
    let auto_rgb = spec.auto_rgb && bands >= 3;
    let bidx: Vec<usize> = if spec.bidx.is_empty() {
        match (auto_rgb, spec.composite.bands) {
            (true, _) => vec![0, 1, 2],
            (false, 1) => vec![0],
            (false, 3) if bands >= 3 => vec![0, 1, 2],
            (false, n) => {
                return Err(AgError::UnsupportedPixel(format!(
                    "expected at least {n} bands, got {bands}"
                )))
            }
        }
    } else {
        spec.bidx.clone()
    };
    for &band in &bidx {
        if band >= bands {
            return Err(AgError::query(
                "bidx",
                format!("band {} out of range for {} bands", band + 1, bands),
            ));
        }
    }

    let _ = n;
    // Materialise per-band Array2<f64> views from the planar (C, H, W) source.
    // `to_owned()` makes each a contiguous Array2<f64> we can broadcast over.
    let band_views: Vec<Array2<f64>> = bidx
        .iter()
        .map(|&b| src.index_axis(Axis(0), b).to_owned())
        .collect();

    let nodata = spec.nodata.or(image.nodata);
    let valid: Option<Array2<bool>> = compute_valid_mask(image, &band_views, nodata, h, w);

    let mut rgba: Array3<u8> = match band_views.len() {
        1 => render_single_band(&band_views[0], valid.as_ref(), spec),
        3 => render_three_band(
            &band_views[0],
            &band_views[1],
            &band_views[2],
            valid.as_ref(),
            spec,
            auto_rgb,
        )?,
        other => {
            return Err(AgError::UnsupportedPixel(format!(
                "expected 1 or 3 bands after bidx, got {other}"
            )))
        }
    };

    let target = spec.tile_size as usize;
    if target > 0 && (h != target || w != target) {
        rgba = resize_rgba(&rgba, target, spec.resampling);
    }
    encode_png(&rgba)
}

/// Build a `(h, w)` validity mask. Returns `None` if every pixel is valid so
/// the caller can skip the mask check entirely.
///
/// Implemented as a sequence of `Zip` reductions over the input `Array2`s —
/// pure ndarray broadcasting, no manual indexing.
fn compute_valid_mask(
    image: &ImageData,
    bands: &[Array2<f64>],
    nodata: Option<f64>,
    h: usize,
    w: usize,
) -> Option<Array2<bool>> {
    let mask_present = image.mask.is_some();
    if !mask_present && nodata.is_none() {
        return None;
    }
    let mut valid = Array2::<bool>::from_elem((h, w), true);
    if let Some(m) = &image.mask {
        valid.assign(m);
    }
    if let Some(nd) = nodata {
        let is_nan = nd.is_nan();
        for band in bands {
            if is_nan {
                Zip::from(&mut valid).and(band).for_each(|v, &x| {
                    if x.is_nan() {
                        *v = false;
                    }
                });
            } else {
                Zip::from(&mut valid).and(band).for_each(|v, &x| {
                    if (x - nd).abs() < 1e-9 {
                        *v = false;
                    }
                });
            }
        }
    }
    Some(valid)
}

/// Apply the validity mask to the alpha channel of an RGBA `Array3<u8>`.
fn apply_valid_to_alpha(rgba: &mut Array3<u8>, valid: Option<&Array2<bool>>) {
    if let Some(v) = valid {
        Zip::from(rgba.slice_mut(s![.., .., 3]))
            .and(v)
            .for_each(|a, &ok| {
                if !ok {
                    *a = 0;
                }
            });
    }
}

/// Single-band path: matrix-style rescale → LUT index Array2<u8> → fill RGBA.
fn render_single_band(
    band: &Array2<f64>,
    valid: Option<&Array2<bool>>,
    spec: &CompiledRenderSpec,
) -> Array3<u8> {
    use crate::render::composite::CompositeKind;
    let comp = &spec.composite;
    let (h, w) = band.dim();

    match comp.kind {
        CompositeKind::Gradient => {
            let lut = comp.lut.unwrap_or([[0u8; 4]; 256]);
            let (lo, hi) = match (&comp.vmin, &comp.vmax) {
                (Some(a), Some(b)) => (a[0], b[0]),
                _ => (0.0, 1.0),
            };
            let scale = 255.0 / (hi - lo).max(1e-12);

            // Matrix-style normalise: out = clamp((band - lo) * scale, 0, 255) as u8
            let idx = band.mapv(|v| (((v - lo) * scale).clamp(0.0, 255.0)) as u8);

            let mut rgba = Array3::<u8>::zeros((h, w, 4));
            interleave_lut(&idx, &lut, &mut rgba);
            apply_valid_to_alpha(&mut rgba, valid);
            rgba
        }
        _ => {
            // Discrete / single-band-rgb fallback: per-pixel composite.get.
            let mut rgba = Array3::<u8>::zeros((h, w, 4));
            Zip::indexed(band).for_each(|(r, c), &v| {
                let px = comp.get(&[v], None);
                rgba.slice_mut(s![r, c, ..])
                    .iter_mut()
                    .zip(px.iter())
                    .for_each(|(dst, src)| *dst = *src);
            });
            apply_valid_to_alpha(&mut rgba, valid);
            rgba
        }
    }
}

/// Three-band RGB path: per-band matrix rescale → channel `assign()` into RGBA.
fn render_three_band(
    r_band: &Array2<f64>,
    g_band: &Array2<f64>,
    b_band: &Array2<f64>,
    valid: Option<&Array2<bool>>,
    spec: &CompiledRenderSpec,
    auto_rgb: bool,
) -> AgResult<Array3<u8>> {
    let (h, w) = r_band.dim();
    let has_formula = !spec.formula.is_empty();
    let ranges = rgb_ranges(spec);

    if has_formula {
        // Color formula goes via Lab/Lch and is unavoidably per-pixel.
        let mut rgba = Array3::<u8>::zeros((h, w, 4));
        Zip::indexed(r_band).for_each(|(r_idx, c_idx), &rv| {
            let gv = g_band[[r_idx, c_idx]];
            let bv = b_band[[r_idx, c_idx]];
            let r = normalize(rv, ranges[0]);
            let g = normalize(gv, ranges[1]);
            let b = normalize(bv, ranges[2]);
            let (r, g, b) = crate::render::color_formula::apply(&spec.formula, r, g, b);
            rgba[[r_idx, c_idx, 0]] = unit_to_u8(r);
            rgba[[r_idx, c_idx, 1]] = unit_to_u8(g);
            rgba[[r_idx, c_idx, 2]] = unit_to_u8(b);
            rgba[[r_idx, c_idx, 3]] = 255;
        });
        apply_valid_to_alpha(&mut rgba, valid);
        return Ok(rgba);
    }

    if auto_rgb || matches!(spec.composite.kind, crate::render::composite::CompositeKind::Rgb) {
        // Per-band matrix-style rescale to u8 then channel-wise assign.
        let r_u8 = scale_to_u8(r_band.view(), ranges[0]);
        let g_u8 = scale_to_u8(g_band.view(), ranges[1]);
        let b_u8 = scale_to_u8(b_band.view(), ranges[2]);
        let mut rgba = Array3::<u8>::zeros((h, w, 4));
        rgba.slice_mut(s![.., .., 0]).assign(&r_u8);
        rgba.slice_mut(s![.., .., 1]).assign(&g_u8);
        rgba.slice_mut(s![.., .., 2]).assign(&b_u8);
        rgba.slice_mut(s![.., .., 3]).fill(255);
        apply_valid_to_alpha(&mut rgba, valid);
        return Ok(rgba);
    }

    // 3-band into a gradient/discrete composite — uncommon. Fallback.
    let mut rgba = Array3::<u8>::zeros((h, w, 4));
    Zip::indexed(r_band).for_each(|(r_idx, c_idx), &rv| {
        let gv = g_band[[r_idx, c_idx]];
        let bv = b_band[[r_idx, c_idx]];
        let px = spec.composite.get(&[rv, gv, bv], None);
        rgba.slice_mut(s![r_idx, c_idx, ..])
            .iter_mut()
            .zip(px.iter())
            .for_each(|(dst, src)| *dst = *src);
    });
    apply_valid_to_alpha(&mut rgba, valid);
    Ok(rgba)
}

/// `(band - lo) / span` clamped to [0, 1] times 255, as u8 — single `mapv`.
fn scale_to_u8(band: ArrayView2<f64>, (lo, hi): (f64, f64)) -> Array2<u8> {
    let span = (hi - lo).max(1e-12);
    band.mapv(|v| (((v - lo) / span).clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// Fill `rgba` using an `(h, w)` LUT-index array and a 256×4 LUT.
fn interleave_lut(idx: &Array2<u8>, lut: &[[u8; 4]; 256], rgba: &mut Array3<u8>) {
    // We could express this as a single Zip over the leading axis of rgba but
    // a slice copy of 4 bytes per pixel is just as cache-friendly and skips
    // ndarray bounds checks in debug.
    let (h, w) = idx.dim();
    let idx_slice = idx
        .as_slice()
        .expect("idx Array2<u8>::mapv is standard layout");
    let rgba_slice = rgba
        .as_slice_mut()
        .expect("rgba Array3<u8>::zeros is standard layout");
    debug_assert_eq!(rgba_slice.len(), h * w * 4);
    for (i, &t) in idx_slice.iter().enumerate() {
        let base = i * 4;
        rgba_slice[base..base + 4].copy_from_slice(&lut[t as usize]);
    }
}

fn rgb_ranges(spec: &CompiledRenderSpec) -> [(f64, f64); 3] {
    [
        spec.rescale.first().copied().unwrap_or((0.0, 255.0)),
        spec.rescale.get(1).copied().unwrap_or((0.0, 255.0)),
        spec.rescale.get(2).copied().unwrap_or((0.0, 255.0)),
    ]
}

fn normalize(value: f64, (lo, hi): (f64, f64)) -> f64 {
    let span = (hi - lo).max(1e-12);
    ((value - lo) / span).clamp(0.0, 1.0)
}

fn unit_to_u8(value: f64) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}


fn resize_rgba(src: &Array3<u8>, size: usize, resampling: Resampling) -> Array3<u8> {
    let (h, w) = (src.shape()[0], src.shape()[1]);
    if h == size && w == size {
        return src.clone();
    }
    let mut dst = Array3::<u8>::zeros((size, size, 4));
    let scale_x = w as f64 / size as f64;
    let scale_y = h as f64 / size as f64;
    for r in 0..size {
        for c in 0..size {
            let x = (c as f64 + 0.5) * scale_x - 0.5;
            let y = (r as f64 + 0.5) * scale_y - 0.5;
            match resampling {
                Resampling::Nearest => copy_nearest(src, &mut dst, c, r, x, y),
                Resampling::Bilinear => copy_bilinear(src, &mut dst, c, r, x, y),
            }
        }
    }
    dst
}

fn copy_nearest(src: &Array3<u8>, dst: &mut Array3<u8>, c: usize, r: usize, x: f64, y: f64) {
    let max_x = src.shape()[1].saturating_sub(1) as i64;
    let max_y = src.shape()[0].saturating_sub(1) as i64;
    let sx = (x.round() as i64).clamp(0, max_x) as usize;
    let sy = (y.round() as i64).clamp(0, max_y) as usize;
    for ch in 0..4 {
        dst[[r, c, ch]] = src[[sy, sx, ch]];
    }
}

fn copy_bilinear(src: &Array3<u8>, dst: &mut Array3<u8>, c: usize, r: usize, x: f64, y: f64) {
    let max_x = src.shape()[1].saturating_sub(1) as i64;
    let max_y = src.shape()[0].saturating_sub(1) as i64;
    let x0 = (x.floor() as i64).clamp(0, max_x);
    let y0 = (y.floor() as i64).clamp(0, max_y);
    let x1 = (x0 + 1).clamp(0, max_x);
    let y1 = (y0 + 1).clamp(0, max_y);
    let dx = (x - x0 as f64).clamp(0.0, 1.0);
    let dy = (y - y0 as f64).clamp(0.0, 1.0);
    for ch in 0..4 {
        let v00 = src[[y0 as usize, x0 as usize, ch]] as f64;
        let v01 = src[[y0 as usize, x1 as usize, ch]] as f64;
        let v10 = src[[y1 as usize, x0 as usize, ch]] as f64;
        let v11 = src[[y1 as usize, x1 as usize, ch]] as f64;
        let top = v00 * (1.0 - dx) + v01 * dx;
        let bot = v10 * (1.0 - dx) + v11 * dx;
        dst[[r, c, ch]] = (top * (1.0 - dy) + bot * dy).round().clamp(0.0, 255.0) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bounds::{Bbox, GeoTransform};
    use crate::crs::Crs;
    use crate::pixel::PixelData;
    use crate::render::composite::{ColourDefinition, Composite};

    #[test]
    fn render_single_band_gradient_to_png() {
        let mut arr = Array3::<f64>::zeros((1, 16, 16));
        for r in 0..16 {
            for c in 0..16 {
                arr[[0, r, c]] = (c as f64) / 15.0;
            }
        }
        let img = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 16.0, 16.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 16.0, 0.0, -1.0]),
            nodata: None,
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Named("viridis".into()),
            Some(vec![0.0]),
            Some(vec![1.0]),
        )
        .unwrap();
        let spec = CompiledRenderSpec {
            composite: comp,
            rescale: vec![],
            formula: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 16,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };
        let bytes = render(&img, &spec).unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));
    }

    #[test]
    fn render_resizes_to_tile_size() {
        let arr = Array3::<f64>::from_shape_vec((1, 2, 4), vec![0.0; 8]).unwrap();
        let img = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 4.0, 2.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 2.0, 0.0, -1.0]),
            nodata: None,
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Named("greys".into()),
            Some(vec![0.0]),
            Some(vec![1.0]),
        )
        .unwrap();
        let spec = CompiledRenderSpec {
            composite: comp,
            rescale: vec![],
            formula: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 8,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };
        let bytes = render(&img, &spec).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (8, 8));
    }

    #[test]
    fn render_uses_dataset_nodata_before_rescale() {
        let arr = Array3::<f64>::from_shape_vec((1, 1, 2), vec![-9999.0, 100.0]).unwrap();
        let img = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 2.0, 1.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 1.0, 0.0, -1.0]),
            nodata: Some(-9999.0),
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Named("greys".into()),
            Some(vec![0.0]),
            Some(vec![1.0]),
        )
        .unwrap();
        let spec = CompiledRenderSpec {
            composite: comp,
            rescale: vec![(0.0, 100.0)],
            formula: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 2,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };
        let bytes = render(&img, &spec).unwrap();
        let rgba = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0[3], 0);
        assert_eq!(rgba.get_pixel(1, 0).0[3], 255);
    }

    #[test]
    fn render_rejects_out_of_range_bidx() {
        let arr = Array3::<f64>::zeros((1, 1, 1));
        let img = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 1.0, 0.0, -1.0]),
            nodata: None,
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Named("greys".into()),
            Some(vec![0.0]),
            Some(vec![1.0]),
        )
        .unwrap();
        let spec = CompiledRenderSpec {
            composite: comp,
            rescale: vec![],
            formula: vec![],
            bidx: vec![1],
            nodata: None,
            tile_size: 1,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };
        assert!(render(&img, &spec).is_err());
    }

    #[test]
    fn rgb_rescale_is_applied_once() {
        let arr = Array3::<f64>::from_shape_vec((3, 1, 1), vec![128.0, 64.0, 32.0]).unwrap();
        let img = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 1.0, 0.0, -1.0]),
            nodata: None,
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Rgb([0.0, 0.0, 0.0], [255.0, 255.0, 255.0]),
            None,
            None,
        )
        .unwrap();
        let spec = CompiledRenderSpec {
            composite: comp,
            rescale: vec![(0.0, 255.0), (0.0, 255.0), (0.0, 255.0)],
            formula: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 1,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };

        let bytes = render(&img, &spec).unwrap();
        let rgba = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0, [128, 64, 32, 255]);
    }

    #[test]
    fn auto_rgb_uses_first_three_bands_with_byte_range() {
        let arr = Array3::<f64>::from_shape_vec((3, 1, 1), vec![120.0, 80.0, 40.0]).unwrap();
        let img = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 1.0, 0.0, -1.0]),
            nodata: None,
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Named("greys".into()),
            Some(vec![0.0]),
            Some(vec![1.0]),
        )
        .unwrap();
        let spec = CompiledRenderSpec {
            composite: comp,
            rescale: vec![],
            formula: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 1,
            resampling: Resampling::Nearest,
            auto_rgb: true,
        };

        let bytes = render(&img, &spec).unwrap();
        let rgba = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0, [120, 80, 40, 255]);
    }
}
