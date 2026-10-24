//! Built-in colormap LUTs used by the Raster Style adapter.

use crate::error::{AgError, AgResult};

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

/// Build an opaque 256-entry color map, matching its name case-insensitively.
///
/// Supports viridis, magma, inferno, plasma, cividis, turbo, greys and terrain.
/// Unknown names return [`AgError::UnknownColormap`].
pub fn builtin_lut(name: &str) -> AgResult<Lut> {
    if name.eq_ignore_ascii_case("terrain") {
        return Ok(std::array::from_fn(|i| {
            let t = i as f64 / 255.0;
            let upper = TERRAIN_STOPS.iter().position(|(p, _)| *p >= t).unwrap();
            let (lo, a) = TERRAIN_STOPS[upper.saturating_sub(1)];
            let (hi, b) = TERRAIN_STOPS[upper];
            let mix = ((t - lo) / (hi - lo).max(1e-12)).clamp(0.0, 1.0);
            std::array::from_fn(|c| ((a[c] * (1.0 - mix) + b[c] * mix) * 255.0) as u8)
        }));
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
    fn terrain_low_is_blue_high_is_white() {
        let lut = builtin_lut("terrain").unwrap();
        let [r0, g0, b0, a0] = lut[0];
        // Low end is deep blue-ish: blue channel dominant, opaque.
        assert!(b0 > 120 && b0 > r0 && b0 > g0, "low={:?}", [r0, g0, b0]);
        assert_eq!(a0, 255);
        // High end is near-white.
        let [r255, g255, b255, _] = lut[255];
        assert!(
            r255 > 240 && g255 > 240 && b255 > 240,
            "high={:?}",
            [r255, g255, b255]
        );
    }

    #[test]
    fn terrain_is_case_insensitive() {
        assert_eq!(
            builtin_lut("Terrain").unwrap(),
            builtin_lut("terrain").unwrap()
        );
    }
}
