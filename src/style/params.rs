//! Titiler-style URL params → `TileStyle` → `CompiledTileStyle`.

use crate::error::{AgError, AgResult};
use crate::render::color_formula::{parse as parse_formula, Op};
use crate::render::composite::{ColourDefinition, Composite};
use crate::render::pipeline::CompiledTileStyle;
use crate::style::stretch::{parse_stretch_kind, Stretch};
use crate::warp::resampler::Resampling;

pub const ALLOWED_TILE_SIZES: [u32; 5] = [64, 128, 256, 512, 1024];

#[derive(Debug, Clone)]
pub struct TileStyle {
    pub bidx: Vec<u32>, // 1-indexed as given in URL
    pub rescale: Vec<(f64, f64)>,
    pub stretch_kind: Option<String>,
    pub stretch_pc: Option<(f64, f64)>,
    pub stretch_sigma: Option<f64>,
    pub nodata: Option<f64>,
    pub colormap_name: Option<String>,
    pub color_formula: Option<String>,
    pub resampling: Resampling,
    pub reproject: Resampling,
    pub tilesize: u32,
}

impl Default for TileStyle {
    fn default() -> Self {
        Self {
            bidx: vec![],
            rescale: vec![],
            stretch_kind: None,
            stretch_pc: None,
            stretch_sigma: None,
            nodata: None,
            colormap_name: None,
            color_formula: None,
            resampling: Resampling::Nearest,
            reproject: Resampling::Nearest,
            tilesize: 256,
        }
    }
}

impl TileStyle {
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

