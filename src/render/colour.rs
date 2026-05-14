//! `Colour` — RGBA in `[0,1]` or hex string. Ported from map-engine.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub enum Colour {
    Seq([f64; 4]),
    Hex(String),
}

impl Colour {
    pub fn rgba(&self) -> [f64; 4] {
        match self {
            Self::Seq(c) => *c,
            Self::Hex(s) => parse_hex(s).unwrap_or([0.0, 0.0, 0.0, 0.0]),
        }
    }

    pub fn rgba_u8(&self) -> [u8; 4] {
        let c = self.rgba();
        [
            (c[0].clamp(0.0, 1.0) * 255.0) as u8,
            (c[1].clamp(0.0, 1.0) * 255.0) as u8,
            (c[2].clamp(0.0, 1.0) * 255.0) as u8,
            (c[3].clamp(0.0, 1.0) * 255.0) as u8,
        ]
    }
}

fn parse_hex(s: &str) -> Option<[f64; 4]> {
    let s = s.trim_start_matches('#');
    let (r, g, b, a) = match s.len() {
        6 => (
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
            255u8,
        ),
        8 => (
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
            u8::from_str_radix(&s[6..8], 16).ok()?,
        ),
        _ => return None,
    };
    Some([
        r as f64 / 255.0,
        g as f64 / 255.0,
        b as f64 / 255.0,
        a as f64 / 255.0,
    ])
}

impl<'de> Deserialize<'de> for Colour {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Hex(String),
            Components(Vec<f64>),
        }
        match Raw::deserialize(d)? {
            Raw::Hex(s) => Ok(Colour::Hex(s)),
            Raw::Components(v) => {
                let max = v.iter().cloned().fold(0.0_f64, f64::max);
                let div = if max > 1.0 { 255.0 } else { 1.0 };
                let mut c = [0.0; 4];
                c[3] = 1.0;
                for (i, x) in v.into_iter().take(4).enumerate() {
                    c[i] = x / div;
                }
                Ok(Colour::Seq(c))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_rgb() {
        let c = Colour::Hex("#FF8040".into());
        let [r, g, b, a] = c.rgba_u8();
        assert_eq!((r, g, b, a), (255, 128, 64, 255));
    }

    #[test]
    fn parse_hex_rgba() {
        let c = Colour::Hex("#FF804080".into());
        let [_, _, _, a] = c.rgba_u8();
        assert_eq!(a, 128);
    }

    #[test]
    fn deserialise_components_0_to_255() {
        let c: Colour = serde_json::from_str("[255, 128, 64]").unwrap();
        assert_eq!(c.rgba_u8(), [255, 128, 64, 255]);
    }

    #[test]
    fn deserialise_components_0_to_1() {
        let c: Colour = serde_json::from_str("[1.0, 0.5, 0.25]").unwrap();
        let [r, g, b, _] = c.rgba_u8();
        assert!((r as i32 - 255).abs() <= 1);
        assert!((g as i32 - 127).abs() <= 1);
        assert!((b as i32 - 63).abs() <= 1);
    }
}
