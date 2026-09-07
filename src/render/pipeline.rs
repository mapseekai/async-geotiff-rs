//! Fused render pipeline: rescale → color_formula → composite → encode
//! (PNG/WebP/JPEG per `style.format`).
//!
//! Hot path uses ndarray matrix operations end-to-end:
//! - normalise per band as one `mapv` (broadcast scalar arithmetic over `Array2`),
//! - colormap = elementwise LUT index (still scalar per cell, single linear pass),
//! - RGB = per-band `assign()` into the (H, W, 4) RGBA `Array3`.
//!
//! Validity mask is applied as a single `Zip` over the alpha channel.

use std::sync::Arc;

use bytes::Bytes;
use ndarray::{s, Array2, Array3, ArrayView2, ArrayView3, Axis, Zip};

use crate::error::{AgError, AgResult};
use crate::pixel::{ImageData, PixelData};
use crate::render::color_formula::Op;
use crate::render::composite::{Composite, HandleGet};
use crate::render::encode::{encode, TileFormat};
use crate::render::expression::{eval_rpn_with, max_band_ref, Token};
use crate::warp::resampler::Resampling;

// `tile_size` and `resampling` on CompiledTileStyle are kept for callers
// (style/params.rs), but the render pipeline assumes warp has already produced
// an `image` matching `style.tile_size` exactly — no resize happens here.

/// Lazily-built lookup tables for u16 sources, hanging off the compiled style
/// behind an `Arc`. All tiles sharing one effective style (via `compile_cached`
/// or the `Tiler`'s effective-style memo) build each table once and then
/// replace per-pixel float rescale math with an indexed load. Code that clones
/// a `CompiledTileStyle` and CHANGES `rescale`/`vmin`/`vmax` must install a
/// fresh `Arc<RenderLuts>` — a shared cell with different ranges would render
/// wrong colors.
#[derive(Debug, Default)]
pub struct RenderLuts {
    /// Per-channel 65536-entry u16→u8 rescale tables for the 3-band RGB path.
    rgb_u16: std::sync::OnceLock<Arc<[Vec<u8>; 3]>>,
    /// 65536-entry u16→RGBA table fusing rescale + colormap for the
    /// single-band Gradient path.
    grad_u16: std::sync::OnceLock<Arc<Vec<[u8; 4]>>>,
}

/// Immutable precompiled render plan for the hot path.
/// Construct a new plan with [`crate::style::params::compile`] when changing
/// a style; private fields keep cached lookup tables consistent with ranges.
///
/// ```compile_fail
/// let mut compiled = async_geotiff::style::params::compile(&Default::default()).unwrap();
/// compiled.rescale.push((0.0, 1.0));
/// ```
#[derive(Debug, Clone)]
pub struct CompiledTileStyle {
    pub(crate) composite: Composite,
    pub(crate) rescale: Vec<(f64, f64)>,
    pub(crate) formula: Vec<Op>,
    /// RPN tokens from a band-math `expression=` param. Empty when not used.
    pub(crate) expression: Vec<crate::render::expression::Token>,
    pub(crate) bidx: Vec<usize>, // 0-indexed after compile
    pub(crate) nodata: Option<f64>,
    pub(crate) tile_size: u32,
    pub(crate) resampling: Resampling,
    pub(crate) auto_rgb: bool,
    /// Output encoding selected by the request.
    pub(crate) format: TileFormat,
    /// See [`RenderLuts`]. Reset to a fresh `Arc` whenever `rescale`/vmin/vmax
    /// are rewritten on a clone.
    pub(crate) luts: Arc<RenderLuts>,
}

impl CompiledTileStyle {
    /// Resampling selected by the source request; rendering itself does not resize.
    pub fn resampling(&self) -> Resampling {
        self.resampling
    }

