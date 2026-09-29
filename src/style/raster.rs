//! Raster Style 0.3.0 input and the explicitly supported rendering subset.
use std::sync::Arc;

use bytes::Bytes;
use ndarray::{Array2, Array3, Axis};
use serde_json::Value;

use super::{
    stretch::{Statistics, Stretch, Transfer},
    RasterStyle, TileFormat, TileStyle,
};
use crate::error::{AgError, AgResult};
use crate::pixel::{ImageData, PixelData};
use crate::render::{colormap::builtin_lut, encode::encode, expression};
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

fn unsupported(path: &str) -> AgError {
    AgError::query(path, "not supported by the Raster Style adapter")
}

fn sdk_error(error: raster_style::Error) -> AgError {
    AgError::query(
        if error.path.is_empty() {
            "style"
        } else {
            &error.path
        },
        error.to_string(),
    )
}

// The SDK has already checked types. This second boundary rejects capabilities
// we cannot execute instead of silently accepting fields from the larger spec.
fn only(value: &Value, allowed: &[&str], path: &str) -> AgResult<()> {
    if let Some(object) = value.as_object() {
        for key in object.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(unsupported(&format!("{path}/{key}")));
            }
        }
    }
    Ok(())
}

impl TileStyle {
    /// Parse grouped Raster Style JSON (SDK 0.3.0, protocol 2.0).
    /// Unsupported renderer capabilities fail before any raster is opened.
    pub fn from_json(json: &str) -> AgResult<Self> {
        Self::from_raster_style(&RasterStyle::from_json(json).map_err(sdk_error)?)
    }

    /// Parse an original, percent-encoded Q2 query. Do not URL-decode it first.
    pub fn from_raster_query(query: &str) -> AgResult<Self> {
        Self::from_raster_style(&raster_style::decode_query(query).map_err(sdk_error)?)
    }

