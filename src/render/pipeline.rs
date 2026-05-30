//! Fused render pipeline: rescale → color_formula → composite → encode PNG.
//!
//! Hot path uses ndarray matrix operations end-to-end:
//! - normalise per band as one `mapv` (broadcast scalar arithmetic over `Array2`),
//! - colormap = elementwise LUT index (still scalar per cell, single linear pass),
//! - RGB = per-band `assign()` into the (H, W, 4) RGBA `Array3`.
//!
//! Validity mask is applied as a single `Zip` over the alpha channel.

use bytes::Bytes;
use ndarray::{ArrayView3, s, Array2, Array3, Axis, Zip};

use crate::error::{AgError, AgResult};
use crate::pixel::ImageData;
use crate::render::color_formula::Op;
use crate::render::composite::{Composite, HandleGet};
use crate::render::encode::encode_png;
use crate::render::expression::{eval_rpn, max_band_ref, Token};
use crate::warp::resampler::Resampling;

// `tile_size` and `resampling` on CompiledTileStyle are kept for callers
// (style/params.rs), but the render pipeline assumes warp has already produced
// an `image` matching `style.tile_size` exactly — no resize happens here.

/// Precompiled render plan for the hot path.
#[derive(Debug, Clone)]
pub struct CompiledTileStyle {
    pub composite: Composite,
    pub rescale: Vec<(f64, f64)>,
    pub formula: Vec<Op>,
    /// RPN tokens from a band-math `expression=` param. Empty when not used.
    pub expression: Vec<crate::render::expression::Token>,
    pub bidx: Vec<usize>, // 0-indexed after compile
    pub nodata: Option<f64>,
    pub tile_size: u32,
    pub resampling: Resampling,
    pub auto_rgb: bool,
}

