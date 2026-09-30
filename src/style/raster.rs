//! Raster Style 0.3.0 input and the explicitly supported rendering subset.
use std::sync::Arc;

use bytes::Bytes;
use ndarray::{Array2, Array3, Axis};

use super::{
    stretch::{Statistics, Stretch, Transfer},
    TileFormat,
};
use crate::error::{AgError, AgResult};
use crate::pixel::{ImageData, PixelData};
use crate::render::{encode::encode, expression};
use crate::warp::resampler::Resampling;

#[derive(Debug, Clone)]
pub(crate) struct RasterPlan {
    pub(crate) key: String,
    pub(crate) categorical: bool,
    pub(crate) mosaic: Option<RasterMosaic>,
    inputs: Vec<Vec<expression::Token>>,
    dependencies: Vec<usize>,
    pub(crate) stretch: Stretch,
    pub(crate) statistics: Statistics,
    transparent: bool,
    colors: Colors,
    gamma: Vec<(String, f64)>,
    opacity: f64,
    pub(crate) statistics_key: String,
    pub(crate) stretch_key: String,
    rgb_luts: Arc<RgbLuts>,
}

#[derive(Debug, Default)]
struct RgbLuts {
    u8: std::sync::OnceLock<[Vec<u8>; 3]>,
    u16: std::sync::OnceLock<[Vec<u8>; 3]>,
}

pub(crate) struct StatisticsResult {
    stretch: Stretch,
    rgb_luts: Arc<RgbLuts>,
}

#[derive(Debug, Clone)]
pub(crate) struct RasterMosaic {
    #[cfg(feature = "mosaic")]
    selection: String,
    pub(crate) after_channels: bool,
    #[cfg(feature = "mosaic")]
    pub(crate) rank: usize,
}

#[cfg(feature = "mosaic")]
impl RasterMosaic {
    pub(crate) fn selection(&self) -> crate::mosaic::PixelSelection {
        use crate::mosaic::PixelSelection;
        match self.selection.as_str() {
            "first" => PixelSelection::First,
            "highest" => PixelSelection::Highest,
            "lowest" => PixelSelection::Lowest,
            "mean" => PixelSelection::Mean,
            "median" => PixelSelection::Median,
            _ => unreachable!("SDK-validated pixel_selection"),
        }
    }
}

#[derive(Debug, Clone)]
enum Colors {
    Rgb,
    Gray(bool),
    Named(Box<[[u8; 4]; 256]>),
    Exact(Vec<(f64, [u8; 4])>, bool),
    Intervals(Vec<(f64, f64, [u8; 4])>, bool),
    Solid([u8; 4]),
}

mod compiler;

#[inline]
fn byte(value: f64) -> u8 {
    let t = 255.0 * value.clamp(0.0, 1.0);
    let half = t.floor() + 0.5;
    let t = if (t - half).abs() <= 1e-10 { half } else { t };
    (t + 0.5).floor() as u8
}

pub(crate) fn normalize(x: f64, lo: f64, hi: f64) -> f64 {
    if x <= lo {
        return 0.0;
    }
    if x >= hi {
        return 1.0;
    }
    if (hi - lo).is_finite() {
        (x - lo) / (hi - lo)
    } else {
        (x / 2.0 - lo / 2.0) / (hi / 2.0 - lo / 2.0)
    }
}

/// Multiplication suffices away from byte boundaries. Near a half step use
/// the original division and quantizer, including its 1e-10 tie tolerance.
#[inline]
fn range_byte(value: f64, (lo, hi, span, scale): (f64, f64, f64, f64)) -> u8 {
    if lo == hi && value == lo {
        return 128;
    }
    if value <= lo {
        return 0;
    }
    if value >= hi {
        return 255;
    }
    if !span.is_finite() || !scale.is_finite() {
        return byte(normalize(value, lo, hi));
    }
    let scaled = (value - lo) * scale;
    // Interior values are nonnegative; the cast already truncates, so an
    // explicit floor would repeat the rounding work.
    let rounded = (scaled + 0.5) as u8;
    // Even a subnormal scale has <1e-12 byte error on an interior
    // sample. The margin also encloses the quantizer's snapping tolerance.
    if ((scaled - f64::from(rounded)).abs() - 0.5).abs() < 1e-8 {
        byte((value - lo) / span)
    } else {
        rounded
    }
}