    /// Effective read-only rescale ranges. Recompile a TileStyle to change them.
    pub fn rescale(&self) -> &[(f64, f64)] {
        &self.rescale
    }
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
        return encode(&rgba, style.format);
    }

    // u16 RGB via per-band LUT: bakes the float rescale into three 65536-entry
    // tables (built once per effective style, shared across tiles), turning the
    // per-pixel `(v - lo) * inv → clamp → cast` chain into an indexed load.
    // This is the dominant path for 16-bit satellite imagery (Sentinel-2 L2A,
    // Landsat C2) browsed with a stats- or user-supplied stretch.
    if let Some(rgba) = try_u16_rgb_lut_fastpath(image, style, auto_rgb, h, w) {
        return encode(&rgba, style.format);
    }

    // Expression path: evaluate band-math RPN per-pixel → synthetic single band.
    if !style.expression.is_empty() {
        // Promote to f64 only on this path; `as_f64` borrows when the source is
        // already F64 (no full-image clone).
        let src = image.pixels.as_f64();
        let nodata = style.nodata.or(image.nodata);
        let (band, valid) = eval_expression_band(&src.view(), &style.expression, nodata, h, w)?;
        let valid_opt = match image.mask.as_ref() {
            Some(mask) => {
                let mut valid = valid;
                Zip::from(&mut valid).and(mask).for_each(|ok, &source_ok| {
                    *ok &= source_ok;
                });
                Some(valid)
            }
            None if valid.iter().all(|&ok| ok) => None,
            None => Some(valid),
        };
        let rgba = render_single_band(&band.view(), valid_opt.as_ref(), style);
        return encode(&rgba, style.format);
    }

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
    // Render selected bands through typed views; bidx is range-checked above.
    let nodata = style.nodata.or(image.nodata);
    let rgba = render_typed_bands(
        &image.pixels,
        &bidx,
        image.mask.as_ref(),
        nodata,
        style,
        auto_rgb,
    )?;

    // Render assumes the warp stage has already produced an image at
    // `style.tile_size`; if not, the encoded PNG simply matches input dims.
    let _ = style.tile_size;
    encode(&rgba, style.format)
}

trait RenderSample: Copy {
    fn to_f64(self) -> f64;
}

macro_rules! impl_render_sample {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl RenderSample for $ty {
                #[inline]
                fn to_f64(self) -> f64 {
                    self as f64
                }
            }
        )+
    };
}

impl_render_sample!(u8, u16, i16, u32, i32, f32, f64);

fn render_typed_bands(
    pixels: &PixelData,
    bidx: &[usize],
    mask: Option<&Array2<bool>>,
    nodata: Option<f64>,
    style: &CompiledTileStyle,
    auto_rgb: bool,
) -> AgResult<Array3<u8>> {
    match pixels {
        PixelData::U8(a) => render_typed_array(a.view(), bidx, mask, nodata, style, auto_rgb),
        PixelData::U16(a) => {
            // Fused rescale+colormap LUT for the single-band Gradient case
            // (DEM/single-band 16-bit rasters); other shapes use the generic
            // typed path below.
            if let Some(rgba) = try_u16_single_gradient_lut(a, bidx, mask, nodata, style) {
                return Ok(rgba);
            }
            render_typed_array(a.view(), bidx, mask, nodata, style, auto_rgb)
        }
        PixelData::I16(a) => render_typed_array(a.view(), bidx, mask, nodata, style, auto_rgb),
        PixelData::U32(a) => render_typed_array(a.view(), bidx, mask, nodata, style, auto_rgb),
        PixelData::I32(a) => render_typed_array(a.view(), bidx, mask, nodata, style, auto_rgb),
        PixelData::F32(a) => render_typed_array(a.view(), bidx, mask, nodata, style, auto_rgb),
        PixelData::F64(a) => render_typed_array(a.view(), bidx, mask, nodata, style, auto_rgb),
    }
}

