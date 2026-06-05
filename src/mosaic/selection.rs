//! Pixel-selection reducer. Mirrors rio-tiler `MosaicMethodBase`.

use ndarray::{Array2, Array3};

use crate::error::{AgError, AgResult};
use crate::pixel::{ImageData, PixelData};

/// Strategy for combining overlapping pixels from multiple mosaic assets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelSelection {
    /// First valid pixel by paint order wins; short-circuits once tile is fully covered.
    First,
    /// Per-pixel maximum on band 0 across ALL assets (global extremum). Reads
    /// every asset — does NOT short-circuit on full coverage, since a
    /// not-yet-read overlapping asset can still hold a higher value. Matches
    /// rio-tiler, where only `FirstMethod` sets `exit_when_filled`.
    Highest,
    /// Per-pixel minimum on band 0 across ALL assets (global extremum). Reads
    /// every asset — does NOT short-circuit (see [`Self::Highest`]).
    Lowest,
    /// Per-pixel mean over all valid asset samples; all assets are read (no short-circuit).
    Mean,
    /// Per-pixel median over all valid asset samples; all assets are read (no short-circuit).
    Median,
}

impl PixelSelection {
    /// `true` if this method can stop once every pixel is valid.
    ///
    /// Only `First` qualifies: once every pixel has a value, paint order
    /// guarantees later assets cannot change the result. `Highest`/`Lowest`
    /// need a global extremum, so an already-covered pixel can still be
    /// overwritten by a later asset — they must read everything.
    fn exits_when_filled(self) -> bool {
        matches!(self, Self::First)
    }
}

/// Accumulates per-asset images into one composite.
pub struct Reducer {
    method: PixelSelection,
    /// `(bands, h, w)` accumulator — `None` until first feed.
    /// First/Highest/Lowest: the running result. Mean: the running **sum**.
    acc: Option<Array3<f64>>,
    /// `(h, w)` validity of `acc`, `true = valid`.
    acc_mask: Option<Array2<bool>>,
    /// Mean only: per-band finite-sample count, parallel to `acc` (the sum).
    count: Option<Array3<u32>>,
    /// Median only: every fed image's `(bands,h,w)` + mask.
    stack: Vec<(Array3<f64>, Array2<bool>)>,
    /// First only: pixels still invalid, for O(1) [`Self::is_done`]. `usize::MAX`
    /// until the first feed sizes it.
    remaining_invalid: usize,
    bbox: Option<crate::bounds::Bbox>,
    crs: Option<crate::crs::Crs>,
    transform: Option<crate::bounds::GeoTransform>,
}

fn mask_of(img: &ImageData) -> Array2<bool> {
    let (h, w) = (img.pixels.height(), img.pixels.width());
    img.mask
        .clone()
        .unwrap_or_else(|| Array2::from_elem((h, w), true))
}

impl Reducer {
    pub fn new(method: PixelSelection) -> Self {
        Self {
            method,
            acc: None,
            acc_mask: None,
            count: None,
            stack: Vec::new(),
            remaining_invalid: usize::MAX,
            bbox: None,
            crs: None,
            transform: None,
        }
    }

    /// Feed one warped asset image (already on the destination tile grid).
    pub fn feed(&mut self, img: &ImageData) -> AgResult<()> {
        let arr = img.pixels.to_f64(); // (bands, h, w)
        let m = mask_of(img);
        if self.bbox.is_none() {
            self.bbox = Some(img.bbox);
            self.crs = Some(img.crs.clone());
            self.transform = Some(img.transform);
        }
        match self.method {
            PixelSelection::First => self.feed_first(arr, m),
            PixelSelection::Highest => self.feed_extreme(arr, m, true),
            PixelSelection::Lowest => self.feed_extreme(arr, m, false),
            PixelSelection::Mean => self.feed_mean(arr, m),
            PixelSelection::Median => {
                if let Some((first, _)) = self.stack.first() {
                    debug_assert_eq!(
                        arr.dim(),
                        first.dim(),
                        "mosaic assets must be homogeneous (same shape)"
                    );
                }
                self.stack.push((arr, m));
                Ok(())
            }
        }
    }

