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

/// Matplotlib `terrain` stops: deep blue → cyan → green → sand → brown → white.
/// Not part of the `colorous` set, so built from explicit stops.
const TERRAIN_STOPS: &[(f64, [f64; 4])] = &[
    (0.00, [0.2, 0.2, 0.6, 1.0]),
    (0.15, [0.0, 0.6, 1.0, 1.0]),
    (0.25, [0.0, 0.8, 0.4, 1.0]),
    (0.50, [1.0, 1.0, 0.6, 1.0]),
    (0.75, [0.5, 0.36, 0.33, 1.0]),
    (1.00, [1.0, 1.0, 1.0, 1.0]),
];

pub fn builtin_lut(name: &str) -> AgResult<Lut> {
    if name.eq_ignore_ascii_case("terrain") {
        let stops: Vec<(f64, Colour)> = TERRAIN_STOPS
            .iter()
            .map(|(p, c)| (*p, Colour::Seq(*c)))
            .collect();
        return lut_from_stops(&stops);
    }
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
    // `total_cmp` is NaN-safe — a stray NaN stop position must not panic
    // the render pipeline (it would take down the tile handler).
    s.sort_by(|a, b| a.0.total_cmp(&b.0));
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

    #[test]
    fn terrain_low_is_blue_high_is_white() {
        let lut = builtin_lut("terrain").unwrap();
        let [r0, g0, b0, a0] = lut[0];
        // Low end is deep blue-ish: blue channel dominant, opaque.
        assert!(b0 > 120 && b0 > r0 && b0 > g0, "low={:?}", [r0, g0, b0]);
        assert_eq!(a0, 255);
        // High end is near-white.
        let [r255, g255, b255, _] = lut[255];
        assert!(r255 > 240 && g255 > 240 && b255 > 240, "high={:?}", [r255, g255, b255]);
    }

    #[test]
    fn terrain_is_case_insensitive() {
        assert_eq!(
            builtin_lut("Terrain").unwrap(),
            builtin_lut("terrain").unwrap()
        );
    }
}
