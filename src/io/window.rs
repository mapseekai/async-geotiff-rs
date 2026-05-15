//! Bbox -> pixel window math + tile stitching.

use std::borrow::Borrow;

use crate::error::{AgError, AgResult};
use crate::pixel::PixelData;
use async_tiff::tags::PlanarConfiguration;
use async_tiff::{Array, ImageFileDirectory, TypedArray};
use ndarray::{s, Array2, Array3};

/// Convert pixel window to inclusive tile coordinate range
/// (tx_min..=tx_max, ty_min..=ty_max). Returns None if the IFD lacks tile dims.
pub fn tile_range_for_window(
    ifd: &ImageFileDirectory,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> Option<((u32, u32), (u32, u32))> {
    let tw = tile_width(ifd) as i64;
    let th = tile_height(ifd) as i64;
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
pub fn stitch<A: Borrow<Array>>(
    arrays: &[A],
    coords: &[(usize, usize)],
    ifd: &ImageFileDirectory,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> AgResult<PixelData> {
    let tw = tile_width(ifd) as i64;
    let th = tile_height(ifd) as i64;
    let bands = ifd.samples_per_pixel() as usize;

    let dst_h = height as usize;
    let dst_w = width as usize;
    let mut out = Array3::<f64>::zeros((bands, dst_h, dst_w));

    for ((tx, ty), arr) in coords.iter().zip(arrays.iter()) {
        let arr = arr.borrow();
        let tile_col = (*tx as i64) * tw;
        let tile_row = (*ty as i64) * th;
        let arr_f64 = array_to_f64(arr, ifd.planar_configuration())?;
        let shape = arr_f64.shape();
        let (a_h, a_w) = (shape[1] as i64, shape[2] as i64);

        // Intersect tile with requested window in world-pixel coordinates.
        let world_r0 = tile_row.max(row_off);
        let world_r1 = (tile_row + a_h).min(row_off + height as i64);
        let world_c0 = tile_col.max(col_off);
        let world_c1 = (tile_col + a_w).min(col_off + width as i64);
        if world_r1 <= world_r0 || world_c1 <= world_c0 {
            continue;
        }

        let src_r0 = (world_r0 - tile_row) as usize;
        let src_r1 = (world_r1 - tile_row) as usize;
        let src_c0 = (world_c0 - tile_col) as usize;
        let src_c1 = (world_c1 - tile_col) as usize;
        let dst_r0 = (world_r0 - row_off) as usize;
        let dst_r1 = (world_r1 - row_off) as usize;
        let dst_c0 = (world_c0 - col_off) as usize;
        let dst_c1 = (world_c1 - col_off) as usize;
        let band_take = bands.min(shape[0]);

        // Single bulk-copy per (band, sub-rect) — ndarray's `assign` is a
        // memcpy when both src and dst are contiguous along the inner axis.
        let src_view = arr_f64.slice(s![..band_take, src_r0..src_r1, src_c0..src_c1]);
        out.slice_mut(s![..band_take, dst_r0..dst_r1, dst_c0..dst_c1])
            .assign(&src_view);
    }
    Ok(PixelData::F64(out))
}

pub fn stitch_mask<A: Borrow<Array>>(
    arrays: &[A],
    coords: &[(usize, usize)],
    ifd: &ImageFileDirectory,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> AgResult<Array2<bool>> {
    let tw = tile_width(ifd) as i64;
    let th = tile_height(ifd) as i64;
    let mut out = Array2::<bool>::from_elem((height as usize, width as usize), true);
    for ((tx, ty), arr) in coords.iter().zip(arrays.iter()) {
        let arr = arr.borrow();
        let tile_col = (*tx as i64) * tw;
        let tile_row = (*ty as i64) * th;
        let mask = array_to_mask(arr, ifd.planar_configuration())?;
        let shape = mask.shape();
        let (a_h, a_w) = (shape[0] as i64, shape[1] as i64);

        let world_r0 = tile_row.max(row_off);
        let world_r1 = (tile_row + a_h).min(row_off + height as i64);
        let world_c0 = tile_col.max(col_off);
        let world_c1 = (tile_col + a_w).min(col_off + width as i64);
        if world_r1 <= world_r0 || world_c1 <= world_c0 {
            continue;
        }
        let src_r0 = (world_r0 - tile_row) as usize;
        let src_r1 = (world_r1 - tile_row) as usize;
        let src_c0 = (world_c0 - tile_col) as usize;
        let src_c1 = (world_c1 - tile_col) as usize;
        let dst_r0 = (world_r0 - row_off) as usize;
        let dst_r1 = (world_r1 - row_off) as usize;
        let dst_c0 = (world_c0 - col_off) as usize;
        let dst_c1 = (world_c1 - col_off) as usize;

        out.slice_mut(s![dst_r0..dst_r1, dst_c0..dst_c1])
            .assign(&mask.slice(s![src_r0..src_r1, src_c0..src_c1]));
    }
    Ok(out)
}

fn array_to_f64(arr: &Array, planar: PlanarConfiguration) -> AgResult<Array3<f64>> {
    let shape = arr.shape();
    let (b, h, w) = band_height_width(shape, planar);
    match arr.data() {
        TypedArray::UInt8(v) => mk(v, shape, b, h, w, planar, |x| x as f64),
        TypedArray::UInt16(v) => mk(v, shape, b, h, w, planar, |x| x as f64),
        TypedArray::UInt32(v) => mk(v, shape, b, h, w, planar, |x| x as f64),
        TypedArray::UInt64(v) => mk(v, shape, b, h, w, planar, |x| x as f64),
        TypedArray::Int8(v) => mk(v, shape, b, h, w, planar, |x| x as f64),
        TypedArray::Int16(v) => mk(v, shape, b, h, w, planar, |x| x as f64),
        TypedArray::Int32(v) => mk(v, shape, b, h, w, planar, |x| x as f64),
        TypedArray::Int64(v) => mk(v, shape, b, h, w, planar, |x| x as f64),
        TypedArray::Float32(v) => mk(v, shape, b, h, w, planar, |x| x as f64),
        TypedArray::Float64(v) => mk(v, shape, b, h, w, planar, |x| x),
        TypedArray::Bool(_) => Err(AgError::UnsupportedPixel("Bool mask array".into())),
    }
}

/// Build an `Array2<bool>` (valid-pixel mask) from the first mask band of a
/// decoded tile. Avoids the previous `iter().map(|x| x != 0).collect()` →
/// `Array3::from_shape_vec` → `index_axis` → `to_owned` chain (3 allocations
/// per tile); instead does one shape-and-axes-aware single-pass fill into a
/// pre-sized `Array2<bool>`.
fn array_to_mask(arr: &Array, planar: PlanarConfiguration) -> AgResult<Array2<bool>> {
    let shape = arr.shape();
    let (_, h, w) = band_height_width(shape, planar);
    // For planar [bands, H, W] the first band is the contiguous prefix of
    // length H*W; for chunky [H, W, bands] each pixel's first sample lives at
    // stride `bands`. `fill_mask_from` handles both by indexing into the
    // typed slice directly.
    let stride = match planar {
        PlanarConfiguration::Planar => 1,
        // chunky: samples interleaved as [s0,s1,...,sN, s0,s1,...] per pixel
        PlanarConfiguration::Chunky => shape[2],
        _ => {
            return Err(AgError::UnsupportedPixel(
                "unknown planar configuration".into(),
            ))
        }
    };
    let mut out = Array2::<bool>::from_elem((h, w), false);
    match arr.data() {
        TypedArray::Bool(v) => fill_mask(&mut out, v, h, w, stride, |&x| x),
        TypedArray::UInt8(v) => fill_mask(&mut out, v, h, w, stride, |&x| x != 0),
        TypedArray::UInt16(v) => fill_mask(&mut out, v, h, w, stride, |&x| x != 0),
        TypedArray::UInt32(v) => fill_mask(&mut out, v, h, w, stride, |&x| x != 0),
        TypedArray::UInt64(v) => fill_mask(&mut out, v, h, w, stride, |&x| x != 0),
        TypedArray::Int8(v) => fill_mask(&mut out, v, h, w, stride, |&x| x != 0),
        TypedArray::Int16(v) => fill_mask(&mut out, v, h, w, stride, |&x| x != 0),
        TypedArray::Int32(v) => fill_mask(&mut out, v, h, w, stride, |&x| x != 0),
        TypedArray::Int64(v) => fill_mask(&mut out, v, h, w, stride, |&x| x != 0),
        TypedArray::Float32(v) => fill_mask(&mut out, v, h, w, stride, |&x| x != 0.0),
        TypedArray::Float64(v) => fill_mask(&mut out, v, h, w, stride, |&x| x != 0.0),
    }
    Ok(out)
}

fn fill_mask<T: Copy, F: Fn(&T) -> bool>(
    out: &mut Array2<bool>,
    src: &[T],
    h: usize,
    w: usize,
    stride: usize,
    pred: F,
) {
    // Single linear sweep over `src` with stride `stride`. The destination is
    // row-major Array2; we use `as_slice_mut` to avoid bounds checks per cell.
    let dst = out
        .as_slice_mut()
        .expect("freshly-allocated Array2 is standard layout");
    let mut i = 0usize;
    for r in 0..h {
        for c in 0..w {
            let s = (r * w + c) * stride;
            if let Some(v) = src.get(s) {
                dst[i] = pred(v);
            }
            i += 1;
        }
    }
}

fn mk<T: Copy>(
    v: &[T],
    shape: [usize; 3],
    b: usize,
    h: usize,
    w: usize,
    planar: PlanarConfiguration,
    f: impl Fn(T) -> f64,
) -> AgResult<Array3<f64>> {
    let buf: Vec<f64> = v.iter().copied().map(f).collect();
    let arr =
        Array3::from_shape_vec(shape, buf).map_err(|e| AgError::Tiff(format!("shape: {e}")))?;
    let band_first = match planar {
        PlanarConfiguration::Planar => arr,
        PlanarConfiguration::Chunky => arr.permuted_axes([2, 0, 1]),
        _ => {
            return Err(AgError::UnsupportedPixel(
                "unknown planar configuration".into(),
            ))
        }
    };
    let actual = band_first.shape();
    if actual != [b, h, w] {
        return Err(AgError::Tiff(format!(
            "unexpected array shape {:?}, expected {:?}",
            actual,
            [b, h, w]
        )));
    }
    Ok(band_first)
}

fn band_height_width(shape: [usize; 3], planar: PlanarConfiguration) -> (usize, usize, usize) {
    match planar {
        PlanarConfiguration::Planar => (shape[0], shape[1], shape[2]),
        PlanarConfiguration::Chunky => (shape[2], shape[0], shape[1]),
        _ => (shape[2], shape[0], shape[1]),
    }
}

fn tile_width(ifd: &ImageFileDirectory) -> u32 {
    ifd.tile_width().unwrap_or_else(|| ifd.image_width())
}

fn tile_height(ifd: &ImageFileDirectory) -> u32 {
    ifd.tile_height().unwrap_or_else(|| ifd.image_height())
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
