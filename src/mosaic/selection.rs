//! Pixel-selection reducer. Mirrors rio-tiler `MosaicMethodBase`.

use ndarray::{Array2, Array3};

use crate::error::{AgError, AgResult};
use crate::pixel::{ImageData, PixelData};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelSelection {
    First,
    Highest,
    Lowest,
    Mean,
    Median,
}

impl PixelSelection {
    /// `true` if this method can stop once every pixel is valid.
    fn exits_when_filled(self) -> bool {
        matches!(self, Self::First | Self::Highest | Self::Lowest)
    }
}

/// Accumulates per-asset images into one composite.
pub struct Reducer {
    method: PixelSelection,
    /// `(bands, h, w)` accumulator (in-place family) — `None` until first feed.
    acc: Option<Array3<f64>>,
    /// `(h, w)` validity of `acc`, `true = valid`.
    acc_mask: Option<Array2<bool>>,
    /// stack family: every fed image's `(bands,h,w)` + mask.
    stack: Vec<(Array3<f64>, Array2<bool>)>,
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
            stack: Vec::new(),
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
            PixelSelection::Mean | PixelSelection::Median => {
                self.stack.push((arr, m));
                Ok(())
            }
        }
    }

    fn feed_first(&mut self, arr: Array3<f64>, m: Array2<bool>) -> AgResult<()> {
        match (&mut self.acc, &mut self.acc_mask) {
            (None, _) => {
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
                        }
                    }
                }
            }
            _ => unreachable!("acc and acc_mask set together"),
        }
        Ok(())
    }

    fn feed_extreme(
        &mut self,
        arr: Array3<f64>,
        m: Array2<bool>,
        want_high: bool,
    ) -> AgResult<()> {
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
    pub fn is_done(&self) -> bool {
        if !self.method.exits_when_filled() {
            return false;
        }
        match &self.acc_mask {
            Some(am) => am.iter().all(|&v| v),
            None => false,
        }
    }

    /// Collapse to one `ImageData`. `Err` if nothing was fed.
    pub fn finish(self) -> AgResult<ImageData> {
        let (data, mask) = match self.method {
            PixelSelection::First | PixelSelection::Highest | PixelSelection::Lowest => {
                let acc = self
                    .acc
                    .ok_or_else(|| AgError::Mosaic("no assets fed".into()))?;
                (acc, self.acc_mask.unwrap())
            }
            PixelSelection::Mean | PixelSelection::Median => {
                if self.stack.is_empty() {
                    return Err(AgError::Mosaic("no assets fed".into()));
                }
                aggregate_stack(&self.stack, self.method == PixelSelection::Mean)
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

/// Mean (or median) across the stack, skipping invalid samples per pixel.
fn aggregate_stack(
    stack: &[(Array3<f64>, Array2<bool>)],
    mean: bool,
) -> (Array3<f64>, Array2<bool>) {
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
                out[[b, y, x]] = if mean {
                    buf.iter().sum::<f64>() / buf.len() as f64
                } else {
                    let mid = buf.len() / 2;
                    buf.sort_by(|a, c| a.partial_cmp(c).unwrap());
                    if buf.len() % 2 == 1 {
                        buf[mid]
                    } else {
                        (buf[mid - 1] + buf[mid]) / 2.0
                    }
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
    fn finish_with_no_assets_is_error() {
        let r = Reducer::new(PixelSelection::First);
        assert!(r.finish().is_err());
    }
}