impl RasterPlan {
    pub(crate) fn mosaic_band_count_for_statistics(&self) -> usize {
        self.dependencies.len().max(self.inputs.len()).max(1)
    }
    /// Warp dependencies independently so unrelated bands and nodata sentinels
    /// cannot change another band's interpolation. NoData is consumed here.
    pub(crate) fn warp_dependencies(
        &self,
        src: &ImageData,
        dst_crs: &crate::crs::Crs,
        dst_bbox: crate::bounds::Bbox,
        size: usize,
        resampling: Resampling,
    ) -> AgResult<ImageData> {
        if self.dependencies.iter().any(|&b| b >= src.pixels.bands()) {
            return Err(AgError::query(
                "/renderer",
                "referenced source band does not exist",
            ));
        }
        if !self.dependencies.is_empty() {
            // Native nearest-neighbor values stay exact. Select first so unused
            // bands cannot affect validity; the other kernels retain F64 precision.
            if src.crs == *dst_crs && src.nodata.is_none() && resampling == Resampling::Nearest {
                if matches!(src.pixels, PixelData::U8(_) | PixelData::U16(_))
                    && self.dependencies.iter().copied().eq(0..src.pixels.bands())
                {
                    return crate::warp::warp(src, dst_crs, dst_bbox, size, size, resampling);
                }
                let pixels = match &src.pixels {
                    PixelData::U8(a) => Some(PixelData::U8(a.select(Axis(0), &self.dependencies))),
                    PixelData::U16(a) => {
                        Some(PixelData::U16(a.select(Axis(0), &self.dependencies)))
                    }
                    _ => None,
                };
                if let Some(pixels) = pixels {
                    let selected = ImageData {
                        pixels,
                        mask: src.mask.clone(),
                        nodata: None,
                        crs: src.crs.clone(),
                        bbox: src.bbox,
                        transform: src.transform,
                    };
                    return crate::warp::warp(&selected, dst_crs, dst_bbox, size, size, resampling);
                }
            }
            return crate::warp::reproject::warp_selected(
                src,
                &self.dependencies,
                dst_crs,
                dst_bbox,
                size,
                size,
                resampling,
            );
        }
        // A constant expression still needs the source coverage mask.
        let coverage = ImageData {
            pixels: PixelData::F64(Array3::zeros((1, src.pixels.height(), src.pixels.width()))),
            mask: src.mask.clone(),
            nodata: None,
            crs: src.crs.clone(),
            bbox: src.bbox,
            transform: src.transform,
        };
        crate::warp::warp(&coverage, dst_crs, dst_bbox, size, size, resampling)
    }

    pub(crate) fn compact(&self) -> Self {
        let mut bound = self.clone();
        for token in bound.inputs.iter_mut().flatten() {
            if let expression::Token::Band(b) = token {
                *b = self
                    .dependencies
                    .binary_search(b)
                    .expect("collected dependency");
            }
        }
        bound.dependencies = (0..self.dependencies.len()).collect();
        bound
    }

    pub(crate) fn render_warped(
        &self,
        src: &ImageData,
        dst_crs: &crate::crs::Crs,
        dst_bbox: crate::bounds::Bbox,
        size: usize,
        resampling: Resampling,
        format: TileFormat,
    ) -> AgResult<Bytes> {
        let image = self.warp_dependencies(src, dst_crs, dst_bbox, size, resampling)?;
        self.compact().render(&image, None, format)
    }

    pub(crate) fn mosaic_band_count(&self) -> usize {
        if self.mosaic.as_ref().is_some_and(|m| m.after_channels) {
            self.inputs.len()
        } else {
            self.dependencies.len().max(1)
        }
    }

