//! Bbox -> pixel window math + tile stitching.

use std::borrow::Borrow;

use crate::error::{AgError, AgResult};
use crate::pixel::PixelData;
use async_tiff::tags::PlanarConfiguration;
use async_tiff::{Array, ImageFileDirectory, TypedArray};
use ndarray::{s, Array2, Array3};

fn checked_window_end(offset: i64, len: u64, axis: &'static str) -> AgResult<i64> {
    let len = i64::try_from(len)
        .map_err(|_| AgError::Tiff(format!("{axis} window length exceeds i64")))?;
    offset
        .checked_add(len)
        .ok_or_else(|| AgError::Tiff(format!("{axis} window end overflows i64")))
}

#[derive(Clone, Copy)]
struct TileCopyWindow {
    src_r0: usize,
    src_r1: usize,
    src_c0: usize,
    src_c1: usize,
    dst_r0: usize,
    dst_r1: usize,
    dst_c0: usize,
    dst_c1: usize,
}

#[derive(Clone, Copy)]
struct TileFootprint {
    col: i64,
    row: i64,
    width: i64,
    height: i64,
}

#[derive(Clone, Copy)]
struct RequestedWindow {
    col: i64,
    row: i64,
    col_end: i64,
    row_end: i64,
}

fn tile_copy_window(tile: TileFootprint, requested: RequestedWindow) -> Option<TileCopyWindow> {
    let world_r0 = tile.row.max(requested.row);
    let world_r1 = tile.row.checked_add(tile.height)?.min(requested.row_end);
    let world_c0 = tile.col.max(requested.col);
    let world_c1 = tile.col.checked_add(tile.width)?.min(requested.col_end);
    if world_r1 <= world_r0 || world_c1 <= world_c0 {
        return None;
    }
    Some(TileCopyWindow {
        src_r0: (world_r0 - tile.row) as usize,
        src_r1: (world_r1 - tile.row) as usize,
        src_c0: (world_c0 - tile.col) as usize,
        src_c1: (world_c1 - tile.col) as usize,
        dst_r0: (world_r0 - requested.row) as usize,
        dst_r1: (world_r1 - requested.row) as usize,
        dst_c0: (world_c0 - requested.col) as usize,
        dst_c1: (world_c1 - requested.col) as usize,
    })
}

fn tile_footprint(tile_col: i64, tile_row: i64, tile_w: i64, tile_h: i64) -> TileFootprint {
    TileFootprint {
        col: tile_col,
        row: tile_row,
        width: tile_w,
        height: tile_h,
    }
}

fn requested_window(col_off: i64, row_off: i64, col_end: i64, row_end: i64) -> RequestedWindow {
    RequestedWindow {
        col: col_off,
        row: row_off,
        col_end,
        row_end,
    }
}

pub(crate) fn validate_window_shape(width: u64, height: u64) -> AgResult<()> {
    if width == 0 || height == 0 {
        return Err(AgError::Tiff("window dimensions must be non-zero".into()));
    }
    i64::try_from(width).map_err(|_| AgError::Tiff("window width exceeds i64".into()))?;
    i64::try_from(height).map_err(|_| AgError::Tiff("window height exceeds i64".into()))?;
    usize::try_from(width).map_err(|_| AgError::Tiff("window width exceeds usize".into()))?;
    usize::try_from(height).map_err(|_| AgError::Tiff("window height exceeds usize".into()))?;
    Ok(())
}

/// Plain-data view of the IFD attributes stitch needs. Extracted up front so
/// `stitch_with_info` can run inside `spawn_blocking` without holding any
/// `&ImageFileDirectory` borrow into `self`.
#[derive(Debug, Clone, Copy)]
pub struct IfdInfo {
    pub tile_width: u32,
    pub tile_height: u32,
    pub bands: usize,
    pub planar: PlanarConfiguration,
    pub image_width: u32,
    pub image_height: u32,
}

impl From<&ImageFileDirectory> for IfdInfo {
    fn from(ifd: &ImageFileDirectory) -> Self {
        Self {
            tile_width: ifd.tile_width().unwrap_or_else(|| ifd.image_width()),
            tile_height: ifd.tile_height().unwrap_or_else(|| ifd.image_height()),
            bands: ifd.samples_per_pixel() as usize,
            planar: ifd.planar_configuration(),
            image_width: ifd.image_width(),
            image_height: ifd.image_height(),
        }
    }
}

