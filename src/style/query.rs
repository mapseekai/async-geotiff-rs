//! Titiler-style URL query → `RenderSpec` → `CompiledRenderSpec`.

use crate::error::{AgError, AgResult};
use crate::render::color_formula::{parse as parse_formula, Op};
use crate::render::composite::{ColourDefinition, Composite};
use crate::render::pipeline::CompiledRenderSpec;
use crate::warp::resampler::Resampling;

pub const ALLOWED_TILE_SIZES: [u32; 5] = [64, 128, 256, 512, 1024];

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
        if q.is_empty() {
            return Ok(Self::default());
        }
        Self::from_pairs(
            url::form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned())),
        )
    }

    /// Parse already-decoded query key/value pairs.
    ///
    /// Axum's `Query<Vec<(String, String)>>` has already URL-decoded values.
    /// Re-stringifying those pairs would let decoded `&` or `=` characters
    /// become new query delimiters, so HTTP callers should use this entrypoint.
    pub fn from_pairs<I, K, V>(pairs: I) -> AgResult<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut s = Self::default();
        for (k, v) in pairs {
            s.apply_param(k.as_ref(), v.as_ref())?;
        }
        Ok(s)
    }

    fn apply_param(&mut self, k: &str, v: &str) -> AgResult<()> {
        match k {
            "bidx" => self
                .bidx
                .push(v.parse().map_err(|_| AgError::query("bidx", v))?),
            "rescale" => {
                let (a, b) = v
                    .split_once(',')
                    .ok_or_else(|| AgError::query("rescale", v))?;
                self.rescale.push((
                    a.parse().map_err(|_| AgError::query("rescale", a))?,
                    b.parse().map_err(|_| AgError::query("rescale", b))?,
                ));
            }
            "nodata" => self.nodata = Some(parse_nodata(v)?),
            "colormap_name" => self.colormap_name = Some(v.to_owned()),
            "color_formula" => self.color_formula = Some(v.to_owned()),
            "resampling" => {
                self.resampling =
                    Resampling::parse(v).ok_or_else(|| AgError::query("resampling", v))?
            }
            "reproject" => {
                self.reproject =
                    Resampling::parse(v).ok_or_else(|| AgError::query("reproject", v))?
            }
            "tilesize" => {
                self.tilesize = v.parse().map_err(|_| AgError::query("tilesize", v))?;
                validate_tilesize(self.tilesize)?;
            }
            _ => {} // unknown params silently ignored (titiler-compatible)
        }
        Ok(())
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

fn validate_tilesize(tilesize: u32) -> AgResult<()> {
    if ALLOWED_TILE_SIZES.contains(&tilesize) {
        Ok(())
    } else {
        Err(AgError::query(
            "tilesize",
            format!("must be one of {ALLOWED_TILE_SIZES:?}"),
        ))
    }
}

pub fn compile(spec: &RenderSpec) -> AgResult<CompiledRenderSpec> {
    validate_tilesize(spec.tilesize)?;
    if let Some(&bad) = spec.bidx.iter().find(|&&b| b == 0) {
        return Err(AgError::query("bidx", format!("{bad} is not 1-indexed")));
    }

    let auto_rgb = spec.bidx.is_empty()
        && spec.rescale.is_empty()
        && spec.colormap_name.is_none()
        && spec.color_formula.is_none();

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
            spec.rescale.first().map(|r| r.1).unwrap_or(255.0),
            spec.rescale.get(1).map(|r| r.1).unwrap_or(255.0),
            spec.rescale.get(2).map(|r| r.1).unwrap_or(255.0),
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
        bidx: spec.bidx.iter().map(|b| *b as usize - 1).collect(),
        nodata: spec.nodata,
        tile_size: spec.tilesize,
        resampling: spec.resampling,
        auto_rgb,
    })
}

/// Cached [`compile`]. Every tile in a viewport carries an identical
/// `RenderSpec`, so the nom color-formula parse + colormap LUT build is
/// pure per-request redundancy. Keyed by the spec's `Debug` string
/// (stable, and `f64` fields make `Hash`/`Eq` impractical). Returns a
/// shared `Arc` so cache hits clone a pointer. Concurrent misses may
/// both compile — `compile` is cheap and idempotent, so single-flight
/// isn't worth the closure-lifetime cost here.
pub async fn compile_cached(spec: &RenderSpec) -> AgResult<std::sync::Arc<CompiledRenderSpec>> {
    use std::sync::{Arc, OnceLock};
    static CACHE: OnceLock<moka::future::Cache<String, Arc<CompiledRenderSpec>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| moka::future::Cache::new(256));
    let key = format!("{spec:?}");
    if let Some(hit) = cache.get(&key).await {
        return Ok(hit);
    }
    let compiled = Arc::new(compile(spec)?);
    cache.insert(key, compiled.clone()).await;
    Ok(compiled)
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
    fn decoded_pairs_preserve_delimiters_inside_values() {
        let s = RenderSpec::from_pairs([
            ("color_formula", "gamma RGB 1.2&not_a_param=1"),
            ("bidx", "1"),
        ])
        .unwrap();
        assert_eq!(
            s.color_formula.as_deref(),
            Some("gamma RGB 1.2&not_a_param=1")
        );
        assert_eq!(s.bidx, vec![1]);
    }

    #[test]
    fn tilesize_is_bounded() {
        for size in ALLOWED_TILE_SIZES {
            assert!(RenderSpec::from_query(&format!("tilesize={size}")).is_ok());
        }
        assert!(RenderSpec::from_query("tilesize=1024").is_ok());
        assert!(RenderSpec::from_query("tilesize=96").is_err());
        assert!(RenderSpec::from_query("tilesize=4096").is_err());
        let s = RenderSpec {
            tilesize: 4096,
            ..RenderSpec::default()
        };
        assert!(compile(&s).is_err());
    }

    #[test]
    fn compile_named_colormap() {
        let s = RenderSpec::from_query("colormap_name=viridis&rescale=0,1000").unwrap();
        let c = compile(&s).unwrap();
        assert_eq!(c.tile_size, 256);
    }

    #[test]
    fn compile_rejects_zero_bidx() {
        let s = RenderSpec::from_query("bidx=0").unwrap();
        assert!(compile(&s).is_err());
    }
}