    /// Input is a compact, independently warped dependency tuple with NoData consumed.
    pub(crate) fn prepare_mosaic(&self, mut image: ImageData) -> ImageData {
        if !self.mosaic.as_ref().is_some_and(|m| m.after_channels) {
            return image;
        }
        let bound = self.compact();
        if bound.inputs.len() == image.pixels.bands()
            && bound.inputs.iter().enumerate().all(
                |(i, tokens)| matches!(tokens.as_slice(), [expression::Token::Band(b)] if *b == i),
            )
        {
            return image;
        }
        let src = image.pixels.as_f64();
        let (h, w) = (image.pixels.height(), image.pixels.width());
        let mut pixels = Array3::zeros((bound.inputs.len(), h, w));
        let mut mask = image
            .mask
            .take()
            .unwrap_or_else(|| Array2::from_elem((h, w), true));
        let mut samples = vec![0.; src.shape()[0]];
        let mut stack = Vec::new();
        for ((r, c), valid) in mask.indexed_iter_mut() {
            if !*valid {
                continue;
            }
            for (b, sample) in samples.iter_mut().enumerate() {
                *sample = src[[b, r, c]];
            }
            for (b, tokens) in bound.inputs.iter().enumerate() {
                let value = expression::eval_rpn_with(tokens, &samples, &mut stack);
                pixels[[b, r, c]] = value;
                *valid &= value.is_finite();
            }
        }
        drop(src);
        image.pixels = PixelData::F64(pixels);
        image.mask = Some(mask);
        image.nodata = None;
        image
    }

    #[cfg(feature = "mosaic")]
    pub(crate) fn render_mosaic(&self, image: &ImageData, format: TileFormat) -> AgResult<Bytes> {
        let mut bound = self.compact();
        if self.mosaic.as_ref().is_some_and(|m| m.after_channels) {
            bound.inputs = (0..self.inputs.len())
                .map(|b| vec![expression::Token::Band(b)])
                .collect();
            bound.dependencies = (0..self.inputs.len()).collect();
        }
        bound.render(image, None, format)
    }

    pub(crate) fn statistics_samples(
        &self,
        image: &ImageData,
        channels_ready: bool,
    ) -> Vec<Vec<f64>> {
        let mut bound = self.compact();
        if channels_ready {
            bound.inputs = (0..self.inputs.len())
                .map(|b| vec![expression::Token::Band(b)])
                .collect();
            bound.dependencies = (0..self.inputs.len()).collect();
        }
        let n = image.pixels.width() * image.pixels.height();
        let k = self.statistics.sample_size.unwrap_or(n).min(n);
        let bands: Option<Vec<usize>> = bound
            .inputs
            .iter()
            .map(|tokens| match tokens.as_slice() {
                [expression::Token::Band(b)] => Some(*b),
                _ => None,
            })
            .collect();
        if let Some(bands) = bands {
            let direct = match &image.pixels {
                PixelData::U8(a) => sample_bands(a, image.mask.as_ref(), &bands, k),
                PixelData::U16(a) => sample_bands(a, image.mask.as_ref(), &bands, k),
                PixelData::I16(a) => sample_bands(a, image.mask.as_ref(), &bands, k),
                PixelData::U32(a) => sample_bands(a, image.mask.as_ref(), &bands, k),
                PixelData::I32(a) => sample_bands(a, image.mask.as_ref(), &bands, k),
                PixelData::F32(a) => sample_bands(a, image.mask.as_ref(), &bands, k),
                PixelData::F64(a) => sample_bands(a, image.mask.as_ref(), &bands, k),
            };
            if let Some(values) = direct {
                return values;
            }
        }
        let src = image.pixels.as_f64();
        let mut values: Vec<Vec<f64>> = (0..self.inputs.len())
            .map(|_| Vec::with_capacity(k))
            .collect();
        let mut samples = vec![0.0; image.pixels.bands()];
        let mut tuple = vec![0.0; self.inputs.len()];
        let mut stack = Vec::new();
        for i in 0..k {
            let index = if k == 1 {
                (n - 1) / 2
            } else {
                ((i as u64) * (n as u64 - 1) / (k as u64 - 1)) as usize
            };
            let (r, c) = (index / image.pixels.width(), index % image.pixels.width());
            if image.mask.as_ref().is_some_and(|m| !m[[r, c]]) {
                continue;
            }
            for &b in &bound.dependencies {
                samples[b] = src[[b, r, c]];
            }
            if bound.dependencies.iter().any(|&b| !samples[b].is_finite()) {
                continue;
            }
            for (v, tokens) in tuple.iter_mut().zip(&bound.inputs) {
                *v = match tokens.as_slice() {
                    [expression::Token::Band(b)] => samples[*b],
                    _ => expression::eval_rpn_with(tokens, &samples, &mut stack),
                };
            }
            // RGB statistics consume only complete valid tuples.
            if tuple.iter().any(|v| !v.is_finite()) {
                continue;
            }
            for (channel, &v) in values.iter_mut().zip(&tuple) {
                channel.push(v);
            }
        }
        values
    }

