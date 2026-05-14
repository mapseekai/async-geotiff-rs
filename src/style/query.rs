//! Titiler-style URL query → `RenderSpec` → `CompiledRenderSpec`.

use crate::error::{AgError, AgResult};
use crate::render::color_formula::{parse as parse_formula, Op};
use crate::render::composite::{ColourDefinition, Composite};
use crate::render::pipeline::CompiledRenderSpec;
use crate::warp::resampler::Resampling;

#[derive(Debug, Clone)]
pub struct RenderSpec {
    pub bidx: Vec<u32>, // 1-indexed as given in URL
    pub rescale: Vec<(f64, f64)>,
    pub nodata: Option<f64>,
    pub colormap_name: Option<String>,
    pub color_formula: Option<String>,
    pub resampling: Resampling,
    pub reproject: Resampling,
    pub tilesize: u32,
}

impl Default for RenderSpec {
    fn default() -> Self {
        Self {
            bidx: vec![],
            rescale: vec![],
            nodata: None,
            colormap_name: None,
            color_formula: None,
            resampling: Resampling::Nearest,
            reproject: Resampling::Nearest,
            tilesize: 256,
        }
    }
}

impl RenderSpec {
    /// Parse a URL query string (`?` not included).
    pub fn from_query(q: &str) -> AgResult<Self> {
        let mut s = Self::default();
        if q.is_empty() {
            return Ok(s);
        }
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = urldecode(v);
            match k {
                "bidx" => s
                    .bidx
                    .push(v.parse().map_err(|_| AgError::query("bidx", v.clone()))?),
                "rescale" => {
                    let (a, b) = v
                        .split_once(',')
                        .ok_or_else(|| AgError::query("rescale", v.clone()))?;
                    s.rescale.push((
                        a.parse().map_err(|_| AgError::query("rescale", a))?,
                        b.parse().map_err(|_| AgError::query("rescale", b))?,
                    ));
                }
                "nodata" => s.nodata = Some(parse_nodata(&v)?),
                "colormap_name" => s.colormap_name = Some(v.clone()),
                "color_formula" => s.color_formula = Some(v.clone()),
                "resampling" => {
                    s.resampling = Resampling::parse(&v)
                        .ok_or_else(|| AgError::query("resampling", v.clone()))?
                }
                "reproject" => {
                    s.reproject = Resampling::parse(&v)
                        .ok_or_else(|| AgError::query("reproject", v.clone()))?
                }
                "tilesize" => {
                    s.tilesize = v
                        .parse()
                        .map_err(|_| AgError::query("tilesize", v.clone()))?
                }
                _ => {} // unknown params silently ignored (titiler-compatible)
            }
        }
        Ok(s)
    }
}

fn parse_nodata(v: &str) -> AgResult<f64> {
    match v {
        "nan" => Ok(f64::NAN),
        "inf" => Ok(f64::INFINITY),
        "-inf" => Ok(f64::NEG_INFINITY),
        _ => v.parse().map_err(|_| AgError::query("nodata", v)),
    }
}

fn urldecode(s: &str) -> String {
    let s = s.replace('+', " ");
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(b as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

pub fn compile(spec: &RenderSpec) -> AgResult<CompiledRenderSpec> {
    let composite = if let Some(name) = &spec.colormap_name {
        Composite::from_definition(
            &ColourDefinition::Named(name.clone()),
            spec.rescale.first().map(|r| vec![r.0]),
            spec.rescale.first().map(|r| vec![r.1]),
        )?
    } else if spec.bidx.len() == 3 || (spec.bidx.is_empty() && spec.rescale.len() == 3) {
        let lo: [f64; 3] = [
            spec.rescale.first().map(|r| r.0).unwrap_or(0.0),
            spec.rescale.get(1).map(|r| r.0).unwrap_or(0.0),
            spec.rescale.get(2).map(|r| r.0).unwrap_or(0.0),
        ];
        let hi: [f64; 3] = [
            spec.rescale.first().map(|r| r.1).unwrap_or(1.0),
            spec.rescale.get(1).map(|r| r.1).unwrap_or(1.0),
            spec.rescale.get(2).map(|r| r.1).unwrap_or(1.0),
        ];
        Composite::from_definition(&ColourDefinition::Rgb(lo, hi), None, None)?
    } else {
        // default: greyscale gradient on first band
        Composite::from_definition(
            &ColourDefinition::Named("greys".into()),
            spec.rescale.first().map(|r| vec![r.0]),
            spec.rescale.first().map(|r| vec![r.1]),
        )?
    };

    let formula: Vec<Op> = match &spec.color_formula {
        Some(s) if !s.is_empty() => parse_formula(s)?,
        _ => vec![],
    };

    Ok(CompiledRenderSpec {
        composite,
        rescale: spec.rescale.clone(),
        formula,
        bidx: spec
            .bidx
            .iter()
            .map(|b| (*b as usize).saturating_sub(1))
            .collect(),
        nodata: spec.nodata,
        tile_size: spec.tilesize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_titiler_url() {
        let s = RenderSpec::from_query(
            "bidx=1&bidx=2&bidx=3&rescale=0,3000&rescale=0,3000&rescale=0,3000\
             &colormap_name=viridis&color_formula=gamma%20RGB%201.2\
             &resampling=bilinear&tilesize=512",
        )
        .unwrap();
        assert_eq!(s.bidx, vec![1, 2, 3]);
        assert_eq!(s.rescale.len(), 3);
        assert_eq!(s.colormap_name.as_deref(), Some("viridis"));
        assert_eq!(s.color_formula.as_deref(), Some("gamma RGB 1.2"));
        assert_eq!(s.resampling, Resampling::Bilinear);
        assert_eq!(s.tilesize, 512);
    }

    #[test]
    fn nodata_special_values() {
        let s = RenderSpec::from_query("nodata=nan").unwrap();
        assert!(s.nodata.unwrap().is_nan());
    }

    #[test]
    fn invalid_bidx_errors() {
        assert!(RenderSpec::from_query("bidx=abc").is_err());
    }

    #[test]
    fn compile_named_colormap() {
        let s = RenderSpec::from_query("colormap_name=viridis&rescale=0,1000").unwrap();
        let c = compile(&s).unwrap();
        assert_eq!(c.tile_size, 256);
    }
}
