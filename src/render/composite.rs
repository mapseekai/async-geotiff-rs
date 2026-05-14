//! `Composite` — precompiled style ready for the hot path.
//! Three variants of dispatch: Gradient (LUT), Discrete (HashMap), Rgb (per-band rescale).

use std::collections::HashMap;

use serde::Deserialize;

use crate::error::AgResult;
use crate::render::colormap::{builtin_lut, lut_from_stops, Lut};
use crate::render::colour::Colour;

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ColourDefinition {
    Named(String),
    Colours(Vec<Colour>),
    ColoursAndBreaks(Vec<(f64, Colour)>),
    Discrete(Vec<(i64, Colour)>),
    Rgb([f64; 3], [f64; 3]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeKind {
    Gradient,
    Discrete,
    Rgb,
}

#[derive(Debug, Clone)]
pub struct Composite {
    pub vmin: Option<Vec<f64>>,
    pub vmax: Option<Vec<f64>>,
    pub lut: Option<Lut>,
    pub discrete: Option<HashMap<i64, [u8; 4]>>,
    pub kind: CompositeKind,
    pub bands: usize,
}

pub trait HandleGet {
    fn get(&self, values: &[f64], nodata: Option<&[f64]>) -> [u8; 4];
}

impl HandleGet for Composite {
    fn get(&self, values: &[f64], nodata: Option<&[f64]>) -> [u8; 4] {
        match self.kind {
            CompositeKind::Discrete => {
                let v = values[0].trunc() as i64;
                self.discrete
                    .as_ref()
                    .and_then(|m| m.get(&v).copied())
                    .unwrap_or([0, 0, 0, 0])
            }
            CompositeKind::Gradient => {
                let v = values[0];
                if let Some(nd) = nodata {
                    if !nd.is_empty() && (v - nd[0]).abs() < 1e-9 {
                        return [0, 0, 0, 0];
                    }
                }
                let (lo, hi) = match (&self.vmin, &self.vmax) {
                    (Some(a), Some(b)) => (a[0], b[0]),
                    _ => (0.0, 1.0),
                };
                let span = (hi - lo).max(1e-12);
                let t = ((v - lo) / span * 255.0).clamp(0.0, 255.0) as usize;
                self.lut.unwrap_or([[0; 4]; 256])[t]
            }
            CompositeKind::Rgb => {
                let lo = self.vmin.as_ref().expect("rgb vmin");
                let hi = self.vmax.as_ref().expect("rgb vmax");
                if let Some(nd) = nodata {
                    for (i, v) in values.iter().enumerate().take(3) {
                        if i < nd.len() && (*v - nd[i]).abs() < 1e-9 {
                            return [0, 0, 0, 0];
                        }
                    }
                }
                let mut out = [0u8; 4];
                for i in 0..3 {
                    let span = (hi[i] - lo[i]).max(1e-12);
                    let t = ((values[i] - lo[i]) / span).clamp(0.0, 1.0);
                    out[i] = (t * 255.0) as u8;
                }
                out[3] = 255;
                out
            }
        }
    }
}

impl Composite {
    pub fn from_definition(
        def: &ColourDefinition,
        vmin: Option<Vec<f64>>,
        vmax: Option<Vec<f64>>,
    ) -> AgResult<Self> {
        Ok(match def {
            ColourDefinition::Named(name) => Self {
                vmin,
                vmax,
                lut: Some(builtin_lut(name)?),
                discrete: None,
                kind: CompositeKind::Gradient,
                bands: 1,
            },
            ColourDefinition::Colours(cs) => {
                let n = cs.len().max(1) - 1;
                let stops: Vec<(f64, Colour)> = cs
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (i as f64 / n.max(1) as f64, c.clone()))
                    .collect();
                Self {
                    vmin,
                    vmax,
                    lut: Some(lut_from_stops(&stops)?),
                    discrete: None,
                    kind: CompositeKind::Gradient,
                    bands: 1,
                }
            }
            ColourDefinition::ColoursAndBreaks(cb) => {
                let (lo, hi) = (
                    cb.first().map(|(p, _)| *p).unwrap_or(0.0),
                    cb.last().map(|(p, _)| *p).unwrap_or(1.0),
                );
                let span = (hi - lo).max(1e-12);
                let stops: Vec<(f64, Colour)> =
                    cb.iter().map(|(p, c)| ((*p - lo) / span, c.clone())).collect();
                Self {
                    vmin: Some(vec![lo]),
                    vmax: Some(vec![hi]),
                    lut: Some(lut_from_stops(&stops)?),
                    discrete: None,
                    kind: CompositeKind::Gradient,
                    bands: 1,
                }
            }
            ColourDefinition::Discrete(pairs) => {
                let map: HashMap<i64, [u8; 4]> =
                    pairs.iter().map(|(k, c)| (*k, c.rgba_u8())).collect();
                Self {
                    vmin: None,
                    vmax: None,
                    lut: None,
                    discrete: Some(map),
                    kind: CompositeKind::Discrete,
                    bands: 1,
                }
            }
            ColourDefinition::Rgb(lo, hi) => Self {
                vmin: Some(lo.to_vec()),
                vmax: Some(hi.to_vec()),
                lut: None,
                discrete: None,
                kind: CompositeKind::Rgb,
                bands: 3,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradient_get_endpoints() {
        let comp = Composite::from_definition(
            &ColourDefinition::Named("viridis".into()),
            Some(vec![0.0]),
            Some(vec![1.0]),
        )
        .unwrap();
        let lo = comp.get(&[0.0], None);
        let hi = comp.get(&[1.0], None);
        assert!(lo[0] < 100 && lo[1] < 40);
        assert!(hi[0] > 200 && hi[1] > 200);
    }

    #[test]
    fn discrete_unmapped_is_transparent() {
        let comp = Composite::from_definition(
            &ColourDefinition::Discrete(vec![(1, Colour::Hex("#FF0000".into()))]),
            None,
            None,
        )
        .unwrap();
        assert_eq!(comp.get(&[1.0], None), [255, 0, 0, 255]);
        assert_eq!(comp.get(&[2.0], None), [0, 0, 0, 0]);
    }

    #[test]
    fn rgb_clamps_and_packs() {
        let comp = Composite::from_definition(
            &ColourDefinition::Rgb([0.0, 0.0, 0.0], [100.0, 100.0, 100.0]),
            None,
            None,
        )
        .unwrap();
        let out = comp.get(&[50.0, 100.0, 200.0], None);
        assert_eq!(out, [127, 255, 255, 255]);
    }

    #[test]
    fn nodata_makes_transparent() {
        let comp = Composite::from_definition(
            &ColourDefinition::Named("viridis".into()),
            Some(vec![0.0]),
            Some(vec![1.0]),
        )
        .unwrap();
        assert_eq!(comp.get(&[-9999.0], Some(&[-9999.0])), [0, 0, 0, 0]);
    }
}
