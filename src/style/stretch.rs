//! QGIS-style stretch modes. `Custom` is represented by a non-empty
//! `TileStyle.rescale`; this module covers the computed modes and the
//! default (StdDev k=2.0) applied when the client passes nothing.

use crate::error::{AgError, AgResult};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StretchSpec {
    MinMax,
    Percent { lo: f64, hi: f64 },
    StdDev { k: f64 },
}

impl StretchSpec {
    /// The default applied when no `rescale` and no `stretch` are given
    /// (caller decides whether the dtype is eligible — u8 keeps its
    /// fast path and never reaches here).
    pub const DEFAULT: StretchSpec = StretchSpec::StdDev { k: 2.0 };
}

/// Build a `StretchSpec` from already-parsed raw params. `kind` is the
/// `stretch=` value; `pc`/`sigma` are the optional companion params.
/// Invalid input → `AgError::query` (maps to HTTP 400).
pub fn parse_stretch_kind(
    kind: &str,
    pc: Option<(f64, f64)>,
    sigma: Option<f64>,
) -> AgResult<StretchSpec> {
    match kind.to_ascii_lowercase().as_str() {
        "minmax" => Ok(StretchSpec::MinMax),
        "percent" => {
            let (lo, hi) = pc.unwrap_or((2.0, 98.0));
            if !(0.0..=100.0).contains(&lo) || !(0.0..=100.0).contains(&hi) || lo >= hi {
                return Err(AgError::query("pc", format!("invalid percentile {lo},{hi}")));
            }
            Ok(StretchSpec::Percent { lo, hi })
        }
        "stddev" => {
            let k = sigma.unwrap_or(2.0);
            if !(k > 0.0) {
                return Err(AgError::query("sigma", format!("must be > 0, got {k}")));
            }
            Ok(StretchSpec::StdDev { k })
        }
        other => Err(AgError::query("stretch", format!("unknown mode {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_three_modes_case_insensitive() {
        assert_eq!(parse_stretch_kind("MinMax", None, None).unwrap(), StretchSpec::MinMax);
        assert_eq!(
            parse_stretch_kind("percent", Some((1.0, 99.0)), None).unwrap(),
            StretchSpec::Percent { lo: 1.0, hi: 99.0 }
        );
        assert_eq!(
            parse_stretch_kind("STDDEV", None, Some(3.0)).unwrap(),
            StretchSpec::StdDev { k: 3.0 }
        );
    }

    #[test]
    fn percent_and_stddev_defaults() {
        assert_eq!(
            parse_stretch_kind("percent", None, None).unwrap(),
            StretchSpec::Percent { lo: 2.0, hi: 98.0 }
        );
        assert_eq!(
            parse_stretch_kind("stddev", None, None).unwrap(),
            StretchSpec::StdDev { k: 2.0 }
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_stretch_kind("bogus", None, None).is_err());
        assert!(parse_stretch_kind("percent", Some((90.0, 10.0)), None).is_err());
        assert!(parse_stretch_kind("percent", Some((-1.0, 50.0)), None).is_err());
        assert!(parse_stretch_kind("stddev", None, Some(0.0)).is_err());
    }
}