    fn feed_first(&mut self, arr: Array3<f64>, m: Array2<bool>) -> AgResult<()> {
        match (&mut self.acc, &mut self.acc_mask) {
            (None, _) => {
                // Seed `remaining_invalid` from the first asset so `is_done`
                // is O(1) thereafter (decremented as holes fill).
                self.remaining_invalid = m.iter().filter(|&&v| !v).count();
                self.acc = Some(arr);
                self.acc_mask = Some(m);
            }
            (Some(acc), Some(am)) => {
                let (bands, h, w) = acc.dim();
                for y in 0..h {
                    for x in 0..w {
                        // fill only where acc is still invalid AND arr is valid
                        if !am[[y, x]] && m[[y, x]] {
                            for b in 0..bands {
                                acc[[b, y, x]] = arr[[b, y, x]];
                            }
                            am[[y, x]] = true;
                            self.remaining_invalid = self.remaining_invalid.saturating_sub(1);
                        }
                    }
                }
            }
            _ => unreachable!("acc and acc_mask set together"),
        }
        Ok(())
    }

    /// Streaming mean: keep a running per-band `sum` (in `acc`) and finite-sample
    /// `count`, instead of stacking every asset's full array. Memory is O(1) in
    /// the number of assets. A pixel is valid (`acc_mask`) if ANY asset marks it
    /// valid; per band, only finite samples contribute — matching the previous
    /// stack aggregation exactly.
    fn feed_mean(&mut self, arr: Array3<f64>, m: Array2<bool>) -> AgResult<()> {
        let (bands, h, w) = arr.dim();
        let (sum, count, am) = match (&mut self.acc, &mut self.count, &mut self.acc_mask) {
            (Some(sum), Some(count), Some(am)) => {
                debug_assert_eq!(
                    sum.dim(),
                    arr.dim(),
                    "mosaic assets must be homogeneous (same shape)"
                );
                (sum, count, am)
            }
            _ => {
                self.acc = Some(Array3::<f64>::zeros((bands, h, w)));
                self.count = Some(Array3::<u32>::zeros((bands, h, w)));
                self.acc_mask = Some(Array2::from_elem((h, w), false));
                (
                    self.acc.as_mut().unwrap(),
                    self.count.as_mut().unwrap(),
                    self.acc_mask.as_mut().unwrap(),
                )
            }
        };
        for y in 0..h {
            for x in 0..w {
                if !m[[y, x]] {
                    continue;
                }
                am[[y, x]] = true;
                for b in 0..bands {
                    let v = arr[[b, y, x]];
                    if v.is_finite() {
                        sum[[b, y, x]] += v;
                        count[[b, y, x]] += 1;
                    }
                }
            }
        }
        Ok(())
    }