    /// Adapt an SDK-validated document to the supported renderer.
    pub fn from_raster_style(style: &RasterStyle) -> AgResult<Self> {
        let v = style.as_value();
        only(
            v,
            &[
                "version",
                "renderer",
                "stretch",
                "resampling",
                "nodata",
                "effects",
                "opacity",
                "image",
                "calibration",
                "mosaic",
                "statistics",
            ],
            "",
        )?;
        let renderer = &v["renderer"];
        only(
            renderer,
            &[
                "type",
                "bidx",
                "expression",
                "language",
                "colormap_name",
                "colormap",
                "renderer_invert",
                "color",
            ],
            "/renderer",
        )?;
        if v.pointer("/calibration/mode")
            .is_some_and(|mode| mode != "none")
        {
            return Err(unsupported("/calibration"));
        }
        let kind = renderer["type"].as_str().unwrap_or_default();
        if !["gray", "rgb", "pseudocolor", "categorized", "single_color"].contains(&kind) {
            return Err(unsupported("/renderer/type"));
        }
        let inputs: Vec<Vec<expression::Token>> = if let Some(bands) = renderer["bidx"].as_array() {
            bands
                .iter()
                .map(|b| {
                    vec![expression::Token::Band(
                        b.as_f64().expect("validated band") as usize - 1,
                    )]
                })
                .collect()
        } else if let Some(text) = renderer["expression"].as_str() {
            text.split(';')
                .map(expression::parse_expression)
                .collect::<AgResult<_>>()?
        } else {
            return Err(unsupported("/renderer/index"));
        };
        let mut dependencies: Vec<_> = inputs
            .iter()
            .flatten()
            .filter_map(|token| {
                if let expression::Token::Band(b) = token {
                    Some(*b)
                } else {
                    None
                }
            })
            .collect();
        dependencies.sort_unstable();
        dependencies.dedup();

        let mosaic = if let Some(mosaic) = v.get("mosaic") {
            if !cfg!(feature = "mosaic") {
                return Err(unsupported("/mosaic (enable the mosaic feature)"));
            }
            let selection = mosaic["pixel_selection"]
                .as_str()
                .expect("validated selection")
                .to_owned();
            let after_channels = mosaic["stage"] == "after_channels";
            let rank = mosaic["rank_channel"].as_f64().unwrap_or(1.0) as usize - 1;
            let channels = if after_channels {
                inputs.len()
            } else {
                dependencies.len()
            };
            if matches!(selection.as_str(), "highest" | "lowest") && rank >= channels {
                return Err(AgError::query(
                    "/mosaic/rank_channel",
                    "outside the compiled channel dependencies",
                ));
            }
            Some(RasterMosaic {
                #[cfg(feature = "mosaic")]
                selection,
                after_channels,
                #[cfg(feature = "mosaic")]
                rank,
            })
        } else {
            None
        };

        let stretch = &v["stretch"];
        let method = stretch["method"].as_str().unwrap_or("none");
        let transfer = Stretch::parse(stretch, inputs.len());
        let statistics = Statistics::parse(&v["statistics"])?;

        only(&v["effects"], &["color_formula"], "/effects")?;
        let mut gamma = Vec::new();
        if let Some(ops) = v
            .pointer("/effects/color_formula")
            .and_then(Value::as_array)
        {
            for op in ops {
                if op["op"] != "gamma" {
                    return Err(unsupported("/effects/color_formula"));
                }
                gamma.push((
                    op["channels"]
                        .as_str()
                        .expect("normalized channels")
                        .to_owned(),
                    op["value"].as_f64().expect("validated gamma"),
                ));
            }
        }
        let data_domain = method == "none" && gamma.is_empty();
        let colors = match kind {
            "rgb" => Colors::Rgb,
            "gray" => Colors::Gray(renderer["renderer_invert"].as_bool().unwrap_or(false)),
            "single_color" => Colors::Solid(hex_color(
                renderer["color"].as_str().expect("validated color"),
            )),
            _ => {
                if let Some(name) = renderer["colormap_name"].as_str() {
                    if kind == "categorized" {
                        return Err(unsupported("/renderer/colormap_name"));
                    }
                    Colors::Named(Box::new(builtin_lut(name)?))
                } else if let Some(map) = renderer["colormap"].as_object() {
                    let mut entries = map
                        .iter()
                        .map(|(key, rgba)| {
                            Ok((
                                key.parse::<f64>()
                                    .map_err(|_| unsupported("/renderer/colormap"))?,
                                rgba_color(rgba),
                            ))
                        })
                        .collect::<AgResult<Vec<_>>>()?;
                    entries.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
                    Colors::Exact(entries, data_domain)
                } else if let Some(intervals) = renderer["colormap"].as_array() {
                    Colors::Intervals(
                        intervals
                            .iter()
                            .map(|entry| {
                                (
                                    entry[0][0].as_f64().expect("validated bound"),
                                    entry[0][1].as_f64().expect("validated bound"),
                                    rgba_color(&entry[1]),
                                )
                            })
                            .collect(),
                        data_domain,
                    )
                } else {
                    return Err(unsupported("/renderer/color_mapping"));
                }
            }
        };
        only(&v["image"], &["format", "tilesize", "lossless"], "/image")?;
        let format = match v
            .pointer("/image/format")
            .and_then(Value::as_str)
            .unwrap_or("png")
        {
            "png" => TileFormat::Png,
            "webp" if v.pointer("/image/lossless") == Some(&Value::Bool(true)) => TileFormat::Webp,
            _ => return Err(unsupported("/image/format (use png or lossless webp)")),
        };
        format.ensure_available()?;
        let resampler = |key: &str| -> AgResult<Resampling> {
            let name = v["resampling"][key].as_str().unwrap_or("nearest");
            if name == "mode" {
                return Err(unsupported(&format!("/resampling/{key} (mode Alpha)")));
            }
            Resampling::parse(name).ok_or_else(|| unsupported(&format!("/resampling/{key}")))
        };
        let out = Self {
            nodata: v["nodata"]
                .as_f64()
                .or_else(|| (v["nodata"] == "nan").then_some(f64::NAN)),
            resampling: resampler("read")?,
            reproject: resampler("reproject")?,
            tilesize: v
                .pointer("/image/tilesize")
                .and_then(Value::as_f64)
                .unwrap_or(256.0) as u32,
            format,
            raster: Arc::new(RasterPlan {
                key: style.to_json().map_err(sdk_error)?,
                categorical: kind == "categorized",
                mosaic,
                inputs,
                dependencies,
                stretch: transfer,
                statistics,
                transparent: stretch["range_policy"] == "transparent",
                colors,
                gamma,
                opacity: v["opacity"].as_f64().unwrap_or(1.0),
            }),
        };
        if !super::params::ALLOWED_TILE_SIZES.contains(&out.tilesize) {
            return Err(unsupported(
                "/image/tilesize (use 64, 128, 256, 512 or 1024)",
            ));
        }
        Ok(out)
    }

    /// The normalized Raster Style protocol document.
    pub fn raster_style_json(&self) -> &str {
        &self.raster.key
    }
}

fn rgba_color(v: &Value) -> [u8; 4] {
    std::array::from_fn(|i| v[i].as_f64().expect("validated RGBA") as u8)
}

