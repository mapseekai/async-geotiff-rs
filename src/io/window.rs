//! Bbox -> pixel window math + tile stitching.

use crate::error::{AgError, AgResult};
use crate::pixel::PixelData;
use async_tiff::{Array, ImageFileDirectory, TypedArray};
use ndarray::Array3;

/// Convert pixel window to inclusive tile coordinate range
/// (tx_min..=tx_max, ty_min..=ty_max). Returns None if the IFD lacks tile dims.
pub fn tile_range_for_window(
    ifd: &ImageFileDirectory,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> Option<((u32, u32), (u32, u32))> {
    let tw = ifd.tile_width()? as i64;
    let th = ifd.tile_height()? as i64;
    let col_end = col_off + width as i64;
    let row_end = row_off + height as i64;
    let tx0 = (col_off.max(0) / tw) as u32;
    let ty0 = (row_off.max(0) / th) as u32;
    let tx1 = ((col_end - 1).max(0) / tw) as u32;
    let ty1 = ((row_end - 1).max(0) / th) as u32;
    Some(((tx0, tx1), (ty0, ty1)))
}

/// Stitch the fetched tiles (in (tx, ty) iteration order) into a planar (bands, h, w) buffer
/// cropped to the requested window. Returns the data as PixelData::F64 for uniformity.
pub fn stitch(
    arrays: &[Array],
    coords: &[(usize, usize)],
    ifd: &ImageFileDirectory,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> AgResult<PixelData> {
    let tw = ifd
        .tile_width()
        .ok_or_else(|| AgError::Tiff("no tile_width".into()))? as i64;
    let th = ifd
        .tile_height()
        .ok_or_else(|| AgError::Tiff("no tile_height".into()))? as i64;
    let bands = ifd.samples_per_pixel() as usize;

    let mut out = Array3::<f64>::zeros((bands, height as usize, width as usize));
    for ((tx, ty), arr) in coords.iter().zip(arrays.iter()) {
        let tile_col = (*tx as i64) * tw;
        let tile_row = (*ty as i64) * th;
        let arr_f64 = array_to_f64(arr)?;
        let shape = arr_f64.shape();
        let (a_h, a_w) = (shape[1], shape[2]);
        for b in 0..bands.min(shape[0]) {
            for r in 0..a_h {
                let world_row = tile_row + r as i64;
                if world_row < row_off || world_row >= row_off + height as i64 {
                    continue;
                }
                for c in 0..a_w {
                    let world_col = tile_col + c as i64;
                    if world_col < col_off || world_col >= col_off + width as i64 {
                        continue;
                    }
                    let dst_r = (world_row - row_off) as usize;
                    let dst_c = (world_col - col_off) as usize;
                    out[[b, dst_r, dst_c]] = arr_f64[[b, r, c]];
                }
            }
        }
    }
    Ok(PixelData::F64(out))
}

fn array_to_f64(arr: &Array) -> AgResult<Array3<f64>> {
    let shape = arr.shape();
    let (b, h, w) = (shape[0], shape[1], shape[2]);
    match arr.data() {
        TypedArray::UInt8(v) => mk(v, b, h, w, |x| x as f64),
        TypedArray::UInt16(v) => mk(v, b, h, w, |x| x as f64),
        TypedArray::UInt32(v) => mk(v, b, h, w, |x| x as f64),
        TypedArray::UInt64(v) => mk(v, b, h, w, |x| x as f64),
        TypedArray::Int8(v) => mk(v, b, h, w, |x| x as f64),
        TypedArray::Int16(v) => mk(v, b, h, w, |x| x as f64),
        TypedArray::Int32(v) => mk(v, b, h, w, |x| x as f64),
        TypedArray::Int64(v) => mk(v, b, h, w, |x| x as f64),
        TypedArray::Float32(v) => mk(v, b, h, w, |x| x as f64),
        TypedArray::Float64(v) => mk(v, b, h, w, |x| x),
        TypedArray::Bool(_) => Err(AgError::UnsupportedPixel("Bool mask array".into())),
    }
}

fn mk<T: Copy>(v: &[T], b: usize, h: usize, w: usize, f: impl Fn(T) -> f64) -> AgResult<Array3<f64>> {
    let buf: Vec<f64> = v.iter().copied().map(f).collect();
    Array3::from_shape_vec((b, h, w), buf).map_err(|e| AgError::Tiff(format!("shape: {e}")))
}

#[cfg(test)]
mod tests {
    fn pure(tw: i64, th: i64, col: i64, row: i64, w: u64, h: u64) -> ((i64, i64), (i64, i64)) {
        let ce = col + w as i64;
        let re = row + h as i64;
        (
            (col.max(0) / tw, (ce - 1).max(0) / tw),
            (row.max(0) / th, (re - 1).max(0) / th),
        )
    }

    #[test]
    fn one_tile_window_yields_one_tile() {
        let r = pure(256, 256, 10, 10, 50, 50);
        assert_eq!(r, ((0, 0), (0, 0)));
    }

    #[test]
    fn straddling_window_spans_multiple_tiles() {
        let r = pure(256, 256, 200, 200, 200, 200);
        assert_eq!(r, ((0, 1), (0, 1)));
    }
}
