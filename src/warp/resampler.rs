//! Resampler kernels for warp interpolation.

use ndarray::ArrayView2;

/// Sampling kernels used when reading or reprojecting a raster grid.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Resampling {
    /// Select the closest source pixel without interpolating its value.
    #[default]
    Nearest,
    /// Interpolate the four surrounding pixels with linear weights.
    Bilinear,
    /// Catmull-Rom bicubic (4-tap). Sharper than bilinear, sharper than
    /// Mitchell, default for most XYZ tile servers.
    Cubic,
    /// Cubic B-spline convolution (4-tap, B=1, C=0).
    CubicSpline,
    /// Lanczos3 windowed sinc (6-tap).
    Lanczos,
    /// Box-filter mean over the source footprint a destination pixel
    /// covers: correct averaging antialiasing when downsampling, but
    /// degrades to nearest-neighbour when both footprint axes are sub-pixel.
    Average,
    /// Majority (most-frequent) value over the source footprint a destination
    /// pixel covers — for categorical/classification rasters. Downsample-only:
    /// a sub-pixel footprint degrades to Nearest (never interpolates). Ties
    /// resolve to the lowest value.
    Mode,
}

impl Resampling {
    /// Parse a lowercase kernel name; aliases are `bicubic`, `cubicspline`, and `lanczos3`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "nearest" => Some(Self::Nearest),
            "bilinear" => Some(Self::Bilinear),
            "cubic" | "bicubic" => Some(Self::Cubic),
            "cubic_spline" | "cubicspline" => Some(Self::CubicSpline),
            "lanczos" | "lanczos3" => Some(Self::Lanczos),
            "average" => Some(Self::Average),
            "mode" => Some(Self::Mode),
            _ => None,
        }
    }

    /// Sample value at fractional (col, row) coordinates.
    ///
    /// At image edges the bilinear neighbours are clamped to the last valid
    /// index instead of returning `fill`. This prevents the "white seams"
    /// that appear between adjacent XYZ tiles when a sample lands on the
    /// last column/row of the read window.
    #[inline]
    pub fn sample(self, view: ArrayView2<f64>, col: f64, row: f64, fill: f64) -> f64 {
        self.sample_masked(view, None, col, row, fill)
    }

    /// Like [`Self::sample`] but skips invalid neighbours and renormalizes weights.
    /// Cubic/Lanczos retain signed weights, including negative total support.
    /// Zero total support or a non-finite result returns `fill`.
    #[inline]
    pub fn sample_masked(
        self,
        view: ArrayView2<f64>,
        valid: Option<ArrayView2<bool>>,
        col: f64,
        row: f64,
        fill: f64,
    ) -> f64 {
        let (h, w) = (view.shape()[0] as i64, view.shape()[1] as i64);
        if w == 0 || h == 0 {
            return fill;
        }
        if !col.is_finite() || !row.is_finite() {
            return fill;
        }
        if col < -0.5 || row < -0.5 || col > (w - 1) as f64 + 0.5 || row > (h - 1) as f64 + 0.5 {
            return fill;
        }
        let cf = col.clamp(0.0, (w - 1) as f64);
        let rf = row.clamp(0.0, (h - 1) as f64);

        // Fast path: a band view sliced out of a C-order `(bands,H,W)`
        // array is standard-contiguous, so `as_slice()` is `Some` and we
        // can index the flat buffer directly — one fused mul-add instead
        // of ndarray's per-access stride dot-product + bounds check. Falls
        // back to strided `[[r,c]]` indexing when the view is non-contiguous
        // (e.g. a `permuted_axes` zero-copy reorder).
        let w_us = w as usize;
        let contig = view.as_slice();
        let get = |r: usize, c: usize| -> f64 {
            match contig {
                Some(s) => {
                    debug_assert!(view.is_standard_layout());
                    // SAFETY: `as_slice()` requires standard contiguous layout,
                    // so physical row stride equals logical width. Callers clamp
                    // `r`/`c` to `[0, h-1]` / `[0, w-1]`, making `r*w_us + c`
                    // strictly less than `h*w == s.len()`.
                    unsafe { *s.get_unchecked(r * w_us + c) }
                }
                None => view[[r, c]],
            }
        };
        // NB: a matching flat-index fast path for the validity mask was
        // benchmarked and reverted — it produced 0.1% change because the
        // masked path is dominated by the reweighting branches + divide,
        // not the mask reads. Kept strided to avoid complexity for no gain.

        match self {
            Self::Nearest | Self::Mode | Self::Average => {
                let c = cf.round().clamp(0.0, (w - 1) as f64) as usize;
                let r = rf.round().clamp(0.0, (h - 1) as f64) as usize;
                if let Some(m) = valid {
                    if !m[[r, c]] {
                        return fill;
                    }
                }
                get(r, c)
            }
            Self::Cubic | Self::CubicSpline | Self::Lanczos => {
                let radius = if self == Self::Lanczos { 3 } else { 2 };
                let weight = |x: f64| {
                    let x = x.abs();
                    match self {
                        Self::Cubic if x < 1.0 => (1.5 * x - 2.5) * x * x + 1.0,
                        Self::Cubic if x < 2.0 => ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0,
                        Self::CubicSpline if x < 1.0 => (4.0 - 6.0 * x * x + 3.0 * x * x * x) / 6.0,
                        Self::CubicSpline if x < 2.0 => (2.0 - x).powi(3) / 6.0,
                        Self::Lanczos if x == 0.0 => 1.0,
                        // Keep sinc's integer zeros exact at masked pixel centers.
                        Self::Lanczos if x.fract() == 0.0 => 0.0,
                        Self::Lanczos if x < 3.0 => {
                            let p = std::f64::consts::PI * x;
                            (p.sin() / p) * ((p / 3.0).sin() / (p / 3.0))
                        }
                        _ => 0.0,
                    }
                };
                let samples = ((rf.floor() as i64 - radius + 1)..=(rf.floor() as i64 + radius))
                    .flat_map(|y| {
                        ((cf.floor() as i64 - radius + 1)..=(cf.floor() as i64 + radius))
                            .filter_map(move |x| {
                                let r = y.clamp(0, h - 1) as usize;
                                let c = x.clamp(0, w - 1) as usize;
                                if valid.is_some_and(|m| !m[[r, c]]) {
                                    return None;
                                }
                                let k = weight(cf - x as f64) * weight(rf - y as f64);
                                (k != 0.0).then(|| (get(r, c), k))
                            })
                    });
                convolution_mean(samples, fill)
            }
            Self::Bilinear => {
                let c0 = cf.floor() as usize;
                let r0 = rf.floor() as usize;
                let c1 = (c0 + 1).min(w as usize - 1);
                let r1 = (r0 + 1).min(h as usize - 1);
                let dc = cf - c0 as f64;
                let dr = rf - r0 as f64;

                let w00 = (1.0 - dc) * (1.0 - dr);
                let w01 = dc * (1.0 - dr);
                let w10 = (1.0 - dc) * dr;
                let w11 = dc * dr;
                let v00 = get(r0, c0);
                let v01 = get(r0, c1);
                let v10 = get(r1, c0);
                let v11 = get(r1, c1);

                match valid {
                    None => {
                        let top = v00 * (1.0 - dc) + v01 * dc;
                        let bot = v10 * (1.0 - dc) + v11 * dc;
                        top * (1.0 - dr) + bot * dr
                    }
                    Some(m) => {
                        // Reweighted bilinear: drop invalid neighbours,
                        // rescale remaining weights to sum to 1.
                        let (m00, m01, m10, m11) =
                            (m[[r0, c0]], m[[r0, c1]], m[[r1, c0]], m[[r1, c1]]);
                        let mut wsum = 0.0;
                        let mut vsum = 0.0;
                        if m00 {
                            wsum += w00;
                            vsum += w00 * v00;
                        }
                        if m01 {
                            wsum += w01;
                            vsum += w01 * v01;
                        }
                        if m10 {
                            wsum += w10;
                            vsum += w10 * v10;
                        }
                        if m11 {
                            wsum += w11;
                            vsum += w11 * v11;
                        }
                        if wsum > 0.0 {
                            vsum / wsum
                        } else {
                            fill
                        }
                    }
                }
            }
        }
    }

    /// Box-mean over the source footprint `[sc-rx, sc+rx] x [sr-ry, sr+ry]`.
    /// `rx`/`ry` are half-footprint sizes in source pixels (from the warp
    /// Jacobian). When the footprint is below one source pixel the result
    /// degenerates to nearest. Partial source pixels use overlap area. Averages only
    /// `valid` pixels; all-invalid (or empty box) returns `fill`.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn sample_average(
        self,
        view: ArrayView2<f64>,
        valid: Option<ArrayView2<bool>>,
        sc: f64,
        sr: f64,
        rx: f64,
        ry: f64,
        fill: f64,
    ) -> f64 {
        let (h, w) = (view.shape()[0], view.shape()[1]);
        if w == 0 || h == 0 {
            return fill;
        }
        // PROJ can emit NaN/inf for unmapped pixels; treat as no data.
        if !sc.is_finite() || !sr.is_finite() {
            return fill;
        }
        if !rx.is_finite() || !ry.is_finite() || rx <= 0.0 || ry <= 0.0 {
            return fill;
        }
        if rx < 0.5 && ry < 0.5 {
            return Self::Nearest.sample_masked(view, valid, sc, sr, fill);
        }
        let c_lo = (sc - rx + 0.5).floor().max(0.0) as usize;
        let c_hi = ((sc + rx + 0.5).ceil() as i64).clamp(0, w as i64) as usize;
        let r_lo = (sr - ry + 0.5).floor().max(0.0) as usize;
        let r_hi = ((sr + ry + 0.5).ceil() as i64).clamp(0, h as i64) as usize;

        let contig = view.as_slice();
        let mask_contig = valid.as_ref().and_then(|m| m.as_slice());
        debug_assert!(contig.is_none() || view.is_standard_layout());
        debug_assert!(valid
            .as_ref()
            .is_none_or(|m| mask_contig.is_none() || m.is_standard_layout()));
        let mut sum = 0.0;
        let mut scale = 1.0;
        let mut total = 0.0;
        for r in r_lo..r_hi {
            for c in c_lo..c_hi {
                let ok = match (mask_contig, valid) {
                    (Some(s), _) => s[r * w + c],
                    (None, Some(m)) => m[[r, c]],
                    (None, None) => true,
                };
                if !ok {
                    continue;
                }
                let val = match contig {
                    Some(s) => s[r * w + c],
                    None => view[[r, c]],
                };
                let dx = ((sc + rx).min(c as f64 + 0.5) - (sc - rx).max(c as f64 - 0.5)).max(0.0);
                let dy = ((sr + ry).min(r as f64 + 0.5) - (sr - ry).max(r as f64 - 0.5)).max(0.0);
                let area = dx * dy;
                if area > 0.0 {
                    add_weighted_sum(&mut sum, &mut total, &mut scale, val, area);
                }
            }
        }
        weighted_mean(sum, total, fill)
    }

    /// Area-weighted sampling of a four-corner source pixel footprint.
    pub fn sample_average_polygon(
        self,
        view: ArrayView2<f64>,
        valid: Option<ArrayView2<bool>>,
        corners: [(f64, f64); 4],
        fill: f64,
    ) -> f64 {
        if corners
            .iter()
            .any(|&(x, y)| !x.is_finite() || !y.is_finite())
        {
            return fill;
        }
        let xmin = corners.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let xmax = corners
            .iter()
            .map(|p| p.0)
            .fold(f64::NEG_INFINITY, f64::max);
        let ymin = corners.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let ymax = corners
            .iter()
            .map(|p| p.1)
            .fold(f64::NEG_INFINITY, f64::max);
        let (sc, sr) = ((xmin + xmax) * 0.5, (ymin + ymax) * 0.5);
        if xmax - xmin < 1.0 && ymax - ymin < 1.0 {
            return Self::Nearest.sample_masked(view, valid, sc, sr, fill);
        }
        if corners[0].1 == corners[1].1
            && corners[2].1 == corners[3].1
            && corners[0].0 == corners[3].0
            && corners[1].0 == corners[2].0
        {
            return self.sample_average(
                view,
                valid,
                sc,
                sr,
                (xmax - xmin) * 0.5,
                (ymax - ymin) * 0.5,
                fill,
            );
        }
        let (h, w) = view.dim();
        let c0 = (xmin + 0.5).floor().max(0.0) as usize;
        let c1 = (xmax + 0.5).ceil().clamp(0.0, w as f64) as usize;
        let r0 = (ymin + 0.5).floor().max(0.0) as usize;
        let r1 = (ymax + 0.5).ceil().clamp(0.0, h as f64) as usize;
        let mut polygon = Vec::with_capacity(12);
        let mut next = Vec::with_capacity(12);
        let mut total = 0.0;
        let mut sum = 0.0;
        let mut scale = 1.0;
        for r in r0..r1 {
            for c in c0..c1 {
                if valid.is_some_and(|m| !m[[r, c]]) {
                    continue;
                }
                polygon.clear();
                polygon.extend_from_slice(&corners);
                for (axis, bound, keep_greater) in [
                    (0, c as f64 - 0.5, true),
                    (0, c as f64 + 0.5, false),
                    (1, r as f64 - 0.5, true),
                    (1, r as f64 + 0.5, false),
                ] {
                    next.clear();
                    let coord = |p: (f64, f64)| if axis == 0 { p.0 } else { p.1 };
                    let inside = |p: (f64, f64)| {
                        if keep_greater {
                            coord(p) >= bound
                        } else {
                            coord(p) <= bound
                        }
                    };
                    if let Some(&last) = polygon.last() {
                        let mut previous = last;
                        for &point in &polygon {
                            if inside(point) != inside(previous) {
                                let t =
                                    (bound - coord(previous)) / (coord(point) - coord(previous));
                                next.push((
                                    previous.0 + t * (point.0 - previous.0),
                                    previous.1 + t * (point.1 - previous.1),
                                ));
                            }
                            if inside(point) {
                                next.push(point);
                            }
                            previous = point;
                        }
                    }
                    std::mem::swap(&mut polygon, &mut next);
                }
                if polygon.len() < 3 {
                    continue;
                }
                let mut twice_area = 0.0;
                for i in 0..polygon.len() {
                    let a = polygon[i];
                    let b = polygon[(i + 1) % polygon.len()];
                    twice_area +=
                        (a.0 - c as f64) * (b.1 - r as f64) - (b.0 - c as f64) * (a.1 - r as f64);
                }
                let area = twice_area.abs() * 0.5;
                if area > 0.0 {
                    add_weighted_sum(&mut sum, &mut total, &mut scale, view[[r, c]], area);
                }
            }
        }
        weighted_mean(sum, total, fill)
    }

    /// Majority value over the source footprint `[sc±rx, sr±ry]`. Counts each
    /// valid value; returns the most frequent (ties → lowest value). Sub-pixel
    /// footprint degrades to Nearest (categorical data must not interpolate).
    /// Empty/all-invalid box returns `fill`.
    ///
    /// Pixel selection uses center-membership: only source pixels whose centers
    /// lie within `[sc-rx, sc+rx] × [sr-ry, sr+ry]` are counted — unlike
    /// `sample_average`, which includes partially-covered edge pixels.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn sample_mode(
        self,
        view: ArrayView2<f64>,
        valid: Option<ArrayView2<bool>>,
        sc: f64,
        sr: f64,
        rx: f64,
        ry: f64,
        fill: f64,
    ) -> f64 {
        let (h, w) = (view.shape()[0], view.shape()[1]);
        if w == 0 || h == 0 {
            return fill;
        }
        if !sc.is_finite() || !sr.is_finite() {
            return fill;
        }
        // Sub-pixel footprint => nearest (NOT bilinear — no interpolation).
        if !rx.is_finite() || !ry.is_finite() || rx < 0.0 || ry < 0.0 {
            return fill;
        }
        if rx < 0.5 && ry < 0.5 {
            return Self::Nearest.sample_masked(view, valid, sc, sr, fill);
        }
        let c_lo = ((sc - rx).ceil().max(0.0)) as usize;
        let c_hi = ((sc + rx).floor() as i64).min(w as i64 - 1);
        let r_lo = ((sr - ry).ceil().max(0.0)) as usize;
        let r_hi = ((sr + ry).floor() as i64).min(h as i64 - 1);
        if c_hi < 0 || r_hi < 0 {
            return Self::Nearest.sample_masked(view, valid, sc, sr, fill);
        }
        let (c_hi, r_hi) = (c_hi as usize, r_hi as usize);

        // Anisotropic footprints (one thin axis) can reverse a center-membership
        // bound, leaving an empty box. Fall back to Nearest rather than emit a
        // spurious nodata hole.
        if c_lo > c_hi || r_lo > r_hi {
            return Self::Nearest.sample_masked(view, valid, sc, sr, fill);
        }

        mode_from_samples(
            (r_lo..=r_hi).flat_map(|r| {
                (c_lo..=c_hi)
                    .filter(move |&c| valid.is_none_or(|m| m[[r, c]]))
                    .map(move |c| view[[r, c]])
            }),
            fill,
        )
    }

    /// Mode counts source pixel centers inside the actual projected footprint.
    pub(crate) fn sample_mode_polygon(
        self,
        view: ArrayView2<f64>,
        valid: Option<ArrayView2<bool>>,
        corners: [(f64, f64); 4],
        sample_center: (f64, f64),
        fill: f64,
    ) -> f64 {
        if corners
            .iter()
            .any(|&(x, y)| !x.is_finite() || !y.is_finite())
        {
            return fill;
        }
        let xmin = corners.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let xmax = corners
            .iter()
            .map(|p| p.0)
            .fold(f64::NEG_INFINITY, f64::max);
        let ymin = corners.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let ymax = corners
            .iter()
            .map(|p| p.1)
            .fold(f64::NEG_INFINITY, f64::max);
        let (h, w) = view.dim();
        let c0 = (xmin - 1e-10).ceil().clamp(0.0, w as f64) as usize;
        let c1 = ((xmax + 1e-10).floor() + 1.0).clamp(0.0, w as f64) as usize;
        let r0 = (ymin - 1e-10).ceil().clamp(0.0, h as f64) as usize;
        let r1 = ((ymax + 1e-10).floor() + 1.0).clamp(0.0, h as f64) as usize;
        if c0 >= c1 || r0 >= r1 || (xmax - xmin < 1.0 && ymax - ymin < 1.0) {
            // Nonlinear projections move the footprint midpoint away from the actual sample.
            return Self::Nearest.sample_masked(
                view,
                valid,
                sample_center.0,
                sample_center.1,
                fill,
            );
        }
        let rectangular = corners[0].1 == corners[1].1
            && corners[2].1 == corners[3].1
            && corners[0].0 == corners[3].0
            && corners[1].0 == corners[2].0;
        let mut centers = 0;
        let value = mode_from_samples(
            (r0..r1)
                .flat_map(|r| (c0..c1).map(move |c| (r, c)))
                .filter(|&(r, c)| rectangular || point_in_footprint((c as f64, r as f64), &corners))
                .inspect(|_| centers += 1)
                .filter(|&(r, c)| valid.is_none_or(|m| m[[r, c]]))
                .map(|(r, c)| view[[r, c]]),
            fill,
        );
        if centers == 0 {
            // Thin footprints may contain no center; retain nearest fallback.
            Self::Nearest.sample_masked(view, valid, sample_center.0, sample_center.1, fill)
        } else {
            value
        }
    }
}