fn hex_color(text: &str) -> [u8; 4] {
    std::array::from_fn(|i| {
        u8::from_str_radix(&text[1 + i * 2..3 + i * 2], 16).expect("normalized color")
    })
}

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
        let count = self.dependencies.len().max(1);
        let mut pixels = Array3::zeros((count, size, size));
        let mut mask = Array2::from_elem((size, size), true);
        // ponytail: one existing warp per dependency; batch with per-band masks
        // if profiling shows many-band expressions need a shared projection grid.
        let mut output = None;
        for i in 0..count {
            let constant = self.dependencies.is_empty();
            let values = if constant {
                Array2::zeros((src.pixels.height(), src.pixels.width()))
            } else {
                src.pixels.band_to_f64(self.dependencies[i])
            };
            let band = ImageData {
                pixels: PixelData::F64(values.insert_axis(Axis(0))),
                mask: src.mask.clone(),
                nodata: if constant { None } else { src.nodata },
                crs: src.crs.clone(),
                bbox: src.bbox,
                transform: src.transform,
            };
            let warped = crate::warp::warp(&band, dst_crs, dst_bbox, size, size, resampling)?;
            pixels
                .index_axis_mut(Axis(0), i)
                .assign(&warped.pixels.band_to_f64(0));
            if let Some(valid) = &warped.mask {
                ndarray::Zip::from(&mut mask)
                    .and(valid)
                    .for_each(|a, b| *a &= *b);
            }
            output = Some(warped);
        }
        let mut output = output.expect("at least the coverage band");
        output.pixels = PixelData::F64(pixels);
        output.mask = Some(mask);
        output.nodata = None;
        Ok(output)
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

    pub(crate) fn resolve_statistics(
        &self,
        image: &ImageData,
        channels_ready: bool,
    ) -> AgResult<Self> {
        let mut bound = self.compact();
        if channels_ready {
            bound.inputs = (0..self.inputs.len())
                .map(|b| vec![expression::Token::Band(b)])
                .collect();
            bound.dependencies = (0..self.inputs.len()).collect();
        }
        let src = image.pixels.as_f64();
        let n = image.pixels.width() * image.pixels.height();
        let k = self.statistics.sample_size.unwrap_or(n).min(n);
        let mut values = vec![Vec::with_capacity(k); self.inputs.len()];
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
                *v = expression::eval_rpn_with(tokens, &samples, &mut stack);
            }
            // RGB statistics consume only complete valid tuples.
            if tuple.iter().any(|v| !v.is_finite()) {
                continue;
            }
            for (channel, &v) in values.iter_mut().zip(&tuple) {
                channel.push(v);
            }
        }
        let mut resolved = self.clone();
        resolved.stretch = Stretch::Resolved(
            values
                .into_iter()
                .map(|v| self.stretch.resolve(v))
                .collect::<AgResult<Vec<Transfer>>>()?,
        );
        Ok(resolved)
    }

    pub(crate) fn render(
        &self,
        image: &ImageData,
        nodata: Option<f64>,
        format: TileFormat,
    ) -> AgResult<Bytes> {
        let src = image.pixels.as_f64();
        let transfers = self.stretch.transfers()?;
        let bands = image.pixels.bands();
        if self.dependencies.iter().any(|&b| b >= bands) {
            return Err(AgError::query(
                "/renderer",
                "referenced source band does not exist",
            ));
        }
        let (h, w) = (image.pixels.height(), image.pixels.width());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bounds::{Bbox, GeoTransform},
        crs::Crs,
    };

    #[test]
    fn statistics_apply_expressions_valid_tuples_and_sampling_before_stretch() {
        let src = ImageData {
            pixels: PixelData::F64(
                Array3::from_shape_vec((2, 1, 4), vec![1., 2., 0., 1., 0., 20., 999., 100.])
                    .unwrap(),
            ),
            mask: Some(Array2::from_shape_vec((1, 4), vec![true, true, true, false]).unwrap()),
            nodata: None,
            crs: Crs::epsg(4326),
            bbox: Bbox::new(0., 0., 4., 1.),
            transform: GeoTransform::from_gdal(&[0., 1., 0., 1., 0., -1.]),
        };
        let style =
            TileStyle::from_raster_query("version=2.0&type=gray&expression=b2%2Fb1&method=minmax")
                .unwrap();
        let plan = style.raster.resolve_statistics(&src, false).unwrap();
        let rgba = image::load_from_memory(&plan.render(&src, None, TileFormat::Png).unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(
            rgba.into_raw(),
            vec![0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        let mut plan = (*style.raster).clone();
        plan.statistics.sample_size = Some(1);
        let resolved = plan.resolve_statistics(&src, false).unwrap();
        assert_eq!(
            resolved.stretch.transfers().unwrap()[0].apply(10., false),
            Some(0.5)
        );
        let mut invalid = src.clone();
        invalid.mask.as_mut().unwrap().fill(false);
        assert!(matches!(
            plan.resolve_statistics(&invalid, false)
                .unwrap()
                .stretch
                .transfers()
                .unwrap()[0],
            Transfer::Empty
        ));
    }

    #[test]
    fn warp_uses_only_dependencies_and_does_not_reapply_source_nodata() {
        let src = ImageData {
            pixels: PixelData::F64(
                Array3::from_shape_vec((2, 2, 2), vec![0., 2., 0., 2., 1., 1., 1., 1.]).unwrap(),
            ),
            mask: None,
            nodata: Some(1.),
            crs: Crs::epsg(3857),
            bbox: Bbox::new(0., 0., 2., 2.),
            transform: GeoTransform::from_gdal(&[0., 1., 0., 2., 0., -1.]),
        };
        let style =
            TileStyle::from_raster_query("version=2.0&type=gray&bidx=1&method=linear&rescale=0,2")
                .unwrap();
        let bytes = style
            .raster
            .render_warped(
                &src,
                &src.crs,
                src.bbox,
                1,
                Resampling::Bilinear,
                TileFormat::Png,
            )
            .unwrap();
        assert_eq!(
            image::load_from_memory(&bytes)
                .unwrap()
                .to_rgba8()
                .into_raw(),
            [128, 128, 128, 255]
        );
    }

    #[test]
    fn negative_kernel_support_survives_raster_style_warp_and_render() {
        let src = ImageData {
            pixels: PixelData::F64(
                Array3::from_shape_vec((1, 1, 4), vec![10., 20., 30., 40.]).unwrap(),
            ),
            mask: Some(ndarray::array![[true, false, false, false]]),
            nodata: None,
            crs: Crs::epsg(3857),
            bbox: Bbox::new(0., 0., 4., 1.),
            transform: GeoTransform::from_gdal(&[0., 1., 0., 1., 0., -1.]),
        };
        for kernel in ["cubic", "lanczos"] {
            let style = TileStyle::from_raster_query(&format!(
                "version=2.0&type=gray&bidx=1&resampling={kernel}"
            ))
            .unwrap();
            let bytes = style
                .raster
                .render_warped(
                    &src,
                    &src.crs,
                    Bbox::new(1.5, 0., 2.5, 1.),
                    1,
                    style.resampling(),
                    TileFormat::Png,
                )
                .unwrap();
            assert_eq!(
                image::load_from_memory(&bytes)
                    .unwrap()
                    .to_rgba8()
                    .into_raw(),
                [10, 10, 10, 255],
                "{kernel}"
            );
        }
    }
}

#[cfg(all(test, feature = "mosaic"))]
mod mosaic_tests {
    use super::*;
    use crate::{
        bounds::{Bbox, GeoTransform},
        crs::Crs,
        mosaic::selection::Reducer,
    };
    use serde_json::json;

    fn render_sources(style: Value, sources: &[&[f64]]) -> Vec<u8> {
        let style = TileStyle::from_json(&style.to_string()).unwrap();
        let raster = style.raster;
        let config = raster.mosaic.as_ref().unwrap();
        let mut reducer: Option<Reducer<crate::mosaic::selection::NonEmptyReducer>> = None;
        for source in sources {
            let image = ImageData {
                pixels: PixelData::F64(
                    Array3::from_shape_vec((source.len(), 1, 1), source.to_vec()).unwrap(),
                ),
                mask: None,
                nodata: None,
                crs: Crs::epsg(3857),
                bbox: Bbox::new(0., 0., 1., 1.),
                transform: GeoTransform::from_gdal(&[0., 1., 0., 1., 0., -1.]),
            };
            let image = raster
                .warp_dependencies(&image, &image.crs, image.bbox, 1, Resampling::Nearest)
                .unwrap();
            let image = raster.prepare_mosaic(image);
            if let Some(r) = &mut reducer {
                r.feed(&image).unwrap();
            } else {
                reducer = Some(
                    Reducer::new_raster(config.selection(), config.rank)
                        .feed(&image)
                        .unwrap(),
                );
            }
        }
        let image = reducer.unwrap().finish().unwrap();
        let bytes = raster.render_mosaic(&image, TileFormat::Png).unwrap();
        image::load_from_memory(&bytes)
            .unwrap()
            .to_rgba8()
            .into_raw()
    }

    #[test]
    fn stage_changes_expression_order_and_invalid_first_fallback() {
        let mut style = json!({"version":"2.0","renderer":{"type":"gray","expression":"b2/b1"},"stretch":{"method":"linear","rescale":[[0,4]]},"mosaic":{"pixel_selection":"mean","stage":"before_channels"}});
        assert_eq!(
            render_sources(style.clone(), &[&[2., 6.], &[6., 6.]]),
            [96, 96, 96, 255]
        );
        style["mosaic"]["stage"] = json!("after_channels");
        assert_eq!(
            render_sources(style.clone(), &[&[2., 6.], &[6., 6.]]),
            [128, 128, 128, 255]
        );
        style["mosaic"]["pixel_selection"] = json!("first");
        assert_eq!(
            render_sources(style.clone(), &[&[0., 1.], &[1., 1.]]),
            [64, 64, 64, 255]
        );
        style["mosaic"]["stage"] = json!("before_channels");
        assert_eq!(render_sources(style, &[&[0., 1.], &[1., 1.]]), [0, 0, 0, 0]);
    }

    #[test]
    fn ranking_uses_stage_positions_and_preserves_whole_tuples() {
        let mut style = json!({"version":"2.0","renderer":{"type":"rgb","bidx":[3,1,3]},"stretch":{"method":"linear","rescale":[[0,100]]},"mosaic":{"pixel_selection":"highest","rank_channel":2,"stage":"before_channels"}});
        assert_eq!(
            render_sources(style.clone(), &[&[10., 0., 90.], &[80., 0., 20.]]),
            [230, 26, 230, 255]
        );
        style["mosaic"]["stage"] = json!("after_channels");
        assert_eq!(
            render_sources(style.clone(), &[&[10., 0., 90.], &[80., 0., 20.]]),
            [51, 204, 51, 255]
        );
        style["mosaic"]["pixel_selection"] = json!("lowest");
        assert_eq!(
            render_sources(style.clone(), &[&[10., 0., 90.], &[80., 0., 20.]]),
            [230, 26, 230, 255]
        );
        style["mosaic"]["pixel_selection"] = json!("highest");
        assert_eq!(
            render_sources(style, &[&[10., 0., 90.], &[10., 0., 20.]]),
            [230, 26, 230, 255]
        );
    }

    #[test]
    fn incomplete_tuples_never_mix_sources_and_opacity_runs_once() {
        let style = json!({"version":"2.0","renderer":{"type":"rgb","bidx":[1,2,1]},"opacity":0.5,"mosaic":{"pixel_selection":"first","stage":"before_channels"}});
        assert_eq!(
            render_sources(
                style.clone(),
                &[&[1., f64::NAN], &[f64::NAN, 2.], &[3., 4.]]
            ),
            [3, 4, 3, 128]
        );
        assert_eq!(
            render_sources(style, &[&[1., f64::NAN], &[f64::NAN, 2.]]),
            [0, 0, 0, 0]
        );
    }

    #[test]
    fn mean_median_avoid_overflow_and_preserve_fractional_results() {
        for method in ["mean", "median"] {
            let style = json!({"version":"2.0","renderer":{"type":"gray","bidx":[1]},"stretch":{"method":"linear","rescale":[[0.,1e308]]},"mosaic":{"pixel_selection":method,"stage":"before_channels"}});
            assert_eq!(
                render_sources(style.clone(), &[&[1e308], &[1e308]]),
                [255, 255, 255, 255]
            );
            assert_eq!(
                render_sources(style, &[&[-1e308], &[1e308]]),
                [0, 0, 0, 255]
            );
            let style = json!({"version":"2.0","renderer":{"type":"gray","bidx":[1]},"mosaic":{"pixel_selection":method,"stage":"after_channels"}});
            assert_eq!(render_sources(style, &[&[2.], &[3.]]), [3, 3, 3, 255]);
        }
    }

    #[test]
    fn expression_rank_bounds_are_checked_after_parsing() {
        for (stage, rank) in [("before_channels", 3), ("after_channels", 2)] {
            let style = json!({"version":"2.0","renderer":{"type":"gray","expression":"b1+b3"},"mosaic":{"pixel_selection":"highest","stage":stage,"rank_channel":rank}});
            assert!(TileStyle::from_json(&style.to_string())
                .unwrap_err()
                .to_string()
                .contains("rank_channel"));
        }
    }
}