impl IfdInfo {
    fn validate(self) -> AgResult<Self> {
        if self.tile_width == 0 || self.tile_height == 0 {
            return Err(AgError::Tiff("IFD has zero tile dimensions".into()));
        }
        if self.image_width == 0 || self.image_height == 0 {
            return Err(AgError::Tiff("IFD has zero image dimensions".into()));
        }
        Ok(self)
    }
}

/// Convert pixel window to inclusive tile coordinate range
/// (tx_min..=tx_max, ty_min..=ty_max). Returns None if the IFD lacks tile dims.
pub fn tile_range_for_window(
    ifd: &ImageFileDirectory,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> Option<((u32, u32), (u32, u32))> {
    validate_window_shape(width, height).ok()?;
    let tw = i64::from(tile_width(ifd)?);
    let th = i64::from(tile_height(ifd)?);
    let col_end = checked_window_end(col_off, width, "column").ok()?;
    let row_end = checked_window_end(row_off, height, "row").ok()?;
    let tx0 = (col_off.max(0) / tw) as u32;
    let ty0 = (row_off.max(0) / th) as u32;
    let tx1 = ((col_end - 1).max(0) / tw) as u32;
    let ty1 = ((row_end - 1).max(0) / th) as u32;
    Some(((tx0, tx1), (ty0, ty1)))
}

/// Stitch the fetched tiles (in (tx, ty) iteration order) into a planar
/// (bands, h, w) buffer cropped to the requested window. The output
/// `PixelData` variant matches the source dtype — no upcast to f64.
///
/// This matters because the downstream warp uses `fast_image_resize` which
/// has native U8/U16/F32 SIMD paths; an upfront cast to f64 would burn
/// allocations + a typed loop and then need a second cast back to f32.
pub fn stitch<A: Borrow<Array>>(
    arrays: &[A],
    coords: &[(usize, usize)],
    ifd: &ImageFileDirectory,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> AgResult<PixelData> {
    stitch_with_info(
        arrays,
        coords,
        &IfdInfo::from(ifd).validate()?,
        col_off,
        row_off,
        width,
        height,
    )
}

/// Like [`stitch`] but takes a plain-data [`IfdInfo`]; used inside
/// `spawn_blocking` where holding a `&ImageFileDirectory` would tie the
/// closure to `self`. The two functions share all the per-tile work.
pub fn stitch_with_info<A: Borrow<Array>>(
    arrays: &[A],
    coords: &[(usize, usize)],
    info: &IfdInfo,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> AgResult<PixelData> {
    let planar = info.planar;
    let info = info.validate()?;
    validate_window_shape(width, height)?;
    let bands = info.bands;
    let dst_h = height as usize;
    let dst_w = width as usize;
    let tw = info.tile_width as i64;
    let th = info.tile_height as i64;
    let row_end = checked_window_end(row_off, height, "row")?;
    let col_end = checked_window_end(col_off, width, "column")?;
    let requested = requested_window(col_off, row_off, col_end, row_end);

    // Determine output dtype from the first decoded array. All tiles in an
    // IFD share dtype, so this is safe.
    let first = arrays
        .first()
        .ok_or_else(|| AgError::Tiff("stitch: no tiles".into()))?
        .borrow();

    macro_rules! stitch_for {
        ($variant:ident, $t:ty, $wrap:ident) => {{
            let mut out = Array3::<$t>::from_elem((bands, dst_h, dst_w), <$t>::default());
            for ((tx, ty), arr) in coords.iter().zip(arrays.iter()) {
                let arr = arr.borrow();
                let tile_col = (*tx as i64) * tw;
                let tile_row = (*ty as i64) * th;
                let typed: &[$t] = match arr.data() {
                    TypedArray::$variant(v) => v.as_slice(),
                    _ => {
                        return Err(AgError::Tiff(
                            "stitch: mixed dtypes across tiles in one IFD".into(),
                        ))
                    }
                };
                let shape = arr.shape();
                // Zero-copy view over the decoded tile's raw slice; `permuted_axes`
                // is also a view (no alloc). The single allocation per tile is
                // gone — only the final `assign` does a memcpy into `out`.
                let typed_view = arrange_planar_view::<$t>(typed, shape, planar)?;
                let s = typed_view.shape();
                let (a_h, a_w) = (s[1] as i64, s[2] as i64);

                let Some(win) =
                    tile_copy_window(tile_footprint(tile_col, tile_row, a_w, a_h), requested)
                else {
                    continue;
                };
                let band_take = bands.min(s[0]);
                let src_sub = typed_view.slice(s![
                    ..band_take,
                    win.src_r0..win.src_r1,
                    win.src_c0..win.src_c1
                ]);
                out.slice_mut(s![
                    ..band_take,
                    win.dst_r0..win.dst_r1,
                    win.dst_c0..win.dst_c1
                ])
                .assign(&src_sub);
            }
            Ok(PixelData::$wrap(out))
        }};
    }

    match first.data() {
        TypedArray::UInt8(_) => stitch_for!(UInt8, u8, U8),
        TypedArray::UInt16(_) => stitch_for!(UInt16, u16, U16),
        TypedArray::Int16(_) => stitch_for!(Int16, i16, I16),
        TypedArray::UInt32(_) => stitch_for!(UInt32, u32, U32),
        TypedArray::Int32(_) => stitch_for!(Int32, i32, I32),
        TypedArray::Float32(_) => stitch_for!(Float32, f32, F32),
        TypedArray::Float64(_) => stitch_for!(Float64, f64, F64),
        // Less common dtypes upcast to f32 to keep PixelData enum small.
        TypedArray::Int8(_)
        | TypedArray::UInt64(_)
        | TypedArray::Int64(_)
        | TypedArray::Bool(_) => {
            stitch_upcast_to_f32(arrays, coords, &info, col_off, row_off, width, height)
        }
    }
}

/// Owned `Array3<U>` from a typed slice via an element-wise mapping —
/// used by the upcast-to-f32 fallback where allocation is unavoidable.
fn upcast_owned<T, U: 'static + Copy>(
    src: &[T],
    shape: [usize; 3],
    planar: PlanarConfiguration,
    f: impl Fn(&T) -> U,
) -> AgResult<Array3<U>> {
    let mapped: Vec<U> = src.iter().map(f).collect();
    let arr = Array3::from_shape_vec(shape, mapped)
        .map_err(|e| AgError::Tiff(format!("upcast shape: {e}")))?;
    match planar {
        PlanarConfiguration::Planar => Ok(arr),
        PlanarConfiguration::Chunky => Ok(arr.permuted_axes([2, 0, 1])),
        _ => Err(AgError::UnsupportedPixel(
            "unknown planar configuration".into(),
        )),
    }
}

/// Zero-copy view of a typed decoded-tile slice in planar `(bands, H, W)`
/// layout. Planar inputs view directly; chunky inputs use `permuted_axes`,
/// which is still a view (axis re-ordering only) — no allocation.
fn arrange_planar_view<'a, T: 'static>(
    src: &'a [T],
    shape: [usize; 3],
    planar: PlanarConfiguration,
) -> AgResult<ndarray::ArrayView3<'a, T>> {
    let view = ndarray::ArrayView3::<T>::from_shape(shape, src)
        .map_err(|e| AgError::Tiff(format!("stitch shape: {e}")))?;
    match planar {
        PlanarConfiguration::Planar => Ok(view),
        PlanarConfiguration::Chunky => Ok(view.permuted_axes([2, 0, 1])),
        _ => Err(AgError::UnsupportedPixel(
            "unknown planar configuration".into(),
        )),
    }
}

/// Fallback for Int8/Int64/UInt64/Bool — upcast to f32 (avoids growing
/// `PixelData` enum + lets the warp's f32 SIMD path stay native).
fn stitch_upcast_to_f32<A: Borrow<Array>>(
    arrays: &[A],
    coords: &[(usize, usize)],
    info: &IfdInfo,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> AgResult<PixelData> {
    let info = info.validate()?;
    validate_window_shape(width, height)?;
    let tw = info.tile_width as i64;
    let th = info.tile_height as i64;
    let bands = info.bands;
    let row_end = checked_window_end(row_off, height, "row")?;
    let col_end = checked_window_end(col_off, width, "column")?;
    let requested = requested_window(col_off, row_off, col_end, row_end);
    let mut out = Array3::<f32>::zeros((bands, height as usize, width as usize));
    let planar = info.planar;
    for ((tx, ty), arr) in coords.iter().zip(arrays.iter()) {
        let arr = arr.borrow();
        let tile_col = (*tx as i64) * tw;
        let tile_row = (*ty as i64) * th;
        let shape = arr.shape();
        let typed_f32: Array3<f32> = match arr.data() {
            TypedArray::Int8(v) => upcast_owned(v, shape, planar, |&x| x as f32)?,
            TypedArray::Int64(v) => upcast_owned(v, shape, planar, |&x| x as f32)?,
            TypedArray::UInt64(v) => upcast_owned(v, shape, planar, |&x| x as f32)?,
            TypedArray::Bool(v) => {
                upcast_owned(v, shape, planar, |&x| if x { 1.0f32 } else { 0.0 })?
            }
            _ => unreachable!("stitch_upcast_to_f32 called with non-upcast variant"),
        };
        let s = typed_f32.shape();
        let (a_h, a_w) = (s[1] as i64, s[2] as i64);
        let Some(win) = tile_copy_window(tile_footprint(tile_col, tile_row, a_w, a_h), requested)
        else {
            continue;
        };
        let band_take = bands.min(s[0]);
        out.slice_mut(s![
            ..band_take,
            win.dst_r0..win.dst_r1,
            win.dst_c0..win.dst_c1
        ])
        .assign(&typed_f32.slice(s![
            ..band_take,
            win.src_r0..win.src_r1,
            win.src_c0..win.src_c1
        ]));
    }
    Ok(PixelData::F32(out))
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
    stitch_mask_with_info(
        arrays,
        coords,
        &IfdInfo::from(ifd).validate()?,
        col_off,
        row_off,
        width,
        height,
    )
}

/// Plain-data version used inside `spawn_blocking`.
pub fn stitch_mask_with_info<A: Borrow<Array>>(
    arrays: &[A],
    coords: &[(usize, usize)],
    info: &IfdInfo,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> AgResult<Array2<bool>> {
    let info = info.validate()?;
    validate_window_shape(width, height)?;
    let tw = info.tile_width as i64;
    let th = info.tile_height as i64;
    let row_end = checked_window_end(row_off, height, "row")?;
    let col_end = checked_window_end(col_off, width, "column")?;
    let requested = requested_window(col_off, row_off, col_end, row_end);
    let mut out = Array2::<bool>::from_elem((height as usize, width as usize), true);
    for ((tx, ty), arr) in coords.iter().zip(arrays.iter()) {
        let arr = arr.borrow();
        let tile_col = (*tx as i64) * tw;
        let tile_row = (*ty as i64) * th;
        let mask = array_to_mask(arr, info.planar)?;
        let shape = mask.shape();
        let (a_h, a_w) = (shape[0] as i64, shape[1] as i64);

        let Some(win) = tile_copy_window(tile_footprint(tile_col, tile_row, a_w, a_h), requested)
        else {
            continue;
        };

        out.slice_mut(s![win.dst_r0..win.dst_r1, win.dst_c0..win.dst_c1])
            .assign(&mask.slice(s![win.src_r0..win.src_r1, win.src_c0..win.src_c1]));
    }
    Ok(out)
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

fn band_height_width(shape: [usize; 3], planar: PlanarConfiguration) -> (usize, usize, usize) {
    match planar {
        PlanarConfiguration::Planar => (shape[0], shape[1], shape[2]),
        PlanarConfiguration::Chunky => (shape[2], shape[0], shape[1]),
        _ => (shape[2], shape[0], shape[1]),
    }
}

fn tile_width(ifd: &ImageFileDirectory) -> Option<u32> {
    let width = ifd.tile_width().unwrap_or_else(|| ifd.image_width());
    (width > 0).then_some(width)
}

fn tile_height(ifd: &ImageFileDirectory) -> Option<u32> {
    let height = ifd.tile_height().unwrap_or_else(|| ifd.image_height());
    (height > 0).then_some(height)
}

#[cfg(test)]
mod tests {
    use super::{requested_window, tile_copy_window, tile_footprint, validate_window_shape};

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

    #[test]
    fn rejects_windows_that_cannot_fit_index_types() {
        assert!(validate_window_shape(i64::MAX as u64 + 1, 1).is_err());
        assert!(validate_window_shape(1, 0).is_err());
    }

    #[test]
    fn shared_tile_copy_window_clips_source_and_destination() {
        let win = tile_copy_window(
            tile_footprint(0, 0, 256, 256),
            requested_window(200, 200, 400, 400),
        )
        .unwrap();
        assert_eq!((win.src_c0, win.src_c1), (200, 256));
        assert_eq!((win.src_r0, win.src_r1), (200, 256));
        assert_eq!((win.dst_c0, win.dst_c1), (0, 56));
        assert_eq!((win.dst_r0, win.dst_r1), (0, 56));
    }
}