fn render_typed_array<T: RenderSample>(
    src: ArrayView3<'_, T>,
    bidx: &[usize],
    mask: Option<&Array2<bool>>,
    nodata: Option<f64>,
    style: &CompiledTileStyle,
    auto_rgb: bool,
) -> AgResult<Array3<u8>> {
    let h = src.shape()[1];
    let w = src.shape()[2];
    match bidx {
        [b0] => {
            let bands = [src.index_axis(Axis(0), *b0)];
            let valid = compute_valid_mask(mask, &bands, nodata, h, w);
            Ok(render_single_band(&bands[0], valid.as_ref(), style))
        }
        [b0, b1, b2] => {
            let bands = [
                src.index_axis(Axis(0), *b0),
                src.index_axis(Axis(0), *b1),
                src.index_axis(Axis(0), *b2),
            ];
            let valid = compute_valid_mask(mask, &bands, nodata, h, w);
            render_three_band(
                &bands[0],
                &bands[1],
                &bands[2],
                valid.as_ref(),
                style,
                auto_rgb,
            )
        }
        other => Err(AgError::UnsupportedPixel(format!(
            "expected 1 or 3 bands after bidx, got {}",
            other.len()
        ))),
    }
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
    if !style.formula.is_empty() || !style.expression.is_empty() {
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

/// One channel's u16→u8 rescale table — exactly the generic path's
/// `((v - lo) * (255 / span)).clamp(0, 255) as u8` evaluated for every
/// possible sample value, so LUT output is bit-identical to the scalar math.
fn build_u16_rescale_lut((lo, hi): (f64, f64)) -> Vec<u8> {
    let inv = 255.0 / (hi - lo).max(1e-12);
    (0..=u16::MAX as u32)
        .map(|v| ((v as f64 - lo) * inv).clamp(0.0, 255.0) as u8)
        .collect()
}

/// u16 nodata sentinel: only an exactly-representable integral value can ever
/// match a u16 sample under the generic path's `|x - nd| < 1e-9` test.
fn u16_nodata(nodata: Option<f64>) -> Option<u16> {
    nodata.and_then(|nd| {
        (nd.is_finite() && nd.fract() == 0.0 && (0.0..=f64::from(u16::MAX)).contains(&nd))
            .then_some(nd as u16)
    })
}

/// Returns Some(rgba) when the u16 3-band LUT path applies. Mirrors
/// [`try_u8_rgb_fastpath`]'s gating (U16 source, three selected bands, RGB
/// composite or auto_rgb, no formula/expression); unlike the u8 path it
/// accepts ANY rescale because the ranges are baked into the LUTs.
fn try_u16_rgb_lut_fastpath(
    image: &ImageData,
    style: &CompiledTileStyle,
    auto_rgb: bool,
    h: usize,
    w: usize,
) -> Option<Array3<u8>> {
    use crate::render::composite::CompositeKind;

    let src = match &image.pixels {
        PixelData::U16(a) => a,
        _ => return None,
    };
    if !style.formula.is_empty() || !style.expression.is_empty() {
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

    let luts = style.luts.rgb_u16.get_or_init(|| {
        let ranges = rgb_ranges(style);
        Arc::new([
            build_u16_rescale_lut(ranges[0]),
            build_u16_rescale_lut(ranges[1]),
            build_u16_rescale_lut(ranges[2]),
        ])
    });

    let nodata = style.nodata.or(image.nodata);
    let nd_u16 = u16_nodata(nodata);
    let mut rgba = Array3::<u8>::from_elem((h, w, 4), 0u8);
    let stride = h * w;
    let src_standard = src.as_standard_layout();
    let src_slice = src_standard
        .as_slice()
        .expect("standard-layout u16 PixelData must be contiguous");
    let r_band = &src_slice[bidx[0] * stride..][..stride];
    let g_band = &src_slice[bidx[1] * stride..][..stride];
    let b_band = &src_slice[bidx[2] * stride..][..stride];
    let mask_slice: Option<&[bool]> = image.mask.as_ref().and_then(|m| m.as_slice());
    let dst = rgba
        .as_slice_mut()
        .expect("freshly-allocated Array3 standard layout");
    let [lut_r, lut_g, lut_b] = &**luts;

    for i in 0..stride {
        let r_val = r_band[i];
        let g_val = g_band[i];
        let b_val = b_band[i];
        let masked = matches!(mask_slice, Some(m) if !m[i]);
        // Per-band nodata: invalid when ANY selected band reads the sentinel,
        // matching `compute_valid_mask` on the generic path.
        let nd_hit = matches!(nd_u16, Some(nd) if r_val == nd || g_val == nd || b_val == nd);
        let alpha: u8 = if masked || nd_hit { 0 } else { 255 };
        let base = i * 4;
        dst[base] = lut_r[r_val as usize];
        dst[base + 1] = lut_g[g_val as usize];
        dst[base + 2] = lut_b[b_val as usize];
        dst[base + 3] = alpha;
    }
    Some(rgba)
}

/// Single-band u16 Gradient path: one 65536-entry table fuses the
/// normalise-to-index step AND the 256-entry colormap, so each pixel becomes
/// a single indexed 4-byte copy. Returns None when the composite isn't a
/// Gradient (discrete/rgb fall back to the generic per-pixel path).
fn try_u16_single_gradient_lut(
    src: &Array3<u16>,
    bidx: &[usize],
    mask: Option<&Array2<bool>>,
    nodata: Option<f64>,
    style: &CompiledTileStyle,
) -> Option<Array3<u8>> {
    use crate::render::composite::CompositeKind;
    let &[b0] = bidx else { return None };
    if !matches!(style.composite.kind, CompositeKind::Gradient) {
        return None;
    }
    let comp = &style.composite;
    let lut = style.luts.grad_u16.get_or_init(|| {
        let cmap = comp.lut.unwrap_or([[0u8; 4]; 256]);
        let (lo, hi) = match (&comp.vmin, &comp.vmax) {
            (Some(a), Some(b)) => (a[0], b[0]),
            _ => (0.0, 1.0),
        };
        let scale = 255.0 / (hi - lo).max(1e-12);
        Arc::new(
            (0..=u16::MAX as u32)
                .map(|v| cmap[(((v as f64 - lo) * scale).clamp(0.0, 255.0)) as u8 as usize])
                .collect(),
        )
    });

    let h = src.shape()[1];
    let w = src.shape()[2];
    let band = src.index_axis(Axis(0), b0);
    let valid = compute_valid_mask(mask, &[band], nodata, h, w);

    let mut rgba = Array3::<u8>::zeros((h, w, 4));
    {
        let band_standard = band.as_standard_layout();
        let band_slice = band_standard
            .as_slice()
            .expect("standard-layout u16 band must be contiguous");
        let dst = rgba
            .as_slice_mut()
            .expect("freshly-allocated Array3 standard layout");
        for (i, &v) in band_slice.iter().enumerate() {
            dst[i * 4..i * 4 + 4].copy_from_slice(&lut[v as usize]);
        }
    }
    apply_valid_to_alpha(&mut rgba, valid.as_ref());
    Some(rgba)
}

/// Build a `(h, w)` validity mask. Returns `None` if every pixel is valid so
/// the caller can skip the mask check entirely.
///
/// Implemented as a sequence of `Zip` reductions over the input `Array2`s —
/// pure ndarray broadcasting, no manual indexing.
fn compute_valid_mask<T: RenderSample>(
    mask: Option<&Array2<bool>>,
    bands: &[ArrayView2<'_, T>],
    nodata: Option<f64>,
    h: usize,
    w: usize,
) -> Option<Array2<bool>> {
    let mask_present = mask.is_some();
    if !mask_present && nodata.is_none() {
        return None;
    }
    let mut valid = Array2::<bool>::from_elem((h, w), true);
    if let Some(m) = mask {
        valid.assign(m);
    }
    if let Some(nd) = nodata {
        let is_nan = nd.is_nan();
        for band in bands {
            if is_nan {
                Zip::from(&mut valid).and(band).for_each(|v, &x| {
                    if x.to_f64().is_nan() {
                        *v = false;
                    }
                });
            } else {
                Zip::from(&mut valid).and(band).for_each(|v, &x| {
                    if (x.to_f64() - nd).abs() < 1e-9 {
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
fn render_single_band<T: RenderSample>(
    band: &ArrayView2<'_, T>,
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
            let idx = band.mapv(|v| (((v.to_f64() - lo) * scale).clamp(0.0, 255.0)) as u8);

            let mut rgba = Array3::<u8>::zeros((h, w, 4));
            interleave_lut(&idx, &lut, &mut rgba);
            apply_valid_to_alpha(&mut rgba, valid);
            rgba
        }
        _ => {
            // Discrete / single-band-rgb fallback: per-pixel composite.get.
            let mut rgba = Array3::<u8>::zeros((h, w, 4));
            Zip::indexed(band).for_each(|(r, c), &v| {
                let px = comp.get(&[v.to_f64()], None);
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
fn render_three_band<T: RenderSample>(
    r_band: &ArrayView2<'_, T>,
    g_band: &ArrayView2<'_, T>,
    b_band: &ArrayView2<'_, T>,
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
            let r = normalize(rv.to_f64(), ranges[0]);
            let g = normalize(gv.to_f64(), ranges[1]);
            let b = normalize(bv.to_f64(), ranges[2]);
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
            let r_u8 = ((r_slice[i].to_f64() - lo_r) * inv_r).clamp(0.0, 255.0) as u8;
            let g_u8 = ((g_slice[i].to_f64() - lo_g) * inv_g).clamp(0.0, 255.0) as u8;
            let b_u8 = ((b_slice[i].to_f64() - lo_b) * inv_b).clamp(0.0, 255.0) as u8;
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
        let px = style
            .composite
            .get(&[rv.to_f64(), gv.to_f64(), bv.to_f64()], None);
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
fn eval_expression_band(
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
    let mut stack: Vec<f64> = Vec::with_capacity(rpn.len().max(8));

    // Unique referenced band indices (0-based), computed once.
    let refs: Vec<usize> = {
        let mut v: Vec<usize> = rpn
            .iter()
            .filter_map(|t| match t {
                crate::render::expression::Token::Band(i) => Some(*i),
                _ => None,
            })
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };

    for row in 0..h {
        for col in 0..w {
            // Gather all band values for this pixel.
            for b in 0..bands {
                pixel_bands[b] = src[[b, row, col]];
            }

            let ok = !crate::render::expression::referenced_nodata_hit(&pixel_bands, &refs, nodata);

            let value = eval_rpn_with(rpn, &pixel_bands, &mut stack);
            result[[row, col]] = value;
            valid[[row, col]] = ok && value.is_finite();
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
    fn eval_expression_band_ndvi_and_masks_nonfinite() {
        use crate::render::expression::parse_expression;
        // 2 bands, 1x2. NDVI=(b2-b1)/(b2+b1). pixel0: b1=0.1,b2=0.5→0.6667 valid.
        // pixel1: b1=0,b2=0 → 0/0 NaN → invalid.
        let src = ndarray::Array3::from_shape_vec((2, 1, 2), vec![0.1, 0.0, 0.5, 0.0]).unwrap();
        let rpn = parse_expression("(b2-b1)/(b2+b1)").unwrap();
        let (band, valid) = eval_expression_band(&src.view(), &rpn, None, 1, 2).unwrap();
        assert!((band[[0, 0]] - 0.6666667).abs() < 1e-5);
        assert!(valid[[0, 0]]);
        assert!(!valid[[0, 1]], "0/0 must be masked invalid");
    }

    #[test]
    fn eval_expression_band_rejects_out_of_range_band() {
        use crate::render::expression::parse_expression;
        let src = ndarray::Array3::from_shape_vec((1, 1, 1), vec![1.0]).unwrap();
        let rpn = parse_expression("b5").unwrap();
        assert!(eval_expression_band(&src.view(), &rpn, None, 1, 1).is_err());
    }

    #[test]
    fn eval_expression_band_masks_nodata_in_referenced_band() {
        use crate::render::expression::parse_expression;
        let src = ndarray::Array3::from_shape_vec((2, 1, 2), vec![-9999.0, 0.2, 0.4, 0.6]).unwrap();
        let rpn = parse_expression("b1+b2").unwrap();
        let (_band, valid) = eval_expression_band(&src.view(), &rpn, Some(-9999.0), 1, 2).unwrap();
        assert!(!valid[[0, 0]], "nodata in referenced band → invalid");
        assert!(valid[[0, 1]]);
    }

    #[test]
    fn eval_expression_band_unreferenced_nodata_does_not_invalidate() {
        use crate::render::expression::parse_expression;
        // 2 bands; expression uses only b1; b2 has the nodata sentinel → pixel stays valid.
        let src = ndarray::Array3::from_shape_vec((2, 1, 1), vec![0.5, -9999.0]).unwrap();
        let rpn = parse_expression("b1").unwrap();
        let (_band, valid) = eval_expression_band(&src.view(), &rpn, Some(-9999.0), 1, 1).unwrap();
        assert!(
            valid[[0, 0]],
            "unreferenced-band nodata must not invalidate"
        );
    }

    #[test]
    fn eval_expression_band_nan_nodata_masks_referenced() {
        use crate::render::expression::parse_expression;
        // 1 band, 1x2; NaN nodata. pixel0 = NaN (referenced) → invalid; pixel1 = 0.5 → valid.
        let src = ndarray::Array3::from_shape_vec((1, 1, 2), vec![f64::NAN, 0.5]).unwrap();
        let rpn = parse_expression("b1").unwrap();
        let (_band, valid) = eval_expression_band(&src.view(), &rpn, Some(f64::NAN), 1, 2).unwrap();
        assert!(!valid[[0, 0]], "NaN nodata in referenced band → invalid");
        assert!(valid[[0, 1]]);
    }

    #[test]
    fn expression_marks_nodata_and_division_by_zero_invalid() {
        use crate::render::expression::parse_expression;
        use ndarray::Array3;

        let pixels =
            Array3::from_shape_vec((2, 1, 3), vec![3.0, -9999.0, 1.0, 1.0, 2.0, -1.0]).unwrap();
        let expression = parse_expression("(b1-b2)/(b1+b2)").unwrap();

        let (values, valid) =
            eval_expression_band(&pixels.view(), &expression, Some(-9999.0), 1, 3).unwrap();

        assert!((values[[0, 0]] - 0.5).abs() < 1e-12);
        assert!(valid[[0, 0]]);
        assert!(!valid[[0, 1]]);
        assert!(!valid[[0, 2]]);
    }

    #[test]
    fn expression_rejects_missing_source_band() {
        use crate::render::expression::parse_expression;
        use ndarray::Array3;

        let pixels = Array3::from_elem((1, 1, 1), 1.0);
        let expression = parse_expression("b2").unwrap();
        let err = eval_expression_band(&pixels.view(), &expression, None, 1, 1).unwrap_err();

        assert!(matches!(err, AgError::Query { .. }));
    }

    #[test]
    fn expression_render_encodes_invalid_pixels_transparent() {
        use crate::pixel::{ImageData, PixelData};
        use crate::style::params::compile;
        use crate::style::TileStyle;
        use ndarray::Array3;

        let source = ImageData {
            pixels: PixelData::F64(
                Array3::from_shape_vec(
                    (2, 1, 4),
                    vec![3.0, -9999.0, 1.0, 4.0, 1.0, 2.0, -1.0, 1.0],
                )
                .unwrap(),
            ),
            mask: Some(
                ndarray::Array2::from_shape_vec((1, 4), vec![false, true, true, true]).unwrap(),
            ),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 0.0, 0.0, -1.0]),
            nodata: Some(-9999.0),
            bbox: Bbox::new(0.0, 0.0, 4.0, 1.0),
        };
        let style = TileStyle {
            expression: Some("(b1-b2)/(b1+b2)".into()),
            rescale: vec![(-1.0, 1.0)],
            ..TileStyle::default()
        };

        let png = render(&source, &compile(&style).unwrap()).unwrap();
        let rgba = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0)[3], 0);
        assert_eq!(rgba.get_pixel(1, 0)[3], 0);
        assert_eq!(rgba.get_pixel(2, 0)[3], 0);
        assert_eq!(rgba.get_pixel(3, 0)[3], 255);
    }

    #[test]
    fn expression_render_masks_infinite_nodata_transparent() {
        use crate::pixel::{ImageData, PixelData};
        use crate::style::params::compile;
        use crate::style::TileStyle;
        use ndarray::Array3;

        let source = ImageData {
            pixels: PixelData::F64(Array3::from_elem((1, 1, 1), f64::INFINITY)),
            mask: None,
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 1.0, 0.0, -1.0]),
            nodata: Some(f64::INFINITY),
            bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
        };
        let style = TileStyle {
            expression: Some("0/b1".into()),
            rescale: vec![(0.0, 1.0)],
            ..TileStyle::default()
        };

        let png = render(&source, &compile(&style).unwrap()).unwrap();
        let rgba = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0)[3], 0);
    }

    /// The u16 LUT fast paths must be BIT-IDENTICAL to the generic typed path.
    /// We render the same pixel values once as U16 (LUT path) and once as F64
    /// (generic path) and require equal encoded bytes.
    #[test]
    fn u16_rgb_lut_matches_generic_path() {
        let vals: Vec<u16> = vec![
            0, 100, 5000, 65535, // R
            1, 200, 30000, 4242, // G
            7, 999, 60000, 12345, // B
        ];
        let arr_u16 = Array3::from_shape_vec((3, 2, 2), vals.clone()).unwrap();
        let arr_f64 =
            Array3::from_shape_vec((3, 2, 2), vals.iter().map(|&v| v as f64).collect()).unwrap();
        let mk_img = |pixels: PixelData| ImageData {
            pixels,
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 2.0, 2.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 2.0, 0.0, -1.0]),
            nodata: Some(100.0), // hits one R sample → that pixel transparent
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Rgb([0.0, 0.0, 0.0], [255.0, 255.0, 255.0]),
            None,
            None,
        )
        .unwrap();
        let style = CompiledTileStyle {
            composite: comp,
            rescale: vec![(0.0, 10000.0), (50.0, 30000.0), (0.0, 65535.0)],
            formula: vec![],
            expression: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 2,
            resampling: Resampling::Nearest,
            auto_rgb: true,
            format: TileFormat::Png,
            luts: Default::default(),
        };
        let via_lut = render(&mk_img(PixelData::U16(arr_u16)), &style).unwrap();
        let generic = render(&mk_img(PixelData::F64(arr_f64)), &style).unwrap();
        assert_eq!(via_lut, generic, "u16 LUT RGB path must match generic path");
    }

    #[test]
    fn u16_single_band_gradient_lut_matches_generic_path() {
        let vals: Vec<u16> = vec![0, 1000, 32768, 65535, 500, 9999];
        let arr_u16 = Array3::from_shape_vec((1, 2, 3), vals.clone()).unwrap();
        let arr_f64 =
            Array3::from_shape_vec((1, 2, 3), vals.iter().map(|&v| v as f64).collect()).unwrap();
        let mk_img = |pixels: PixelData| ImageData {
            pixels,
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 3.0, 2.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 2.0, 0.0, -1.0]),
            nodata: Some(500.0),
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Named("viridis".into()),
            Some(vec![0.0]),
            Some(vec![40000.0]),
        )
        .unwrap();
        let style = CompiledTileStyle {
            composite: comp,
            rescale: vec![],
            formula: vec![],
            expression: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 2,
            resampling: Resampling::Nearest,
            auto_rgb: false,
            format: TileFormat::Png,
            luts: Default::default(),
        };
        let via_lut = render(&mk_img(PixelData::U16(arr_u16)), &style).unwrap();
        let generic = render(&mk_img(PixelData::F64(arr_f64)), &style).unwrap();
        assert_eq!(
            via_lut, generic,
            "u16 gradient LUT path must match generic path"
        );
    }

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
            format: TileFormat::Png,
            luts: Default::default(),
        };
        let bytes = render(&img, &style).unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));
    }

    #[cfg(feature = "webp")]
    #[test]
    fn render_honors_webp_format() {
        let arr = Array3::<u8>::from_elem((3, 8, 8), 200u8);
        let img = ImageData {
            pixels: PixelData::U8(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 8.0, 8.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 8.0, 0.0, -1.0]),
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
            rescale: vec![],
            formula: vec![],
            expression: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 8,
            resampling: Resampling::Nearest,
            auto_rgb: false,
            format: TileFormat::Webp,
            luts: Default::default(),
        };
        let bytes = render(&img, &style).unwrap();
        assert!(bytes.starts_with(b"RIFF"));
        assert_eq!(&bytes[8..12], b"WEBP");
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
            format: TileFormat::Png,
            luts: Default::default(),
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
            format: TileFormat::Png,
            luts: Default::default(),
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
            format: TileFormat::Png,
            luts: Default::default(),
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
            format: TileFormat::Png,
            luts: Default::default(),
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
            format: TileFormat::Png,
            luts: Default::default(),
        };

        let bytes = render(&img, &style).unwrap();
        let rgba = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0, [120, 80, 40, 255]);
    }
}
