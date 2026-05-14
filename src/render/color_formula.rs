//! rio-color formula subset: `gamma BANDS V`, `sigmoidal BANDS β α`, `saturation P`.
//!
//! Applied left-to-right in sRGB. Saturation goes via Lab/Lch (matches rio-color).

use bitflags::bitflags;
use nom::{
    branch::alt,
    bytes::complete::tag_no_case,
    character::complete::{multispace0, multispace1},
    combinator::recognize,
    multi::{many1, many1_count},
    number::complete::double,
    sequence::preceded,
    IResult,
};

use crate::error::{AgError, AgResult};

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Gamma {
        bands: Bands,
        value: f64,
    },
    Sigmoidal {
        bands: Bands,
        contrast: f64,
        bias: f64,
    },
    Saturation {
        factor: f64,
    },
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Bands: u8 {
        const R = 0b001;
        const G = 0b010;
        const B = 0b100;
    }
}

pub fn parse(input: &str) -> AgResult<Vec<Op>> {
    let s = input.replace(',', " ");
    let (rest, ops) = many1(preceded(multispace0, op))(s.as_str())
        .map_err(|e| AgError::ColorFormula(format!("parse: {e}")))?;
    if !rest.trim().is_empty() {
        return Err(AgError::ColorFormula(format!(
            "trailing: {:?}",
            rest.trim()
        )));
    }
    Ok(ops)
}

fn op(input: &str) -> IResult<&str, Op> {
    alt((gamma, sigmoidal, saturation))(input)
}

fn gamma(input: &str) -> IResult<&str, Op> {
    let (i, _) = tag_no_case("gamma")(input)?;
    let (i, _) = multispace1(i)?;
    let (i, b) = bands(i)?;
    let (i, _) = multispace1(i)?;
    let (i, v) = double(i)?;
    Ok((i, Op::Gamma { bands: b, value: v }))
}

fn sigmoidal(input: &str) -> IResult<&str, Op> {
    let (i, _) = tag_no_case("sigmoidal")(input)?;
    let (i, _) = multispace1(i)?;
    let (i, b) = bands(i)?;
    let (i, _) = multispace1(i)?;
    let (i, c) = double(i)?;
    let (i, _) = multispace1(i)?;
    let (i, bi) = double(i)?;
    Ok((
        i,
        Op::Sigmoidal {
            bands: b,
            contrast: c,
            bias: bi,
        },
    ))
}

fn saturation(input: &str) -> IResult<&str, Op> {
    let (i, _) = tag_no_case("saturation")(input)?;
    let (i, _) = multispace1(i)?;
    let (i, v) = double(i)?;
    Ok((i, Op::Saturation { factor: v }))
}

fn bands(input: &str) -> IResult<&str, Bands> {
    let (i, raw) = recognize(many1_count(nom::character::complete::satisfy(|c| {
        matches!(c, 'r' | 'g' | 'b' | 'R' | 'G' | 'B' | '1' | '2' | '3')
    })))(input)?;
    let mut bs = Bands::empty();
    for ch in raw.chars() {
        match ch.to_ascii_lowercase() {
            'r' | '1' => bs |= Bands::R,
            'g' | '2' => bs |= Bands::G,
            'b' | '3' => bs |= Bands::B,
            _ => {}
        }
    }
    Ok((i, bs))
}

/// Apply ops to a single sRGB pixel (r, g, b) in [0, 1].
pub fn apply(ops: &[Op], r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let (mut r, mut g, mut b) = (r, g, b);
    for op in ops {
        match *op {
            Op::Gamma { bands, value } => {
                let g_inv = 1.0 / value;
                if bands.contains(Bands::R) {
                    r = r.max(0.0).powf(g_inv);
                }
                if bands.contains(Bands::G) {
                    g = g.max(0.0).powf(g_inv);
                }
                if bands.contains(Bands::B) {
                    b = b.max(0.0).powf(g_inv);
                }
            }
            Op::Sigmoidal {
                bands,
                contrast,
                bias,
            } => {
                if contrast == 0.0 {
                    continue;
                }
                let f = |x: f64| sigmoid_norm(x, contrast, bias);
                if bands.contains(Bands::R) {
                    r = f(r);
                }
                if bands.contains(Bands::G) {
                    g = f(g);
                }
                if bands.contains(Bands::B) {
                    b = f(b);
                }
            }
            Op::Saturation { factor } => {
                use palette::{FromColor, IntoColor, Lch, Srgb};
                let srgb = Srgb::new(r as f32, g as f32, b as f32);
                let mut lch: Lch = Lch::from_color(srgb);
                lch.chroma *= factor as f32;
                let back: Srgb = lch.into_color();
                r = back.red.clamp(0.0, 1.0) as f64;
                g = back.green.clamp(0.0, 1.0) as f64;
                b = back.blue.clamp(0.0, 1.0) as f64;
            }
        }
    }
    (r, g, b)
}

fn sigmoid_norm(x: f64, beta: f64, alpha: f64) -> f64 {
    let s = |v: f64| 1.0 / (1.0 + (beta * (alpha - v)).exp());
    let s0 = s(0.0);
    let s1 = s(1.0);
    ((s(x) - s0) / (s1 - s0)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn parse_gamma_only() {
        let ops = parse("gamma RGB 1.5").unwrap();
        assert_eq!(
            ops,
            vec![Op::Gamma {
                bands: Bands::R | Bands::G | Bands::B,
                value: 1.5,
            }]
        );
    }

    #[test]
    fn parse_chain() {
        let ops = parse("gamma RG 1.2 sigmoidal B 5 0.5 saturation 1.3").unwrap();
        assert_eq!(ops.len(), 3);
    }

    #[test]
    fn rio_color_gamma_regression() {
        let (r, _, _) = apply(
            &parse("gamma rgb 0.95").unwrap(),
            10.0 / 255.0,
            50.0 / 255.0,
            100.0 / 255.0,
        );
        assert_relative_eq!(r, 0.033069782, epsilon = 1e-4);
    }

    #[test]
    fn rio_color_sigmoidal_regression() {
        // Math-derived expected for β=5, α=0.53, x=10/255:
        //   s(x)  = 1/(1+exp(5*(0.53 - 0.03922))) ≈ 0.07924
        //   s(0)  = 1/(1+exp(2.65))               ≈ 0.06604
        //   s(1)  = 1/(1+exp(-2.35))              ≈ 0.91293
        //   norm  = (s(x)-s(0)) / (s(1)-s(0))     ≈ 0.01554
        let (r, _, _) = apply(
            &parse("sigmoidal rgb 5 0.53").unwrap(),
            10.0 / 255.0,
            50.0 / 255.0,
            100.0 / 255.0,
        );
        assert_relative_eq!(r, 0.015542, epsilon = 1e-4);
    }

    #[test]
    fn sigmoidal_zero_contrast_is_identity() {
        let (r, g, b) = apply(
            &[Op::Sigmoidal {
                bands: Bands::R | Bands::G | Bands::B,
                contrast: 0.0,
                bias: 0.5,
            }],
            0.3,
            0.5,
            0.7,
        );
        assert_relative_eq!(r, 0.3);
        assert_relative_eq!(g, 0.5);
        assert_relative_eq!(b, 0.7);
    }
}