    pub(crate) fn resolve_samples(&self, values: &[Vec<f64>]) -> AgResult<StatisticsResult> {
        use rayon::prelude::*;
        Ok(StatisticsResult {
            stretch: Stretch::Resolved(
                values
                    .par_iter()
                    .map(|v| self.stretch.resolve(v.as_slice()))
                    .collect::<AgResult<Vec<Transfer>>>()?,
            ),
            rgb_luts: Arc::default(),
        })
    }

    pub(crate) fn with_statistics(&self, result: &StatisticsResult) -> Self {
        let mut resolved = self.clone();
        resolved.stretch = result.stretch.clone();
        resolved.rgb_luts = result.rgb_luts.clone();
        resolved
    }

    fn render_rgb_fast(
        &self,
        image: &ImageData,
        nodata: Option<f64>,
        format: TileFormat,
    ) -> AgResult<Option<Bytes>> {
        if !matches!(self.colors, Colors::Rgb) || !self.gamma.is_empty() {
            return Ok(None);
        }
        let transfers = self.stretch.transfers()?;
        if self.inputs.len() != 3 || !transfers.iter().all(|t| matches!(t, Transfer::Range(..))) {
            return Ok(None);
        }
        let bands: [Option<usize>; 3] = std::array::from_fn(|i| match self.inputs[i].as_slice() {
            [expression::Token::Band(b)] => Some(*b),
            _ => None,
        });
        let [Some(r), Some(g), Some(b)] = bands else {
            return Ok(None);
        };
        let bands = [r, g, b];
        let lut = |size| {
            std::array::from_fn(|b| {
                (0..size)
                    .map(|v| byte(transfers[b].apply(v as f64, false).expect("range transfer")))
                    .collect()
            })
        };
        let rgba = match &image.pixels {
            PixelData::U8(a) => self.rgb_array(
                a,
                image.mask.as_ref(),
                nodata,
                &bands,
                Some(self.rgb_luts.u8.get_or_init(|| lut(256))),
            ),
            PixelData::U16(a) => self.rgb_array(
                a,
                image.mask.as_ref(),
                nodata,
                &bands,
                Some(self.rgb_luts.u16.get_or_init(|| lut(65536))),
            ),
            PixelData::I16(a) => self.rgb_array(a, image.mask.as_ref(), nodata, &bands, None),
            PixelData::U32(a) => self.rgb_array(a, image.mask.as_ref(), nodata, &bands, None),
            PixelData::I32(a) => self.rgb_array(a, image.mask.as_ref(), nodata, &bands, None),
            PixelData::F32(a) => self.rgb_array(a, image.mask.as_ref(), nodata, &bands, None),
            PixelData::F64(a) => self.rgb_array(a, image.mask.as_ref(), nodata, &bands, None),
        };
        rgba.map(|rgba| encode(&rgba, format)).transpose()
    }