fn add_weighted_sum(sum: &mut f64, total: &mut f64, scale: &mut f64, value: f64, weight: f64) {
    let mut term = value * (weight * *scale);
    // Scale only on overflow so ordinary sums retain their rounding behavior.
    if sum.is_finite() && value.is_finite() && weight.is_finite() {
        while (*sum + term).is_infinite() {
            *sum *= 0.5;
            *total *= 0.5;
            *scale *= 0.5;
            term = value * (weight * *scale);
        }
    }
    *sum += term;
    *total += weight * *scale;
}

fn weighted_mean(sum: f64, total: f64, fill: f64) -> f64 {
    if total <= 0.0 {
        return fill;
    }
    let mean = sum / total;
    // Positive weights cannot put a finite-input mean beyond the F64 range.
    if sum.is_finite() && mean.is_infinite() {
        mean.signum() * f64::MAX
    } else {
        mean
    }
}

fn convolution_mean(samples: impl Iterator<Item = (f64, f64)> + Clone, fill: f64) -> f64 {
    let (mut sum, mut total, mut scale) = (0.0, 0.0, 1.0);
    for (value, weight) in samples.clone() {
        add_weighted_sum(&mut sum, &mut total, &mut scale, value, weight);
    }
    if total == 0.0 || !sum.is_finite() {
        return fill;
    }
    let mean = sum / total;
    if mean.is_finite() {
        return mean;
    }

    // Rounding the scaled sum/divisor can overflow even for a constant MAX
    // field. Retry in units of the largest sample so constant fields have an
    // exact normalized mean of +/-1. Unlike positive-weight averages, signed
    // kernels may truly overshoot MAX; preserve that failure instead of clamping.
    let largest = samples
        .clone()
        .map(|(value, _)| value.abs())
        .fold(0.0, f64::max);
    let (sum, total) = samples.fold((0.0, 0.0), |(sum, total), (value, weight)| {
        (sum + (value / largest) * weight, total + weight)
    });
    let mean = (sum / total) * largest;
    if mean.is_finite() {
        mean
    } else {
        fill
    }
}

