//! Colormap LUTs (colorous builtins + user-supplied stops).

use crate::error::{AgError, AgResult};
use crate::render::colour::Colour;

/// 256-entry RGBA lookup table.
pub type Lut = [[u8; 4]; 256];

const BUILTINS: &[(&str, &colorous::Gradient)] = &[
    ("viridis", &colorous::VIRIDIS),
    ("magma", &colorous::MAGMA),
    ("inferno", &colorous::INFERNO),
    ("plasma", &colorous::PLASMA),
    ("cividis", &colorous::CIVIDIS),
    ("turbo", &colorous::TURBO),
    ("greys", &colorous::GREYS),
];

pub fn builtin_lut(name: &str) -> AgResult<Lut> {
    let g = BUILTINS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, g)| *g)
        .ok_or_else(|| AgError::UnknownColormap(name.into()))?;
    let mut lut = [[0u8; 4]; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        let c = g.eval_rational(i, 255);
        *slot = [c.r, c.g, c.b, 255];
    }
    Ok(lut)
}

/// Build a LUT by linearly interpolating between (position-in-[0,1], Colour) stops.
pub fn lut_from_stops(stops: &[(f64, Colour)]) -> AgResult<Lut> {
    if stops.len() < 2 {
        return Err(AgError::UnknownColormap("need >=2 stops".into()));
    }
    let mut s = stops.to_vec();
    s.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut lut = [[0u8; 4]; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        let t = i as f64 / 255.0;
        let upper_idx = s.iter().position(|(p, _)| *p >= t).unwrap_or(s.len() - 1);
        let lower_idx = if upper_idx == 0 { 0 } else { upper_idx - 1 };
        let a = &s[lower_idx];
        let b = &s[upper_idx];
        let span = (b.0 - a.0).max(1e-12);
        let mix = ((t - a.0) / span).clamp(0.0, 1.0);
        let ca = a.1.rgba();
        let cb = b.1.rgba();
        *slot = [
            ((ca[0] * (1.0 - mix) + cb[0] * mix) * 255.0) as u8,
            ((ca[1] * (1.0 - mix) + cb[1] * mix) * 255.0) as u8,
            ((ca[2] * (1.0 - mix) + cb[2] * mix) * 255.0) as u8,
            ((ca[3] * (1.0 - mix) + cb[3] * mix) * 255.0) as u8,
        ];
    }
    Ok(lut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viridis_endpoints_are_purple_yellow() {
        let lut = builtin_lut("viridis").unwrap();
        let [r0, g0, _, _] = lut[0];
        let [r255, g255, _, _] = lut[255];
        assert!(r0 < 100 && g0 < 40);
        assert!(r255 > 200 && g255 > 200);
    }

    #[test]
    fn unknown_colormap_errors() {
        assert!(builtin_lut("not-a-thing").is_err());
    }

    #[test]
    fn stops_lut_linear_red_to_blue() {
        let lut = lut_from_stops(&[
            (0.0, Colour::Hex("#FF0000".into())),
            (1.0, Colour::Hex("#0000FF".into())),
        ])
        .unwrap();
        let mid = lut[127];
        assert!(mid[0] > 100 && mid[0] < 160);
        assert!(mid[2] > 100 && mid[2] < 160);
    }
}