    fn feed_extreme(&mut self, arr: Array3<f64>, m: Array2<bool>, want_high: bool) -> AgResult<()> {
        match (&mut self.acc, &mut self.acc_mask) {
            (None, _) => {
                self.acc = Some(arr);
                self.acc_mask = Some(m);
            }
            (Some(acc), Some(am)) => {
                let (bands, h, w) = acc.dim();
                for y in 0..h {
                    for x in 0..w {
                        if !m[[y, x]] {
                            continue;
                        }
                        // compare on band 0; replace whole pixel stack
                        let replace = if !am[[y, x]] {
                            true
                        } else if want_high {
                            arr[[0, y, x]] > acc[[0, y, x]]
                        } else {
                            arr[[0, y, x]] < acc[[0, y, x]]
                        };
                        if replace {
                            for b in 0..bands {
                                acc[[b, y, x]] = arr[[b, y, x]];
                            }
                            am[[y, x]] = true;
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    /// `true` when the method can short-circuit (tile fully covered).
    ///
    /// O(1): only `First` exits-when-filled, and it tracks `remaining_invalid`
    /// incrementally as holes fill, so no full mask scan is needed.
    pub fn is_done(&self) -> bool {
        if !self.method.exits_when_filled() {
            return false;
        }
        // `remaining_invalid` is `usize::MAX` until the first feed sizes it.
        self.acc.is_some() && self.remaining_invalid == 0
    }

    /// Collapse to one `ImageData`. `Err` if nothing was fed.
    pub fn finish(self) -> AgResult<ImageData> {
        let (data, mask) = match self.method {
            PixelSelection::First | PixelSelection::Highest | PixelSelection::Lowest => {
                let acc = self
                    .acc
                    .ok_or_else(|| AgError::Mosaic("no assets fed".into()))?;
                (
                    acc,
                    self.acc_mask
                        .expect("invariant: acc_mask is set whenever acc is set"),
                )
            }
            PixelSelection::Mean => {
                // Streamed: divide running sum by finite-sample count per band.
                let sum = self
                    .acc
                    .ok_or_else(|| AgError::Mosaic("no assets fed".into()))?;
                let count = self
                    .count
                    .expect("invariant: count is set whenever the Mean sum is set");
                let mask = self
                    .acc_mask
                    .expect("invariant: acc_mask is set whenever the Mean sum is set");
                let mut out = sum;
                ndarray::Zip::from(&mut out).and(&count).for_each(|v, &c| {
                    *v = if c > 0 { *v / c as f64 } else { 0.0 };
                });
                (out, mask)
            }
            PixelSelection::Median => {
                if self.stack.is_empty() {
                    return Err(AgError::Mosaic("no assets fed".into()));
                }
                aggregate_median(&self.stack)
            }
        };
        Ok(ImageData {
            pixels: PixelData::F64(data),
            mask: Some(mask),
            bbox: self.bbox.unwrap(),
            crs: self.crs.unwrap(),
            transform: self.transform.unwrap(),
            nodata: None,
        })
    }
}

/// Per-pixel median across the stack, skipping invalid samples per pixel.
/// Uses `select_nth_unstable_by` (quickselect, O(n)) rather than a full sort.
fn aggregate_median(stack: &[(Array3<f64>, Array2<bool>)]) -> (Array3<f64>, Array2<bool>) {
    let (bands, h, w) = stack[0].0.dim();
    let mut out = Array3::<f64>::zeros((bands, h, w));
    let mut out_mask = Array2::from_elem((h, w), false);
    let mut buf: Vec<f64> = Vec::with_capacity(stack.len());
    for y in 0..h {
        for x in 0..w {
            let any_valid = stack.iter().any(|(_, m)| m[[y, x]]);
            if !any_valid {
                continue;
            }
            out_mask[[y, x]] = true;
            for b in 0..bands {
                buf.clear();
                for (arr, m) in stack {
                    if m[[y, x]] {
                        buf.push(arr[[b, y, x]]);
                    }
                }
                let len = buf.len();
                let mid = len / 2;
                // total_cmp orders NaN last, matching the previous full-sort
                // behaviour; quickselect only positions the needed rank(s).
                let (_, hi, _) = buf.select_nth_unstable_by(mid, f64::total_cmp);
                let hi_val = *hi;
                out[[b, y, x]] = if len % 2 == 1 {
                    hi_val
                } else {
                    let (_, lo, _) = buf.select_nth_unstable_by(mid - 1, f64::total_cmp);
                    (*lo + hi_val) / 2.0
                };
            }
        }
    }
    (out, out_mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bounds::{Bbox, GeoTransform};
    use crate::crs::Crs;

    fn img(vals: &[f64], valid: &[bool]) -> ImageData {
        // 1 band, 1 row, N cols
        let n = vals.len();
        let arr = Array3::from_shape_vec((1, 1, n), vals.to_vec()).unwrap();
        let mask = Array2::from_shape_vec((1, n), valid.to_vec()).unwrap();
        ImageData {
            pixels: PixelData::F64(arr),
            mask: Some(mask),
            bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 1.0, 0.0, -1.0]),
            nodata: None,
        }
    }

    #[test]
    fn first_fills_holes_in_order_and_short_circuits() {
        let mut r = Reducer::new(PixelSelection::First);
        r.feed(&img(&[1.0, 0.0], &[true, false])).unwrap();
        assert!(!r.is_done()); // pixel 1 still invalid
        r.feed(&img(&[9.0, 2.0], &[true, true])).unwrap();
        assert!(r.is_done()); // all valid now
        let out = r.finish().unwrap();
        let d = out.pixels.to_f64();
        assert_eq!(d[[0, 0, 0]], 1.0); // first asset wins (not overwritten)
        assert_eq!(d[[0, 0, 1]], 2.0); // filled from second
    }

    #[test]
    fn highest_takes_max_lowest_takes_min() {
        let mut hi = Reducer::new(PixelSelection::Highest);
        hi.feed(&img(&[1.0], &[true])).unwrap();
        hi.feed(&img(&[5.0], &[true])).unwrap();
        hi.feed(&img(&[3.0], &[true])).unwrap();
        assert_eq!(hi.finish().unwrap().pixels.to_f64()[[0, 0, 0]], 5.0);

        let mut lo = Reducer::new(PixelSelection::Lowest);
        lo.feed(&img(&[4.0], &[true])).unwrap();
        lo.feed(&img(&[2.0], &[true])).unwrap();
        assert_eq!(lo.finish().unwrap().pixels.to_f64()[[0, 0, 0]], 2.0);
    }

    #[test]
    fn mean_and_median_skip_invalid_and_never_short_circuit() {
        let mut me = Reducer::new(PixelSelection::Mean);
        me.feed(&img(&[2.0], &[true])).unwrap();
        me.feed(&img(&[6.0], &[true])).unwrap();
        me.feed(&img(&[100.0], &[false])).unwrap(); // ignored
        assert!(!me.is_done()); // stack family: no short-circuit
        assert_eq!(me.finish().unwrap().pixels.to_f64()[[0, 0, 0]], 4.0);

        let mut md = Reducer::new(PixelSelection::Median);
        md.feed(&img(&[1.0], &[true])).unwrap();
        md.feed(&img(&[10.0], &[true])).unwrap();
        md.feed(&img(&[3.0], &[true])).unwrap();
        assert_eq!(md.finish().unwrap().pixels.to_f64()[[0, 0, 0]], 3.0);
    }

    #[test]
    fn highest_lowest_do_not_short_circuit_and_take_global_extremum() {
        // Two assets that both fully cover the tile. The second carries a
        // higher value at the pixel, so a correct Highest must read it even
        // though the tile was already "filled" after the first asset.
        let mut hi = Reducer::new(PixelSelection::Highest);
        hi.feed(&img(&[3.0], &[true])).unwrap();
        assert!(
            !hi.is_done(),
            "Highest must never short-circuit (it needs a global max)"
        );
        hi.feed(&img(&[7.0], &[true])).unwrap();
        assert!(!hi.is_done());
        assert_eq!(hi.finish().unwrap().pixels.to_f64()[[0, 0, 0]], 7.0);

        let mut lo = Reducer::new(PixelSelection::Lowest);
        lo.feed(&img(&[3.0], &[true])).unwrap();
        assert!(!lo.is_done());
        lo.feed(&img(&[1.0], &[true])).unwrap();
        assert_eq!(lo.finish().unwrap().pixels.to_f64()[[0, 0, 0]], 1.0);
    }

    #[test]
    fn first_is_done_is_o1_and_tracks_remaining() {
        // Empty reducer is not done; partial coverage is not done; full is.
        let mut r = Reducer::new(PixelSelection::First);
        assert!(!r.is_done(), "no assets fed → not done");
        r.feed(&img(&[1.0, 0.0, 0.0], &[true, false, false]))
            .unwrap();
        assert!(!r.is_done());
        r.feed(&img(&[0.0, 2.0, 0.0], &[false, true, false]))
            .unwrap();
        assert!(!r.is_done());
        r.feed(&img(&[0.0, 0.0, 3.0], &[false, false, true]))
            .unwrap();
        assert!(r.is_done(), "all three pixels filled → done");
    }

    #[test]
    fn finish_with_no_assets_is_error() {
        let r = Reducer::new(PixelSelection::First);
        assert!(r.finish().is_err());
    }

    #[test]
    fn median_and_mean_are_nan_safe() {
        // NaN samples with mask=true must not panic; median uses total order,
        // mean ignores non-finite samples.
        let mut md = Reducer::new(PixelSelection::Median);
        md.feed(&img(&[1.0], &[true])).unwrap();
        md.feed(&img(&[f64::NAN], &[true])).unwrap();
        md.feed(&img(&[3.0], &[true])).unwrap();
        let v = md.finish().unwrap().pixels.to_f64()[[0, 0, 0]];
        // total_cmp orders NaN last → sorted [1.0, 3.0, NaN], len=3, mid=1 → 3.0
        assert!(v.is_finite()); // no panic
        assert_eq!(v, 3.0);

        let mut me = Reducer::new(PixelSelection::Mean);
        me.feed(&img(&[2.0], &[true])).unwrap();
        me.feed(&img(&[f64::NAN], &[true])).unwrap();
        me.feed(&img(&[4.0], &[true])).unwrap();
        // NaN filtered out: mean of [2.0, 4.0] = 3.0
        assert_eq!(me.finish().unwrap().pixels.to_f64()[[0, 0, 0]], 3.0);
    }
}
