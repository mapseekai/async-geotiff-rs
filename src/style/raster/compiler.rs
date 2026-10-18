//! SDK adaptation and supported-capability validation.
use super::{Colors, RasterMosaic, RasterPlan};
use crate::{
    error::{AgError, AgResult},
    render::{colormap::builtin_lut, expression},
    style::{
        stretch::{Statistics, Stretch},
        RasterStyle, TileFormat, TileStyle,
    },
    warp::resampler::Resampling,
};
use serde_json::Value;
use std::sync::Arc;

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
    /// Parse Raster Style 3.0 JSON with versioned Mosaic and Tile extensions.
    /// Unsupported renderer capabilities fail before any raster is opened.
    pub fn from_json(json: &str) -> AgResult<Self> {
        Self::from_raster_style(&RasterStyle::from_json(json).map_err(sdk_error)?)
    }

    /// Parse an original, percent-encoded Q3 query. Do not URL-decode it first.
    pub fn from_raster_query(query: &str) -> AgResult<Self> {
        Self::from_raster_style(&raster_style::decode_query(query).map_err(sdk_error)?)
    }

    /// Adapt an SDK-validated document to the supported renderer.
    pub fn from_raster_style(style: &RasterStyle) -> AgResult<Self> {
        let supported: &[&str] = if cfg!(feature = "mosaic") {
            &[
                raster_style::TILE_SCHEMA_URL,
                raster_style::MOSAIC_SCHEMA_URL,
            ]
        } else {
            &[raster_style::TILE_SCHEMA_URL]
        };
        style
            .require_extension_support(supported)
            .map_err(sdk_error)?;
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
                "extensions",
                "raster:tile",
                "calibration",
                "raster:mosaic",
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
        let inputs = compile_inputs(renderer)?;
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

        let mosaic = if let Some(mosaic) = v.get("raster:mosaic") {
            if !cfg!(feature = "mosaic") {
                return Err(unsupported("/raster:mosaic (enable the mosaic feature)"));
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
                    "/raster:mosaic/rank_channel",
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
        let colors = compile_colors(renderer, kind, data_domain)?;
        only(
            &v["raster:tile"],
            &["format", "tilesize", "lossless"],
            "/raster:tile",
        )?;
        let format = match v
            .pointer("/raster:tile/format")
            .and_then(Value::as_str)
            .unwrap_or("png")
        {
            "png" => TileFormat::Png,
            "webp" if v.pointer("/raster:tile/lossless") == Some(&Value::Bool(true)) => {
                TileFormat::Webp
            }
            _ => {
                return Err(unsupported(
                    "/raster:tile/format (use png or lossless webp)",
                ))
            }
        };
        format.ensure_available()?;
        let resampler = |key: &str| -> AgResult<Resampling> {
            let name = v["resampling"][key].as_str().unwrap_or("nearest");
            if name == "mode" {
                return Err(unsupported(&format!("/resampling/{key} (mode Alpha)")));
            }
            Resampling::parse(name).ok_or_else(|| unsupported(&format!("/resampling/{key}")))
        };
        let (statistics_key, stretch_key) = if transfer.needs_statistics() {
            let key = serde_json::json!({
                "bidx": renderer.get("bidx"), "expression": renderer.get("expression"),
                "language": renderer.get("language"), "resampling": v.get("resampling"),
                "nodata": v.get("nodata"), "calibration": v.get("calibration"),
                "raster:mosaic": v.get("raster:mosaic"), "statistics": v.get("statistics")
            })
            .to_string();
            let mut stretch = stretch.clone();
            if let Some(object) = stretch.as_object_mut() {
                object.remove("range_policy");
            }
            (key, stretch.to_string())
        } else {
            (String::new(), String::new())
        };
        let out = Self {
            nodata: v["nodata"]
                .as_f64()
                .or_else(|| (v["nodata"] == "nan").then_some(f64::NAN)),
            resampling: resampler("read")?,
            reproject: resampler("reproject")?,
            tilesize: v
                .pointer("/raster:tile/tilesize")
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
                statistics_key,
                stretch_key,
                rgb_luts: Arc::default(),
            }),
        };
        if !crate::style::params::ALLOWED_TILE_SIZES.contains(&out.tilesize) {
            return Err(unsupported(
                "/raster:tile/tilesize (use 64, 128, 256, 512 or 1024)",
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

fn compile_inputs(renderer: &Value) -> AgResult<Vec<Vec<expression::Token>>> {
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
    Ok(inputs)
}

fn compile_colors(renderer: &Value, kind: &str, data_domain: bool) -> AgResult<Colors> {
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
    Ok(colors)
}
