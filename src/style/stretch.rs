//! Raster Style transfer functions; statistics are resolved on a bound shared grid.
use super::raster::normalize;
use crate::error::{AgError, AgResult};
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(crate) enum Transfer {
    Empty,
    Range(f64, f64),
    Curve(Arc<[(f64, f64)]>),
}

impl Transfer {
    pub(crate) fn apply(&self, x: f64, transparent: bool) -> Option<f64> {
        match self {
            Self::Empty => None,
            Self::Range(lo, hi) => {
                if transparent && (x < *lo || x > *hi) {
                    return None;
                }
                Some(if lo == hi && x == *lo {
                    0.5
                } else {
                    normalize(x, *lo, *hi)
                })
            }
            Self::Curve(points) => {
                let first = points[0];
                let last = points[points.len() - 1];
                if transparent && (x < first.0 || x > last.0) {
                    return None;
                }
                if x <= first.0 {
                    return Some(first.1);
                }
                if x >= last.0 {
                    return Some(last.1);
                }
                let i = points.partition_point(|p| p.0 <= x);
                let (a, b) = (points[i - 1], points[i]);
                Some(lerp(a.1, b.1, normalize(x, a.0, b.0)))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Stretch {
    Resolved(Vec<Transfer>),
    MinMax,
    Percentile([f64; 2]),
    StdDev(f64),
    Histogram,
}

impl Stretch {
    pub(crate) fn parse(v: &Value, channels: usize) -> Self {
        let pair = |p: &Value| {
            (
                p[0].as_f64().expect("validated number"),
                p[1].as_f64().expect("validated number"),
            )
        };
        let mut transfers = match v["method"].as_str().unwrap_or("none") {
            "none" => vec![Transfer::Range(0., 255.)],
            "linear" => v["rescale"]
                .as_array()
                .expect("validated ranges")
                .iter()
                .map(|p| {
                    let (a, b) = pair(p);
                    Transfer::Range(a, b)
                })
                .collect(),
            "curve" => v["curves"]
                .as_array()
                .expect("validated curves")
                .iter()
                .map(|c| {
                    Transfer::Curve(
                        c.as_array()
                            .expect("validated points")
                            .iter()
                            .map(pair)
                            .collect(),
                    )
                })
                .collect(),
            "minmax" => return Self::MinMax,
            "percentile" => {
                let (a, b) = pair(&v["percentiles"]);
                return Self::Percentile([a, b]);
            }
            "stddev" => return Self::StdDev(v["stddev"].as_f64().expect("validated deviation")),
            "histogram_equalization" => return Self::Histogram,
            _ => unreachable!("SDK-validated stretch"),
        };
        if transfers.len() == 1 {
            transfers.resize(channels, transfers[0].clone());
        }
        Self::Resolved(transfers)
    }

    pub(crate) fn needs_statistics(&self) -> bool {
        !matches!(self, Self::Resolved(_))
    }

    pub(crate) fn transfers(&self) -> AgResult<&[Transfer]> {
        match self {
            Self::Resolved(v) => Ok(v),
            _ => Err(AgError::query(
                "statistics",
                "a bound dataset or mosaic statistics grid is required; use Tiler or MosaicTiler",
            )),
        }
    }

    pub(crate) fn resolve(&self, mut values: Vec<f64>) -> AgResult<Transfer> {
        if values.is_empty() {
            return Ok(Transfer::Empty);
        }
        values.sort_unstable_by(f64::total_cmp);
        let first = values[0];
        let last = values[values.len() - 1];
        Ok(match self {
            Self::MinMax => Transfer::Range(first, last),
            Self::Percentile([lo, hi]) => {
                let quantile = |p: f64| {
                    let h = (values.len() - 1) as f64 * (p / 100.);
                    let a = h.floor() as usize;
                    let b = h.ceil() as usize;
                    lerp(values[a], values[b], h - a as f64)
                };
                Transfer::Range(quantile(*lo), quantile(*hi))
            }
            Self::StdDev(k) => {
                // Scaled population variance avoids overflow in sums/squares.
                let scale = first.abs().max(last.abs());
                if scale == 0. {
                    return Ok(Transfer::Range(0., 0.));
                }
                let n = values.len() as f64;
                let mean = compensated_sum(values.iter().map(|x| x / scale)) / n;
                let deviation =
                    (compensated_sum(values.iter().map(|x| (x / scale - mean).powi(2))) / n).sqrt();
                let (lo, hi) = (
                    (mean - k * deviation) * scale,
                    (mean + k * deviation) * scale,
                );
                if !lo.is_finite() || !hi.is_finite() {
                    return Err(AgError::query(
                        "statistics",
                        "standard deviation range is not representable as finite f64",
                    ));
                }
                Transfer::Range(lo, hi)
            }
            Self::Histogram => {
                if first == last {
                    return Ok(Transfer::Range(first, last));
                }
                let initial = values.partition_point(|v| *v == first);
                let denominator = (values.len() - initial) as f64;
                let mut points = Vec::new();
                let mut i = 0;
                while i < values.len() {
                    let x = values[i];
                    let end = i + values[i..].partition_point(|v| *v == x);
                    points.push((x, (end - initial) as f64 / denominator));
                    i = end;
                }
                Transfer::Curve(points.into())
            }
            Self::Resolved(_) => {
                return Err(AgError::query(
                    "statistics",
                    "stretch does not consume statistics",
                ))
            }
        })
    }
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    if t == 0. {
        a
    } else if t == 1. {
        b
    } else if a.signum() == b.signum() {
        a + (b - a) * t
    } else {
        a * (1. - t) + b * t
    }
}
fn compensated_sum(values: impl Iterator<Item = f64>) -> f64 {
    let (mut sum, mut correction) = (0., 0.);
    for x in values {
        let y = x - correction;
        let next = sum + y;
        correction = (next - sum) - y;
        sum = next;
    }
    sum
}

#[derive(Debug, Clone)]
pub(crate) struct Statistics {
    pub(crate) scope: Option<String>,
    pub(crate) sample_size: Option<usize>,
}
impl Statistics {
    pub(crate) fn parse(v: &Value) -> AgResult<Self> {
        if v.get("ref").is_some() {
            return Err(AgError::query(
                "statistics.ref",
                "external statistics snapshots are not supported",
            ));
        }
        if v["scope"] == "viewport" {
            return Err(AgError::query(
                "statistics.scope",
                "viewport statistics require a bound shared view grid",
            ));
        }
        Ok(Self {
            scope: v["scope"].as_str().map(str::to_owned),
            sample_size: if v["accuracy"] == "exact" {
                None
            } else {
                Some(v["sample_size"].as_u64().unwrap_or(1_000_000) as usize)
            },
        })
    }
    pub(crate) fn check_scope(&self, mosaic: bool) -> AgResult<()> {
        let expected = if mosaic { "mosaic" } else { "dataset" };
        if self.scope.as_deref().is_some_and(|s| s != expected) {
            return Err(AgError::query(
                "statistics.scope",
                format!("this source requires {expected} statistics"),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reference_statistics_curves_and_degenerate_ranges() {
        let samples = vec![0., 0., 10., 20.];
        for (stretch, expected) in [
            (Stretch::MinMax, 0.5),
            (Stretch::Percentile([25., 75.]), 0.8),
            (Stretch::Histogram, 0.5),
        ] {
            let t = stretch.resolve(samples.clone()).unwrap();
            assert!((t.apply(10., false).unwrap() - expected).abs() < 1e-12);
            assert_eq!(t.apply(-1., true), None);
        }
        let t = Stretch::StdDev(1.).resolve(vec![0., 2.]).unwrap();
        assert_eq!(t.apply(1., false), Some(0.5));
        for s in [
            Stretch::MinMax,
            Stretch::Percentile([2., 98.]),
            Stretch::StdDev(2.),
            Stretch::Histogram,
        ] {
            let t = s.resolve(vec![7.; 3]).unwrap();
            assert_eq!(t.apply(7., true), Some(0.5));
            assert_eq!(t.apply(6., true), None);
            assert_eq!(t.apply(8., false), Some(1.));
            assert_eq!(s.resolve(vec![]).unwrap().apply(7., false), None);
        }
        let t = Transfer::Curve(vec![(0., 0.2), (10., 0.8), (20., 1.)].into());
        assert_eq!(t.apply(-1., false), Some(0.2));
        assert!((t.apply(5., false).unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(t.apply(21., true), None);
    }
    #[test]
    fn extreme_statistics_are_finite_or_explicit_errors() {
        let t = Stretch::Percentile([25., 75.])
            .resolve(vec![-f64::MAX, f64::MAX])
            .unwrap();
        assert_eq!(t.apply(0., false), Some(0.5));
        assert!(Stretch::StdDev(2.)
            .resolve(vec![-f64::MAX, f64::MAX])
            .is_err());
        let t = Stretch::StdDev(1.).resolve(vec![f64::MAX; 3]).unwrap();
        assert_eq!(t.apply(f64::MAX, false), Some(0.5));
    }
}
