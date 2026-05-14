//! Overview level selection by target GSD.

use async_tiff::TIFF;

/// Pick the IFD index whose horizontal GSD is the largest still ≤ `target_gsd`.
/// Falls back to the highest-resolution IFD (index 0) if none qualifies.
pub fn pick_overview(tiff: &TIFF, full_width: u64, full_gsd: f64, target_gsd: f64) -> usize {
    let ifds = tiff.ifds();
    let mut best: usize = 0;
    let mut best_gsd: f64 = full_gsd;
    for (i, ifd) in ifds.iter().enumerate() {
        let w = ifd.image_width() as f64;
        let gsd = full_gsd * (full_width as f64 / w);
        if gsd <= target_gsd && gsd >= best_gsd {
            best = i;
            best_gsd = gsd;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    fn pick_pure(widths: &[u64], full_w: u64, full_gsd: f64, target: f64) -> usize {
        let mut best = 0usize;
        let mut best_gsd = full_gsd;
        for (i, &w) in widths.iter().enumerate() {
            let gsd = full_gsd * (full_w as f64 / w as f64);
            if gsd <= target && gsd >= best_gsd {
                best = i;
                best_gsd = gsd;
            }
        }
        best
    }

    #[test]
    fn picks_largest_gsd_within_target() {
        // full=10000@1m, overviews 5000@2m, 2500@4m, 1250@8m
        // target 5m → pick the 4m overview (index 2)
        let idx = pick_pure(&[10000, 5000, 2500, 1250], 10000, 1.0, 5.0);
        assert_eq!(idx, 2);
    }

    #[test]
    fn falls_back_to_full_when_all_too_coarse() {
        let idx = pick_pure(&[10000, 5000], 10000, 1.0, 0.5);
        assert_eq!(idx, 0);
    }
}
