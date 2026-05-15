//! Fused render pipeline: rescale → color_formula → composite → encode PNG.

use bytes::Bytes;
use ndarray::Array3;

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

    // Hot-path optimisation: extract contiguous f64 slices once. Each band view
    // is a (h, w) C-order `Array2<f64>`; `to_owned()` + `into_raw_vec()` gives a
    // flat `Vec<f64>` of length n that we can index linearly.
    let band_buffers: Vec<Vec<f64>> = bidx
        .iter()
        .map(|&b| {
            src.index_axis(ndarray::Axis(0), b)
                .to_owned()
                .into_raw_vec_and_offset()
                .0
        })
        .collect();

    let nodata = spec.nodata.or(image.nodata);
    let valid = compute_valid(image, &band_buffers, nodata);

    let mut rgba = Array3::<u8>::zeros((h, w, 4));
    {
        let rgba_flat = rgba
            .as_slice_mut()
            .expect("rgba Array3<u8>::zeros is standard layout");
        match band_buffers.len() {
            1 => write_single_band(rgba_flat, &band_buffers[0], valid.as_deref(), spec),
            3 => write_three_band(
                rgba_flat,
                &band_buffers[0],
                &band_buffers[1],
                &band_buffers[2],
                valid.as_deref(),
                spec,
                auto_rgb,
            ),
            other => {
                return Err(AgError::UnsupportedPixel(format!(
                    "expected 1 or 3 bands after bidx, got {other}"
                )))
            }
        }
        let _ = n; // silence unused if logging removed
    }

    let target = spec.tile_size as usize;
    if target > 0 && (h != target || w != target) {
        rgba = resize_rgba(&rgba, target, spec.resampling);
    }
    encode_png(&rgba)
}

/// Build a flat validity mask (`true` = render this pixel). Returns `None`
/// when every pixel is valid — caller skips the check entirely.
fn compute_valid(
    image: &ImageData,
    bands: &[Vec<f64>],
    nodata: Option<f64>,
) -> Option<Vec<bool>> {
    let n = bands.first().map(|b| b.len()).unwrap_or(0);
    let mask_slice: Option<&[bool]> = image.mask.as_ref().and_then(|m| m.as_slice());

    if mask_slice.is_none() && nodata.is_none() {
        return None;
    }

    let mut out = vec![true; n];
    if let Some(m) = mask_slice {
        for i in 0..n {
            out[i] = m[i];
        }
    }
    if let Some(nd) = nodata {
        for band in bands {
            if nd.is_nan() {
                for (i, &v) in band.iter().enumerate() {
                    if v.is_nan() {
                        out[i] = false;
                    }
                }
            } else {
                for (i, &v) in band.iter().enumerate() {
                    if (v - nd).abs() < 1e-9 {
                        out[i] = false;
                    }
                }
            }
        }
    }
    Some(out)
}

/// Single-band gradient/discrete path — slice-only, no ndarray indexing,
/// and direct LUT lookup when the composite is a gradient.
fn write_single_band(
    rgba_flat: &mut [u8],
    band: &[f64],
    valid: Option<&[bool]>,
    spec: &CompiledRenderSpec,
) {
    use crate::render::composite::CompositeKind;
    let comp = &spec.composite;
    let n = band.len();
    match comp.kind {
        CompositeKind::Gradient => {
            let lut = comp.lut.unwrap_or([[0u8; 4]; 256]);
            let (lo, hi) = match (&comp.vmin, &comp.vmax) {
                (Some(a), Some(b)) => (a[0], b[0]),
                _ => (0.0, 1.0),
            };
            let scale = 255.0 / (hi - lo).max(1e-12);
            for i in 0..n {
                if valid.is_some_and(|v| !v[i]) {
                    continue;
                }
                let t = ((band[i] - lo) * scale).clamp(0.0, 255.0) as usize;
                let px = lut[t];
                let base = i * 4;
                rgba_flat[base..base + 4].copy_from_slice(&px);
            }
        }
        _ => {
            // Discrete/Rgb single-band fallback (rare) — keep general dispatch.
            for i in 0..n {
                if valid.is_some_and(|v| !v[i]) {
                    continue;
                }
                let px = comp.get(&[band[i]], None);
                let base = i * 4;
                rgba_flat[base..base + 4].copy_from_slice(&px);
            }
        }
    }
}

/// Three-band RGB path — same slice-only structure.
fn write_three_band(
    rgba_flat: &mut [u8],
    r_band: &[f64],
    g_band: &[f64],
    b_band: &[f64],
    valid: Option<&[bool]>,
    spec: &CompiledRenderSpec,
    auto_rgb: bool,
) {
    let n = r_band.len();
    let has_formula = !spec.formula.is_empty();
    let ranges = rgb_ranges(spec);
    for i in 0..n {
        if valid.is_some_and(|v| !v[i]) {
            continue;
        }
        let (rb, gb, bb) = (r_band[i], g_band[i], b_band[i]);
        let px = if has_formula {
            let r = normalize(rb, ranges[0]);
            let g = normalize(gb, ranges[1]);
            let b = normalize(bb, ranges[2]);
            let (r, g, b) = crate::render::color_formula::apply(&spec.formula, r, g, b);
            [unit_to_u8(r), unit_to_u8(g), unit_to_u8(b), 255]
        } else if auto_rgb {
            let r = normalize(rb, ranges[0]);
            let g = normalize(gb, ranges[1]);
            let b = normalize(bb, ranges[2]);
            [unit_to_u8(r), unit_to_u8(g), unit_to_u8(b), 255]
        } else {
            spec.composite.get(&[rb, gb, bb], None)
        };
        let base = i * 4;
        rgba_flat[base..base + 4].copy_from_slice(&px);
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