    /// `None` → caller applies the dtype-aware default (u8 keeps its
    /// fast path; others use `Stretch::DEFAULT`). `Some(spec)` →
    /// an explicit `?stretch=` mode. Invalid params → `AgError::query`
    /// (HTTP 400). A non-empty `rescale` (Custom) makes this irrelevant
    /// — the caller checks `rescale` first.
    pub fn resolved_stretch(&self) -> AgResult<Option<Stretch>> {
        match &self.stretch_kind {
            None => {
                if self.stretch_pc.is_some() || self.stretch_sigma.is_some() {
                    return Err(AgError::query(
                        "stretch",
                        "pc/sigma given without stretch=",
                    ));
                }
                Ok(None)
            }
            Some(k) => Ok(Some(parse_stretch_kind(
                k,
                self.stretch_pc,
                self.stretch_sigma,
            )?)),
        }
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
            "stretch" => self.stretch_kind = Some(v.to_owned()),
            "pc" => {
                let (a, b) = v.split_once(',').ok_or_else(|| AgError::query("pc", v))?;
                self.stretch_pc = Some((
                    a.parse().map_err(|_| AgError::query("pc", a))?,
                    b.parse().map_err(|_| AgError::query("pc", b))?,
                ));
            }
            "sigma" => {
                self.stretch_sigma =
                    Some(v.parse().map_err(|_| AgError::query("sigma", v))?)
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

pub fn compile(style: &TileStyle) -> AgResult<CompiledTileStyle> {
    validate_tilesize(style.tilesize)?;
    if let Some(&bad) = style.bidx.iter().find(|&&b| b == 0) {
        return Err(AgError::query("bidx", format!("{bad} is not 1-indexed")));
    }

    let auto_rgb = style.bidx.is_empty()
        && style.rescale.is_empty()
        && style.colormap_name.is_none()
        && style.color_formula.is_none();

    let composite = if let Some(name) = &style.colormap_name {
        Composite::from_definition(
            &ColourDefinition::Named(name.clone()),
            style.rescale.first().map(|r| vec![r.0]),
            style.rescale.first().map(|r| vec![r.1]),
        )?
    } else if style.bidx.len() == 3 || (style.bidx.is_empty() && style.rescale.len() == 3) {
        let lo: [f64; 3] = [
            style.rescale.first().map(|r| r.0).unwrap_or(0.0),
            style.rescale.get(1).map(|r| r.0).unwrap_or(0.0),
            style.rescale.get(2).map(|r| r.0).unwrap_or(0.0),
        ];
        let hi: [f64; 3] = [
            style.rescale.first().map(|r| r.1).unwrap_or(255.0),
            style.rescale.get(1).map(|r| r.1).unwrap_or(255.0),
            style.rescale.get(2).map(|r| r.1).unwrap_or(255.0),
        ];
        Composite::from_definition(&ColourDefinition::Rgb(lo, hi), None, None)?
    } else {
        // default: greyscale gradient on first band
        Composite::from_definition(
            &ColourDefinition::Named("greys".into()),
            style.rescale.first().map(|r| vec![r.0]),
            style.rescale.first().map(|r| vec![r.1]),
        )?
    };

    let formula: Vec<Op> = match &style.color_formula {
        Some(s) if !s.is_empty() => parse_formula(s)?,
        _ => vec![],
    };

    Ok(CompiledTileStyle {
        composite,
        rescale: style.rescale.clone(),
        formula,
        bidx: style.bidx.iter().map(|b| *b as usize - 1).collect(),
        nodata: style.nodata,
        tile_size: style.tilesize,
        resampling: style.resampling,
        auto_rgb,
    })
}

/// Cached [`compile`]. Every tile in a viewport carries an identical
/// `TileStyle`, so the nom color-formula parse + colormap LUT build is
/// pure per-request redundancy. Keyed by the style's `Debug` string
/// (stable, and `f64` fields make `Hash`/`Eq` impractical). Returns a
/// shared `Arc` so cache hits clone a pointer. Concurrent misses may
/// both compile — `compile` is cheap and idempotent, so single-flight
/// isn't worth the closure-lifetime cost here.
pub async fn compile_cached(style: &TileStyle) -> AgResult<std::sync::Arc<CompiledTileStyle>> {
    use std::sync::{Arc, OnceLock};
    static CACHE: OnceLock<moka::future::Cache<String, Arc<CompiledTileStyle>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| moka::future::Cache::new(256));
    let key = format!("{style:?}");
    if let Some(hit) = cache.get(&key).await {
        return Ok(hit);
    }
    let compiled = Arc::new(compile(style)?);
    cache.insert(key, compiled.clone()).await;
    Ok(compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_titiler_url() {
        let s = TileStyle::from_query(
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
        let s = TileStyle::from_query("nodata=nan").unwrap();
        assert!(s.nodata.unwrap().is_nan());
    }

    #[test]
    fn invalid_bidx_errors() {
        assert!(TileStyle::from_query("bidx=abc").is_err());
    }

    #[test]
    fn decoded_pairs_preserve_delimiters_inside_values() {
        let s = TileStyle::from_pairs([
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
            assert!(TileStyle::from_query(&format!("tilesize={size}")).is_ok());
        }
        assert!(TileStyle::from_query("tilesize=1024").is_ok());
        assert!(TileStyle::from_query("tilesize=96").is_err());
        assert!(TileStyle::from_query("tilesize=4096").is_err());
        let s = TileStyle {
            tilesize: 4096,
            ..TileStyle::default()
        };
        assert!(compile(&s).is_err());
    }

    #[test]
    fn compile_named_colormap() {
        let s = TileStyle::from_query("colormap_name=viridis&rescale=0,1000").unwrap();
        let c = compile(&s).unwrap();
        assert_eq!(c.tile_size, 256);
    }

    #[test]
    fn compile_rejects_zero_bidx() {
        let s = TileStyle::from_query("bidx=0").unwrap();
        assert!(compile(&s).is_err());
    }

    #[test]
    fn parses_stretch_params() {
        use crate::style::stretch::Stretch;

        let s = TileStyle::from_query("stretch=percent&pc=2,98").unwrap();
        assert_eq!(
            s.resolved_stretch().unwrap(),
            Some(Stretch::Percent { lo: 2.0, hi: 98.0 })
        );

        let sd = TileStyle::from_query("stretch=stddev&sigma=3.0").unwrap();
        assert_eq!(
            sd.resolved_stretch().unwrap(),
            Some(Stretch::StdDev { k: 3.0 })
        );

        let none = TileStyle::from_query("").unwrap();
        assert_eq!(none.resolved_stretch().unwrap(), None);

        let bad = TileStyle::from_query("stretch=nope").unwrap();
        assert!(bad.resolved_stretch().is_err());

        // Malformed pc → error at query parse time (AgError::query).
        assert!(TileStyle::from_query("stretch=percent&pc=abc,2").is_err());
        assert!(TileStyle::from_query("stretch=percent&pc=2").is_err());

        // Orphan pc/sigma without stretch= → rejected (not silently dropped).
        let orphan = TileStyle::from_query("pc=2,98").unwrap();
        assert!(orphan.resolved_stretch().is_err());
    }
}