pub fn render(image: &ImageData, style: &CompiledTileStyle) -> AgResult<Bytes> {
    let bands = image.pixels.bands();
    let h = image.pixels.height();
    let w = image.pixels.width();
    let n = h * w;

    let auto_rgb = style.auto_rgb && bands >= 3;

    // Fast path: source already u8, 3 bands, identity rescale, no formula.
    // Skip the f64 cast + per-pixel scale_to_u8: copy R/G/B bytes directly
    // into the interleaved RGBA buffer. This is the dominant case for 8-bit
    // RGB COGs (Sentinel TCI, NAIP, drone orthos), where every other render
    // path is wasted CPU.
    if let Some(rgba) = try_u8_rgb_fastpath(image, style, auto_rgb, h, w) {
        return encode_png(&rgba);
    }

    let src = image.pixels.to_f64();
    let bidx: Vec<usize> = if style.bidx.is_empty() {
        match (auto_rgb, style.composite.bands) {
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
        style.bidx.clone()
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

    let nodata = style.nodata.or(image.nodata);
    let valid: Option<Array2<bool>> = compute_valid_mask(image, &band_views, nodata, h, w);

    let rgba: Array3<u8> = match band_views.len() {
        1 => render_single_band(&band_views[0], valid.as_ref(), style),
        3 => render_three_band(
            &band_views[0],
            &band_views[1],
            &band_views[2],
            valid.as_ref(),
            style,
            auto_rgb,
        )?,
        other => {
            return Err(AgError::UnsupportedPixel(format!(
                "expected 1 or 3 bands after bidx, got {other}"
            )))
        }
    };

    // Render assumes the warp stage has already produced an image at
    // `style.tile_size`; if not, the encoded PNG simply matches input dims.
    let _ = style.tile_size;
    encode_png(&rgba)
}

/// Returns Some(rgba) when the u8 RGB fast path applies. Conditions:
/// - source PixelData is U8
/// - bidx selects exactly three bands (explicit or auto_rgb)
/// - composite is Rgb or auto_rgb is on (no colormap, no formula)
/// - rescale is missing or identity-on-byte-range for all three bands
///
/// The buffer is filled with a single linear sweep over the source bands;
/// nodata/mask only affect the alpha channel.
fn try_u8_rgb_fastpath(
    image: &ImageData,
    style: &CompiledTileStyle,
    auto_rgb: bool,
    h: usize,
    w: usize,
) -> Option<Array3<u8>> {
    use crate::pixel::PixelData;
    use crate::render::composite::CompositeKind;

    let src = match &image.pixels {
        PixelData::U8(a) => a,
        _ => return None,
    };
    if !style.formula.is_empty() {
        return None;
    }
    let bands = src.shape()[0];
    let bidx: [usize; 3] = if style.bidx.is_empty() {
        let want_rgb =
            (auto_rgb || matches!(style.composite.kind, CompositeKind::Rgb)) && bands >= 3;
        if !want_rgb {
            return None;
        }
        [0, 1, 2]
    } else if style.bidx.len() == 3 && style.bidx.iter().all(|&b| b < bands) {
        [style.bidx[0], style.bidx[1], style.bidx[2]]
    } else {
        return None;
    };
    // Identity rescale check: empty, or each (lo, hi) is (0, 255).
    let identity = style.rescale.is_empty()
        || (style.rescale.len() == 3
            && style
                .rescale
                .iter()
                .all(|(lo, hi)| *lo == 0.0 && *hi == 255.0));
    if !identity {
        return None;
    }
    let nodata = style.nodata.or(image.nodata);
    let nd_u8: Option<u8> = nodata.and_then(|nd| {
        if nd.is_finite() && (0.0..=255.0).contains(&nd) {
            Some(nd as u8)
        } else {
            None
        }
    });
    // Build RGBA in a single linear sweep over the contiguous byte buffer:
    // for each pixel write R, G, B, A together. Stride-4 scatter across
    // 3 separate passes is cache-unfriendly and was the same anti-pattern
    // H5 fixed in the f64 path.
    let mut rgba = Array3::<u8>::from_elem((h, w, 4), 0u8);
    let stride = h * w;
    let src_standard = src.as_standard_layout();
    let src_slice = src_standard
        .as_slice()
        .expect("standard-layout u8 PixelData must be contiguous");
    let r_band = &src_slice[bidx[0] * stride..][..stride];
    let g_band = &src_slice[bidx[1] * stride..][..stride];
    let b_band = &src_slice[bidx[2] * stride..][..stride];
    let mask_slice: Option<&[bool]> = image.mask.as_ref().and_then(|m| m.as_slice());
    let dst = rgba
        .as_slice_mut()
        .expect("freshly-allocated Array3 standard layout");

    for i in 0..stride {
        let r_val = r_band[i];
        let g_val = g_band[i];
        let b_val = b_band[i];
        let masked = matches!(mask_slice, Some(m) if !m[i]);
        // Per-band nodata semantics matching GDAL/rio-tiler: a pixel is
        // invalid if ANY band reads the nodata sentinel — not only when
        // all three happen to coincide.
        let nd_hit = matches!(nd_u8, Some(nd) if r_val == nd || g_val == nd || b_val == nd);
        let alpha: u8 = if masked || nd_hit { 0 } else { 255 };
        let base = i * 4;
        dst[base] = r_val;
        dst[base + 1] = g_val;
        dst[base + 2] = b_val;
        dst[base + 3] = alpha;
    }
    Some(rgba)
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
    style: &CompiledTileStyle,
) -> Array3<u8> {
    use crate::render::composite::CompositeKind;
    let comp = &style.composite;
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
    style: &CompiledTileStyle,
    auto_rgb: bool,
) -> AgResult<Array3<u8>> {
    let (h, w) = r_band.dim();
    let has_formula = !style.formula.is_empty();
    let ranges = rgb_ranges(style);

    if has_formula {
        // Color formula goes via Lab/Lch and is unavoidably per-pixel.
        let mut rgba = Array3::<u8>::zeros((h, w, 4));
        Zip::indexed(r_band).for_each(|(r_idx, c_idx), &rv| {
            let gv = g_band[[r_idx, c_idx]];
            let bv = b_band[[r_idx, c_idx]];
            let r = normalize(rv, ranges[0]);
            let g = normalize(gv, ranges[1]);
            let b = normalize(bv, ranges[2]);
            let (r, g, b) = crate::render::color_formula::apply(&style.formula, r, g, b);
            rgba[[r_idx, c_idx, 0]] = unit_to_u8(r);
            rgba[[r_idx, c_idx, 1]] = unit_to_u8(g);
            rgba[[r_idx, c_idx, 2]] = unit_to_u8(b);
            rgba[[r_idx, c_idx, 3]] = 255;
        });
        apply_valid_to_alpha(&mut rgba, valid);
        return Ok(rgba);
    }

    if auto_rgb
        || matches!(
            style.composite.kind,
            crate::render::composite::CompositeKind::Rgb
        )
    {
        // Fused single-pass rescale + interleave + alpha (mask/nodata).
        // Previous version did 3 mapv allocations (one Array2<u8> per band)
        // and 3 strided slice_mut().assign() into RGBA, then a separate
        // fill(255) and a Zip mask pass — five sweeps over h*w cells.
        // Now: one linear sweep over the contiguous RGBA byte buffer.
        let mut rgba = Array3::<u8>::zeros((h, w, 4));
        let n = h * w;
        let r_standard = r_band.as_standard_layout();
        let g_standard = g_band.as_standard_layout();
        let b_standard = b_band.as_standard_layout();
        let r_slice = r_standard
            .as_slice()
            .expect("standard-layout r_band must be contiguous");
        let g_slice = g_standard
            .as_slice()
            .expect("standard-layout g_band must be contiguous");
        let b_slice = b_standard
            .as_slice()
            .expect("standard-layout b_band must be contiguous");
        let rgba_slice = rgba
            .as_slice_mut()
            .expect("freshly-allocated RGBA standard layout");
        let mask_slice: Option<&[bool]> = valid.and_then(|v| v.as_slice());
        let (lo_r, hi_r) = ranges[0];
        let (lo_g, hi_g) = ranges[1];
        let (lo_b, hi_b) = ranges[2];
        let inv_r = 255.0 / (hi_r - lo_r).max(1e-12);
        let inv_g = 255.0 / (hi_g - lo_g).max(1e-12);
        let inv_b = 255.0 / (hi_b - lo_b).max(1e-12);
        for i in 0..n {
            // Pre-scale: (v - lo) * (255 / span), then clamp+cast.
            let r_u8 = ((r_slice[i] - lo_r) * inv_r).clamp(0.0, 255.0) as u8;
            let g_u8 = ((g_slice[i] - lo_g) * inv_g).clamp(0.0, 255.0) as u8;
            let b_u8 = ((b_slice[i] - lo_b) * inv_b).clamp(0.0, 255.0) as u8;
            let alpha = match mask_slice {
                Some(m) if !m[i] => 0,
                _ => 255,
            };
            let base = i * 4;
            rgba_slice[base] = r_u8;
            rgba_slice[base + 1] = g_u8;
            rgba_slice[base + 2] = b_u8;
            rgba_slice[base + 3] = alpha;
        }
        return Ok(rgba);
    }

    // 3-band into a gradient/discrete composite — uncommon. Fallback.
    let mut rgba = Array3::<u8>::zeros((h, w, 4));
    Zip::indexed(r_band).for_each(|(r_idx, c_idx), &rv| {
        let gv = g_band[[r_idx, c_idx]];
        let bv = b_band[[r_idx, c_idx]];
        let px = style.composite.get(&[rv, gv, bv], None);
        rgba.slice_mut(s![r_idx, c_idx, ..])
            .iter_mut()
            .zip(px.iter())
            .for_each(|(dst, src)| *dst = *src);
    });
    apply_valid_to_alpha(&mut rgba, valid);
    Ok(rgba)
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

fn rgb_ranges(style: &CompiledTileStyle) -> [(f64, f64); 3] {
    [
        style.rescale.first().copied().unwrap_or((0.0, 255.0)),
        style.rescale.get(1).copied().unwrap_or((0.0, 255.0)),
        style.rescale.get(2).copied().unwrap_or((0.0, 255.0)),
    ]
}

fn normalize(value: f64, (lo, hi): (f64, f64)) -> f64 {
    let span = (hi - lo).max(1e-12);
    ((value - lo) / span).clamp(0.0, 1.0)
}

fn unit_to_u8(value: f64) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Evaluate a band-math RPN expression over every pixel in `src` (shape C×H×W).
///
/// Returns `(result_band, valid_mask)`:
/// - `result_band`: `Array2<f64>` of shape `(h, w)` with the expression value per pixel.
/// - `valid_mask`: `Array2<bool>` of shape `(h, w)`; `false` where the result is
///   non-finite (NaN / ±Inf) or any referenced band pixel equals `nodata`.
///
/// Errors if the expression references a band index that exceeds the number of
/// source bands in `src` (a `AgError::Query`).
pub fn eval_expression_band(
    src: &ArrayView3<f64>,
    rpn: &[Token],
    nodata: Option<f64>,
    h: usize,
    w: usize,
) -> AgResult<(Array2<f64>, Array2<bool>)> {
    let bands = src.shape()[0];
    if let Some(max_idx) = max_band_ref(rpn) {
        if max_idx >= bands {
            return Err(AgError::query(
                "expression",
                format!(
                    "expression references b{} but source has only {} band(s)",
                    max_idx + 1,
                    bands
                ),
            ));
        }
    }

    let mut result = Array2::<f64>::zeros((h, w));
    let mut valid = Array2::<bool>::from_elem((h, w), true);
    let mut pixel_bands: Vec<f64> = vec![0.0; bands];

    let is_nan_nodata = nodata.map(|nd| nd.is_nan()).unwrap_or(false);

    for row in 0..h {
        for col in 0..w {
            // Gather all band values for this pixel.
            for b in 0..bands {
                pixel_bands[b] = src[[b, row, col]];
            }

            // Check nodata in any band referenced by the expression.
            let mut nd_hit = false;
            if let Some(nd) = nodata {
                for &b in rpn.iter().filter_map(|t| match t {
                    Token::Band(i) => Some(i),
                    _ => None,
                }) {
                    let v = pixel_bands[b];
                    if is_nan_nodata && v.is_nan() || !is_nan_nodata && (v - nd).abs() < 1e-9 {
                        nd_hit = true;
                        break;
                    }
                }
            }

            let value = eval_rpn(rpn, &pixel_bands);
            result[[row, col]] = value;
            valid[[row, col]] = !nd_hit && value.is_finite();
        }
    }

    Ok((result, valid))
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
        let style = CompiledTileStyle {
            composite: comp,
            rescale: vec![],
            formula: vec![],
            expression: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 16,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };
        let bytes = render(&img, &style).unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));
    }

    #[test]
    fn render_emits_tile_at_input_dims() {
        // Render now assumes warp has already produced an image at tile_size;
        // it does not resize internally. Verify the output dims match input.
        let arr = Array3::<f64>::from_shape_vec((1, 8, 8), vec![0.0; 64]).unwrap();
        let img = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 8.0, 8.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 8.0, 0.0, -1.0]),
            nodata: None,
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Named("greys".into()),
            Some(vec![0.0]),
            Some(vec![1.0]),
        )
        .unwrap();
        let style = CompiledTileStyle {
            composite: comp,
            rescale: vec![],
            formula: vec![],
            expression: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 8,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };
        let bytes = render(&img, &style).unwrap();
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
        let style = CompiledTileStyle {
            composite: comp,
            rescale: vec![(0.0, 100.0)],
            formula: vec![],
            expression: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 2,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };
        let bytes = render(&img, &style).unwrap();
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
        let style = CompiledTileStyle {
            composite: comp,
            rescale: vec![],
            formula: vec![],
            expression: vec![],
            bidx: vec![1],
            nodata: None,
            tile_size: 1,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };
        assert!(render(&img, &style).is_err());
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
        let style = CompiledTileStyle {
            composite: comp,
            rescale: vec![(0.0, 255.0), (0.0, 255.0), (0.0, 255.0)],
            formula: vec![],
            expression: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 1,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };

        let bytes = render(&img, &style).unwrap();
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
        let style = CompiledTileStyle {
            composite: comp,
            rescale: vec![],
            formula: vec![],
            expression: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 1,
            resampling: Resampling::Nearest,
            auto_rgb: true,
        };

        let bytes = render(&img, &style).unwrap();
        let rgba = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0, [120, 80, 40, 255]);
    }
}
