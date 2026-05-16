//! Dataset band statistics: embedded `STATISTICS_*` tags (exact) or a
//! one-shot top-overview computation (estimated). See
//! docs/superpowers/specs/2026-05-16-non-uint8-data-stretch-design.md.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Provenance {
    Exact,
    Estimated,
}

/// Equal-width histogram over `[min, max]`. `counts.len()` is the bin count.
#[derive(Debug, Clone)]
pub struct Histogram {
    pub min: f64,
    pub max: f64,
    pub counts: Vec<u64>,
}

impl Histogram {
    /// Value at percentile `p` (0..=100) via linear interpolation on the
    /// cumulative count. Robust to the overview-smoothing approximation
    /// (see spec accuracy table). Empty / degenerate → `min`.
    pub fn quantile(&self, p: f64) -> f64 {
        let total: u64 = self.counts.iter().sum();
        if total == 0 || self.max <= self.min || self.counts.is_empty() {
            return self.min;
        }
        let target = (p / 100.0) * total as f64;
        let bin_w = (self.max - self.min) / self.counts.len() as f64;
        let mut cum = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            let next = cum + c;
            if (next as f64) >= target {
                // Fraction into this bin.
                let into = if c == 0 {
                    0.0
                } else {
                    (target - cum as f64) / c as f64
                };
                return self.min + (i as f64 + into) * bin_w;
            }
            cum = next;
        }
        self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantile_linear_interpolates() {
        // 4 bins over [0,4], 10 counts each. p=50 → midpoint = 2.0.
        let h = Histogram { min: 0.0, max: 4.0, counts: vec![10, 10, 10, 10] };
        assert!((h.quantile(50.0) - 2.0).abs() < 1e-9);
        assert!((h.quantile(0.0) - 0.0).abs() < 1e-9);
        assert!((h.quantile(100.0) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn quantile_skewed() {
        // Mass concentrated in first bin: p=2 stays near min.
        let h = Histogram { min: 0.0, max: 100.0, counts: vec![98, 1, 1, 0] };
        assert!(h.quantile(2.0) < 25.0);
        assert!(h.quantile(98.0) >= 25.0);
    }

    #[test]
    fn quantile_degenerate_returns_min() {
        let h = Histogram { min: 5.0, max: 5.0, counts: vec![0, 0] };
        assert_eq!(h.quantile(50.0), 5.0);
    }
}