    fn rgb_array<T: Copy + Into<f64> + Sync>(
        &self,
        src: &Array3<T>,
        mask: Option<&Array2<bool>>,
        nodata: Option<f64>,
        bands: &[usize],
        lut: Option<&[Vec<u8>; 3]>,
    ) -> Option<Array3<u8>> {
        let views: [_; 3] = std::array::from_fn(|i| src.index_axis(Axis(0), bands[i]));
        let channels: [&[T]; 3] = [
            views[0].as_slice()?,
            views[1].as_slice()?,
            views[2].as_slice()?,
        ];
        let mask = match mask {
            Some(m) => Some(m.as_slice()?),
            None => None,
        };
        let transfers = self.stretch.transfers().ok()?;
        let ranges: [_; 3] = std::array::from_fn(|i| match transfers[i] {
            Transfer::Range(lo, hi) => (lo, hi, hi - lo, 255.0 / (hi - lo)),
            _ => unreachable!("RGB range fast path"),
        });
        let byte_ranges = ranges
            .iter()
            .all(|&(lo, hi, _, _)| lo == 0.0 && hi == 255.0);
        let mut rgba = Array3::zeros((src.shape()[1], src.shape()[2], 4));
        let alpha = byte(self.opacity);
        if alpha == 0 {
            return Some(rgba);
        }
        const CHUNK_PIXELS: usize = 16 * 1024;
        let render_chunk = |(chunk, out): (usize, &mut [u8])| {
            let start = chunk * CHUNK_PIXELS;
            let end = start + out.len() / 4;
            let [r, g, b] = channels.map(|band| &band[start..end]);
            let pixels = r
                .iter()
                .zip(g)
                .zip(b)
                .zip(out.chunks_exact_mut(4))
                .enumerate();
            // A LUT is supplied only for finite native U8/U16 samples.
            if let Some(lut) = lut.filter(|_| nodata.is_none() && !self.transparent) {
                for (offset, (((&r, &g), &b), pixel)) in pixels {
                    if mask.is_some_and(|m| !m[start + offset]) {
                        continue;
                    }
                    let rgb = if byte_ranges {
                        [r.into() as u8, g.into() as u8, b.into() as u8]
                    } else {
                        [
                            lut[0][r.into() as usize],
                            lut[1][g.into() as usize],
                            lut[2][b.into() as usize],
                        ]
                    };
                    pixel[..3].copy_from_slice(&rgb);
                    pixel[3] = alpha;
                }
                return;
            }
            for (offset, (((&r, &g), &b), pixel)) in pixels {
                let i = start + offset;
                if mask.is_some_and(|m| !m[i]) {
                    continue;
                }
                let values = [r.into(), g.into(), b.into()];
                if values.iter().any(|&v| !v.is_finite() || nodata == Some(v)) {
                    continue;
                }
                if self.transparent
                    && values
                        .iter()
                        .zip(ranges)
                        .any(|(&v, (lo, hi, _, _))| v < lo || v > hi)
                {
                    continue;
                }
                for b in 0..3 {
                    pixel[b] = if byte_ranges && lut.is_some() {
                        // Integer samples with a 0..255 range need no table lookup.
                        values[b] as u8
                    } else if let Some(lut) = lut {
                        lut[b][values[b] as usize]
                    } else if byte_ranges {
                        range_byte(values[b], (0.0, 255.0, 255.0, 1.0))
                    } else {
                        range_byte(values[b], ranges[b])
                    };
                }
                pixel[3] = alpha;
            }
        };
        let out = rgba.as_slice_mut()?;
        // Small tiles and LUT lookups do not amortize thread scheduling.
        if lut.is_none() && out.len() >= 256 * 256 * 4 {
            use rayon::prelude::*;
            out.par_chunks_mut(CHUNK_PIXELS * 4)
                .enumerate()
                .for_each(render_chunk);
        } else {
            out.chunks_mut(CHUNK_PIXELS * 4)
                .enumerate()
                .for_each(render_chunk);
        }
        Some(rgba)
    }

    #[cfg(test)]
    pub(crate) fn resolve_statistics(
        &self,
        image: &ImageData,
        channels_ready: bool,
    ) -> AgResult<Self> {
        let values = self.statistics_samples(image, channels_ready);
        Ok(self.with_statistics(&self.resolve_samples(&values)?))
    }