fn point_in_footprint((x, y): (f64, f64), corners: &[(f64, f64); 4]) -> bool {
    let mut inside = false;
    for i in 0..4 {
        let (ax, ay) = corners[i];
        let (bx, by) = corners[(i + 1) % 4];
        let cross = (bx - ax) * (y - ay) - (by - ay) * (x - ax);
        let tolerance = 1e-10 * ((bx - ax).abs() + (by - ay).abs()).max(1.0);
        if cross.abs() <= tolerance
            && x >= ax.min(bx) - 1e-10
            && x <= ax.max(bx) + 1e-10
            && y >= ay.min(by) - 1e-10
            && y <= ay.max(by) + 1e-10
        {
            return true;
        }
        if (ay > y) != (by > y) && x < (bx - ax) * (y - ay) / (by - ay) + ax {
            inside = !inside;
        }
    }
    inside
}

fn mode_from_samples(samples: impl Iterator<Item = f64>, fill: f64) -> f64 {
    // Keep the inexpensive small-class path; its scan has a fixed ceiling.
    const SMALL_CLASS_LIMIT: usize = 32;
    let mut tally: Vec<(f64, usize)> = Vec::new();
    let mut sorted_samples: Option<Vec<f64>> = None;
    for value in samples.filter(|v| v.is_finite()) {
        if let Some(values) = &mut sorted_samples {
            values.push(value);
        } else if let Some((_, count)) = tally.iter_mut().find(|(v, _)| *v == value) {
            *count += 1;
        } else if tally.len() < SMALL_CLASS_LIMIT {
            tally.push((value, 1));
        } else {
            let mut values = Vec::new();
            for (value, count) in tally.drain(..) {
                values.extend(std::iter::repeat_n(value, count));
            }
            values.push(value);
            sorted_samples = Some(values);
        }
    }
    if let Some(mut values) = sorted_samples {
        values.sort_unstable_by(f64::total_cmp);
        let mut best = (fill, 0);
        let mut i = 0;
        while i < values.len() {
            let end = i + values[i..].partition_point(|v| *v == values[i]);
            if end - i > best.1 {
                best = (values[i], end - i);
            }
            i = end;
        }
        best.0
    } else {
        tally
            .into_iter()
            .reduce(|best, cur| {
                if cur.1 > best.1 || (cur.1 == best.1 && cur.0 < best.0) {
                    cur
                } else {
                    best
                }
            })
            .map(|(v, _)| v)
            .unwrap_or(fill)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use ndarray::array;

    #[test]
    fn convolution_preserves_extreme_finite_values_and_cancellation() {
        for kernel in [
            Resampling::Cubic,
            Resampling::CubicSpline,
            Resampling::Lanczos,
        ] {
            for expected in [f64::MAX, -f64::MAX] {
                let values = ndarray::Array2::from_elem((4, 4), expected);
                let mask = ndarray::Array2::from_shape_fn((4, 4), |(r, c)| (r + c) % 2 == 0);
                for (col, row) in [
                    (1.5, 1.5),
                    (1.0 + 1.0 / 137.0, 1.0 + 1.0 / 139.0),
                    (0.0, 0.0),
                ] {
                    for valid in [None, Some(mask.view())] {
                        let value = kernel.sample_masked(values.view(), valid, col, row, f64::NAN);
                        assert!(value.is_finite(), "{kernel:?}, {col},{row}");
                        assert_relative_eq!(value, expected, max_relative = 1e-14);
                    }
                }
            }
            let values = array![[f64::MAX, f64::MAX, -f64::MAX, -f64::MAX]];
            let value = kernel.sample(values.view(), 1.5, 0.0, f64::NAN);
            assert!(value.is_finite(), "{kernel:?}");
            assert_relative_eq!(value, 0.0, epsilon = 1e293);
        }
    }

    #[test]
    fn signed_kernel_support_preserves_valid_samples_and_rejects_zero_support() {
        let values = array![[10.0, 20.0, 30.0, 40.0]];
        let only_first = array![[true, false, false, false]];
        let none = array![[false, false, false, false]];
        for kernel in [Resampling::Cubic, Resampling::Lanczos] {
            for col in [0.5, 1.5] {
                assert_relative_eq!(
                    kernel.sample_masked(values.view(), Some(only_first.view()), col, 0.0, -1.0),
                    10.0,
                    epsilon = 1e-12
                );
            }
            for mask in [&only_first, &none] {
                assert_eq!(
                    kernel.sample_masked(values.view(), Some(mask.view()), 1.0, 0.0, -1.0),
                    -1.0
                );
            }
            let overflow = array![[-f64::MAX, f64::MAX, f64::MAX, -f64::MAX]];
            assert_eq!(kernel.sample(overflow.view(), 1.5, 0.0, -1.0), -1.0);
        }
        // A small but nonzero B-spline weight is still valid support.
        assert_relative_eq!(
            Resampling::CubicSpline.sample_masked(
                values.view(),
                Some(only_first.view()),
                1.9999,
                0.0,
                -1.0
            ),
            10.0,
            epsilon = 1e-12
        );
    }

    #[test]
    fn nearest_at_pixel_corners() {
        let a = array![[1.0, 2.0], [3.0, 4.0]];
        assert_eq!(Resampling::Nearest.sample(a.view(), 0.0, 0.0, 0.0), 1.0);
        assert_eq!(Resampling::Nearest.sample(a.view(), 1.0, 1.0, 0.0), 4.0);
    }

    #[test]
    fn bilinear_midpoint_averages() {
        let a = array![[0.0, 10.0], [0.0, 10.0]];
        assert_relative_eq!(Resampling::Bilinear.sample(a.view(), 0.5, 0.0, 0.0), 5.0);
    }

    #[test]
    fn convolution_kernels_have_distinct_reference_weights() {
        let a = array![[0.0, 0.0, 1.0, 0.0, 0.0, 0.0]];
        assert_relative_eq!(
            Resampling::Cubic.sample(a.view(), 2.5, 0.0, -1.0),
            0.5625,
            epsilon = 1e-12
        );
        assert_relative_eq!(
            Resampling::CubicSpline.sample(a.view(), 2.5, 0.0, -1.0),
            23.0 / 48.0,
            epsilon = 1e-12
        );
        assert_relative_eq!(
            Resampling::Lanczos.sample(a.view(), 2.5, 0.0, -1.0),
            0.6114130434782609,
            epsilon = 1e-12
        );
    }

    #[test]
    fn average_weights_partial_source_pixel_coverage() {
        let a = array![[0.0, 10.0, 100.0]];
        // Footprint [-0.25, 1.25] intersects pixel 0 by .75 and pixel 1 by .75;
        // pixel 2 has no overlap at all.
        assert_relative_eq!(
            Resampling::Average.sample_average(a.view(), None, 0.5, 0.0, 0.75, 0.5, -1.0),
            5.0
        );
        // Footprint [0, 1.5] weights pixel 0 by .5 and pixel 1 by 1.
        assert_relative_eq!(
            Resampling::Average.sample_average(a.view(), None, 0.75, 0.0, 0.75, 0.5, -1.0),
            20.0 / 3.0
        );
    }

    #[test]
    fn average_rotated_footprint_uses_polygon_overlap() {
        let a = array![[10.0, 10.0, 10.0], [10.0, 100.0, 10.0], [10.0, 10.0, 10.0]];
        // Diamond area2: central cell area1, four axial cells area.25 each.
        let corners = [(1.0, 0.0), (2.0, 1.0), (1.0, 2.0), (0.0, 1.0)];
        assert_relative_eq!(
            Resampling::Average.sample_average_polygon(a.view(), None, corners, -1.0),
            55.0,
            epsilon = 1e-12
        );
    }

    #[test]
    fn average_subpixel_uses_nearest() {
        let a = array![[0.0, 10.0]];
        assert_eq!(
            Resampling::Average.sample_average(a.view(), None, 0.25, 0.0, 0.1, 0.1, -1.0),
            0.0
        );
    }

    #[test]
    fn out_of_bounds_returns_fill() {
        let a = array![[1.0]];
        assert_eq!(Resampling::Nearest.sample(a.view(), 5.0, 5.0, -1.0), -1.0);
    }

    #[test]
    fn parse_string() {
        assert_eq!(Resampling::parse("bilinear"), Some(Resampling::Bilinear));
        assert_eq!(Resampling::parse("cubic"), Some(Resampling::Cubic));
        assert_eq!(Resampling::parse("bicubic"), Some(Resampling::Cubic));
        assert_eq!(Resampling::parse("foo"), None);
        assert_eq!(Resampling::parse("average"), Some(Resampling::Average));
    }

    #[test]
    fn spline_name_does_not_alias_a_different_kernel() {
        assert_eq!(
            Resampling::parse("cubicspline"),
            Some(Resampling::CubicSpline)
        );
        assert_eq!(Resampling::parse("mitchell"), None);
    }

    #[test]
    fn average_box_mean() {
        // 4x4 ramp 0..16; full-tile footprint => arithmetic mean = 7.5
        let a = ndarray::Array2::from_shape_fn((4, 4), |(r, c)| (r * 4 + c) as f64);
        let v = Resampling::Average.sample_average(a.view(), None, 1.5, 1.5, 1.5, 1.5, -1.0);
        assert_relative_eq!(v, 7.5);
    }

    #[test]
    fn average_preserves_large_finite_values_and_cancellation() {
        let diamond = [(-0.5, 0.5), (0.5, -0.5), (1.5, 0.5), (0.5, 1.5)];
        for (pixels, expected) in [
            (array![[f64::MAX, f64::MAX], [f64::MAX, f64::MAX]], f64::MAX),
            (
                array![[-f64::MAX, -f64::MAX], [-f64::MAX, -f64::MAX]],
                -f64::MAX,
            ),
            (array![[1e308, 1e308], [-1e308, -1e308]], 0.0),
        ] {
            for value in [
                Resampling::Average.sample_average(
                    pixels.view(),
                    None,
                    0.5,
                    0.5,
                    1.0,
                    1.0,
                    f64::NAN,
                ),
                Resampling::Average.sample_average_polygon(pixels.view(), None, diamond, f64::NAN),
            ] {
                assert!(value.is_finite());
                assert_relative_eq!(value, expected, epsilon = 1e293);
            }
        }
    }

    #[test]
    fn average_fractional_weights_keep_extreme_values_finite() {
        for expected in [f64::MAX, -f64::MAX] {
            let pixels = ndarray::Array2::from_elem((3, 3), expected);
            let value = Resampling::Average.sample_average(
                pixels.view(),
                None,
                1.0 / 137.0,
                1.0 / 139.0,
                0.5 + 1.0 / 211.0,
                0.5 + 1.0 / 223.0,
                f64::NAN,
            );
            assert_eq!(value, expected);
        }
        let pixels = ndarray::Array2::from_elem((3, 3), f64::INFINITY);
        assert!(Resampling::Average
            .sample_average(pixels.view(), None, 1.0, 1.0, 1.0, 1.0, f64::NAN,)
            .is_infinite());
    }

    #[test]
    fn average_respects_mask() {
        let a = ndarray::Array2::from_elem((4, 4), 10.0);
        let mut m = ndarray::Array2::from_elem((4, 4), true);
        m[[0, 0]] = false; // one invalid pixel, value still 10 elsewhere
        let v =
            Resampling::Average.sample_average(a.view(), Some(m.view()), 1.5, 1.5, 1.5, 1.5, -1.0);
        assert_relative_eq!(v, 10.0); // mean of valid pixels (all 10)

        let none_valid = ndarray::Array2::from_elem((4, 4), false);
        let f = Resampling::Average.sample_average(
            a.view(),
            Some(none_valid.view()),
            1.5,
            1.5,
            1.5,
            1.5,
            -7.0,
        );
        assert_eq!(f, -7.0); // all invalid => fill
    }

    #[test]
    fn average_nan_coords_return_fill() {
        let a = ndarray::Array2::from_elem((4, 4), 5.0);
        let v = Resampling::Average.sample_average(a.view(), None, f64::NAN, 2.0, 2.0, 2.0, -9.0);
        assert_eq!(v, -9.0);
    }

    #[test]
    fn average_degenerates_to_nearest() {
        let a = array![[0.0, 10.0], [0.0, 10.0]];
        let avg = Resampling::Average.sample_average(a.view(), None, 0.5, 0.0, 0.3, 0.3, 0.0);
        assert_eq!(avg, 10.0); // sub-1px footprint chooses the nearest center
    }

    #[test]
    fn parse_mode() {
        assert_eq!(Resampling::parse("mode"), Some(Resampling::Mode));
    }

    #[test]
    fn mode_high_cardinality_preserves_counts_ties_and_signed_zero() {
        let unique = ndarray::Array2::from_shape_fn((256, 256), |(r, c)| (r * 256 + c) as f64);
        assert_eq!(
            Resampling::Mode.sample_mode(unique.view(), None, 127.5, 127.5, 127.5, 127.5, -1.),
            0.
        );
        // Promote to sorted samples only after some repeated small-class values.
        let values = std::iter::repeat_n(99., 12)
            .chain(std::iter::repeat_n(-0., 7))
            .chain((1..1000).map(|n| n as f64))
            .chain(std::iter::repeat_n(0., 7));
        assert_eq!(mode_from_samples(values, -1.), 0.);
        assert_eq!(
            mode_from_samples((0..100).map(|n| n as f64).chain([91., 92.]), -1.),
            91.
        );
        assert_eq!(
            mode_from_samples([f64::NAN, f64::INFINITY].into_iter(), -1.),
            -1.
        );
    }

    #[test]
    fn mode_rotated_footprint_excludes_outer_corners_and_respects_mask() {
        let a = array![[9., 2., 9.], [3., 1., 4.], [9., 5., 9.]];
        let corners = [(1., 0.), (2., 1.), (1., 2.), (0., 1.)];
        assert_eq!(
            Resampling::Mode.sample_mode_polygon(a.view(), None, corners, (1.0, 1.0), -1.),
            1.
        );
        let mut mask = ndarray::Array2::from_elem((3, 3), true);
        mask[[1, 1]] = false;
        assert_eq!(
            Resampling::Mode.sample_mode_polygon(
                a.view(),
                Some(mask.view()),
                corners,
                (1.0, 1.0),
                -1.
            ),
            2.
        );
        mask.fill(false);
        assert_eq!(
            Resampling::Mode.sample_mode_polygon(
                a.view(),
                Some(mask.view()),
                corners,
                (1.0, 1.0),
                -1.
            ),
            -1.
        );
        assert_eq!(
            Resampling::Mode.sample_mode_polygon(
                a.view(),
                None,
                corners.map(|(x, y)| (x + 10., y)),
                (11.0, 1.0),
                -1.
            ),
            -1.
        );
        for shift in [(-10., 0.), (0., -10.), (0., 10.)] {
            assert_eq!(
                Resampling::Mode.sample_mode_polygon(
                    a.view(),
                    None,
                    corners.map(|(x, y)| (x + shift.0, y + shift.1)),
                    (1.0 + shift.0, 1.0 + shift.1),
                    -1.
                ),
                -1.
            );
        }
        for (x, y) in [(-10., 1.), (1., -10.), (10., 1.), (1., 10.)] {
            assert_eq!(
                Resampling::Mode.sample_mode(a.view(), None, x, y, 1., 1., -1.),
                -1.
            );
        }
        assert_eq!(
            Resampling::Mode.sample_mode_polygon(
                a.view(),
                None,
                [(f64::NAN, 0.); 4],
                (f64::NAN, 0.0),
                -1.
            ),
            -1.
        );
    }

    #[test]
    fn mode_sheared_footprint_and_thin_fallback() {
        let a = array![[9., 1., 9.], [2., 1., 9.], [2., 9., 9.]];
        let corners = [(0.5, -0.5), (1.5, -0.5), (0.5, 2.5), (-0.5, 2.5)];
        assert_eq!(
            Resampling::Mode.sample_mode_polygon(a.view(), None, corners, (1.0, 1.0), -1.),
            1.
        );
        let thin = [(0., 0.2), (2., 0.2), (2.1, 0.3), (0.1, 0.3)];
        assert_eq!(
            Resampling::Mode.sample_mode_polygon(a.view(), None, thin, (1.05, 0.25), -1.),
            1.
        );
    }

    #[test]
    fn mode_box_majority() {
        let a = ndarray::array![
            [5.0, 5.0, 1.0, 1.0],
            [5.0, 1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0, 1.0]
        ];
        // footprint over top-left 2x2 (values 5,5,5,1) → majority 5
        let v = Resampling::Mode.sample_mode(a.view(), None, 0.5, 0.5, 1.0, 1.0, -1.0);
        assert_eq!(v, 5.0);
    }

    #[test]
    fn mode_subpixel_uses_nearest_not_bilinear() {
        let a = ndarray::array![[0.0, 10.0], [0.0, 10.0]];
        let m = Resampling::Mode.sample_mode(a.view(), None, 0.5, 0.0, 0.3, 0.3, -1.0);
        let n = Resampling::Nearest.sample_masked(a.view(), None, 0.5, 0.0, -1.0);
        assert_eq!(m, n);
    }

    #[test]
    fn mode_ties_pick_lowest_value() {
        let a = ndarray::array![[1.0, 2.0], [2.0, 1.0]];
        let v = Resampling::Mode.sample_mode(a.view(), None, 0.5, 0.5, 1.0, 1.0, -1.0);
        assert_eq!(v, 1.0);
    }

    #[test]
    fn mode_all_invalid_returns_fill() {
        let a = ndarray::Array2::from_elem((2, 2), 5.0);
        let none_valid = ndarray::Array2::from_elem((2, 2), false);
        let v = Resampling::Mode.sample_mode(
            a.view(),
            Some(none_valid.view()),
            0.5,
            0.5,
            1.0,
            1.0,
            -7.0,
        );
        assert_eq!(v, -7.0);
    }

    #[test]
    fn mode_anisotropic_thin_axis_does_not_hole() {
        // rx wide, ry thin (<0.5): the thin-axis center-membership bound would
        // reverse to an empty box; must fall back to Nearest, not return fill.
        let a = ndarray::array![[3.0, 3.0, 7.0], [3.0, 7.0, 7.0]];
        let v = Resampling::Mode.sample_mode(a.view(), None, 1.0, 0.5, 1.0, 0.3, -1.0);
        assert_ne!(
            v, -1.0,
            "anisotropic footprint must not produce a fill hole"
        );
        // Nearest at (1.0, 0.5) rounds to a real cell value (3 or 7), never fill.
        let n = Resampling::Nearest.sample_masked(a.view(), None, 1.0, 0.5, -1.0);
        assert_eq!(v, n);
    }
}