    pub(crate) fn render(
        &self,
        image: &ImageData,
        nodata: Option<f64>,
        format: TileFormat,
    ) -> AgResult<Bytes> {
        let (h, w) = (image.pixels.height(), image.pixels.width());
        if image.pixels.bands() == 0
            || h == 0
            || w == 0
            || image.mask.as_ref().is_some_and(|mask| mask.dim() != (h, w))
        {
            return Err(AgError::query("image", "invalid pixel or mask dimensions"));
        }
        let transfers = self.stretch.transfers()?;
        let bands = image.pixels.bands();
        if self.dependencies.iter().any(|&b| b >= bands) {
            return Err(AgError::query(
                "/renderer",
                "referenced source band does not exist",
            ));
        }
        if let Some(bytes) = self.render_rgb_fast(image, nodata, format)? {
            return Ok(bytes);
        }
        let src = image.pixels.as_f64();
        let mut rgba = Array3::zeros((h, w, 4));
        let mut samples = vec![0.0; bands];
        let mut stack = Vec::new();
        let mut values = vec![0.0; self.inputs.len()];
        for r in 0..h {
            for c in 0..w {
                if image.mask.as_ref().is_some_and(|m| !m[[r, c]]) {
                    continue;
                }
                for &b in &self.dependencies {
                    samples[b] = src[[b, r, c]];
                }
                if self
                    .dependencies
                    .iter()
                    .any(|&b| !samples[b].is_finite() || nodata == Some(samples[b]))
                {
                    continue;
                }
                for (value, tokens) in values.iter_mut().zip(&self.inputs) {
                    *value = expression::eval_rpn_with(tokens, &samples, &mut stack);
                }
                if values.iter().any(|x| !x.is_finite()) {
                    continue;
                }
                let data = values[0];
                let display = !matches!(
                    self.colors,
                    Colors::Exact(_, true) | Colors::Intervals(_, true) | Colors::Solid(_)
                );
                if display {
                    let mut valid = true;
                    for (x, transfer) in values.iter_mut().zip(transfers) {
                        match transfer.apply(*x, self.transparent) {
                            Some(value) => *x = value,
                            None => {
                                valid = false;
                                break;
                            }
                        }
                    }
                    if !valid {
                        continue;
                    }
                    for (channels, gamma) in &self.gamma {
                        for (i, x) in values.iter_mut().enumerate() {
                            if channels.as_bytes().contains(&b"rgb"[i]) && *x > 0.0 && *x < 1.0 {
                                *x = (x.ln() / gamma).exp().clamp(0.0, 1.0);
                            }
                        }
                    }
                }
                let mut color = match &self.colors {
                    Colors::Rgb => [values[0], values[1], values[2], 1.0],
                    Colors::Gray(invert) => {
                        let g = if *invert { 1.0 - values[0] } else { values[0] };
                        [g, g, g, 1.0]
                    }
                    Colors::Named(lut) => {
                        lut[byte(values[0]) as usize].map(|x| f64::from(x) / 255.0)
                    }
                    Colors::Solid(color) => color.map(|x| f64::from(x) / 255.0),
                    Colors::Exact(entries, raw) => {
                        let key = if *raw {
                            data
                        } else {
                            f64::from(byte(values[0]))
                        };
                        // Numeric ordering preserves exact matching, including -0 == 0.
                        let index = entries.partition_point(|(v, _)| *v < key);
                        entries
                            .get(index)
                            .filter(|(v, _)| *v == key)
                            .map(|(_, color)| *color)
                            .unwrap_or([0; 4])
                            .map(|x| f64::from(x) / 255.0)
                    }
                    Colors::Intervals(entries, raw) => {
                        let key = if *raw {
                            data
                        } else {
                            f64::from(byte(values[0]))
                        };
                        // SDK validation guarantees sorted, non-overlapping intervals.
                        let index = entries.partition_point(|(_, hi, _)| *hi <= key);
                        entries
                            .get(index)
                            .filter(|(lo, _, _)| key >= *lo)
                            .map(|(_, _, color)| *color)
                            .unwrap_or([0; 4])
                            .map(|x| f64::from(x) / 255.0)
                    }
                };
                color[3] *= self.opacity;
                let mut pixel = color.map(byte);
                if pixel[3] == 0 {
                    pixel = [0; 4];
                }
                for i in 0..4 {
                    rgba[[r, c, i]] = pixel[i];
                }
            }
        }
        encode(&rgba, format)
    }
}

/// Preserve deterministic sample positions and complete valid channel tuples
/// without promoting native arrays or evaluating direct band inputs as RPN.
fn sample_bands<T: Copy + Into<f64>>(
    src: &Array3<T>,
    mask: Option<&Array2<bool>>,
    bands: &[usize],
    k: usize,
) -> Option<Vec<Vec<f64>>> {
    let views: Vec<_> = bands.iter().map(|&b| src.index_axis(Axis(0), b)).collect();
    let channels: Vec<_> = views.iter().map(|v| v.as_slice()).collect::<Option<_>>()?;
    let mask = match mask {
        Some(m) => Some(m.as_slice()?),
        None => None,
    };
    let n = src.shape()[1] * src.shape()[2];
    let mut values: Vec<Vec<f64>> = (0..bands.len()).map(|_| Vec::with_capacity(k)).collect();
    let mut tuple = vec![0.; bands.len()];
    for i in 0..k {
        let index = if k == 1 {
            (n - 1) / 2
        } else {
            ((i as u64) * (n as u64 - 1) / (k as u64 - 1)) as usize
        };
        if mask.is_some_and(|m| !m[index]) {
            continue;
        }
        for (v, channel) in tuple.iter_mut().zip(&channels) {
            *v = channel[index].into();
        }
        if tuple.iter().any(|v| !v.is_finite()) {
            continue;
        }
        for (channel, &v) in values.iter_mut().zip(&tuple) {
            channel.push(v);
        }
    }
    Some(values)
}

#[cfg(test)]
#[path = "raster/tests.rs"]
mod tests;
