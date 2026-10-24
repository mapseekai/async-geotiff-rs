//! Bbox -> pixel window math + tile stitching.

use std::borrow::Borrow;
use std::marker::PhantomData;

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

/// Marker for IFD dimensions that have not been checked by [`IfdInfo::validate`].
#[derive(Clone, Copy, Debug)]
pub enum UncheckedIfdInfo {}

/// Marker produced after checking that tile and image dimensions are nonzero.
#[derive(Clone, Copy, Debug)]
pub enum ValidatedIfdInfo {}

/// Owned IFD layout needed for stitching without borrowing TIFF metadata.
///
/// [`Self::validate`] checks tile and image dimensions. It does not validate
/// decoded array shapes, band counts or subsequent edits to these public fields.
#[derive(Clone, Copy, Debug)]
pub struct IfdInfo<State = UncheckedIfdInfo> {
    /// Width of one internal tile in pixels.
    pub tile_width: u32,
    /// Height of one internal tile in pixels.
    pub tile_height: u32,
    /// Number of native sample bands, including extra samples.
    pub bands: usize,
    /// Whether decoded samples are band-planar or pixel-interleaved.
    pub planar: PlanarConfiguration,
    /// Width of the full image at this IFD's resolution, in pixels.
    pub image_width: u32,
    /// Height of the full image at this IFD's resolution, in pixels.
    pub image_height: u32,
    _state: PhantomData<State>,
}

impl From<&ImageFileDirectory> for IfdInfo<UncheckedIfdInfo> {
    fn from(ifd: &ImageFileDirectory) -> Self {
        Self {
            tile_width: ifd.tile_width().unwrap_or_else(|| ifd.image_width()),
            tile_height: ifd.tile_height().unwrap_or_else(|| ifd.image_height()),
            bands: ifd.samples_per_pixel() as usize,
            planar: ifd.planar_configuration(),
            image_width: ifd.image_width(),
            image_height: ifd.image_height(),
            _state: PhantomData,
        }
    }
}

impl IfdInfo<UncheckedIfdInfo> {
    /// Store the supplied tile dimensions, band layout and image dimensions
    /// without validating or clamping them. Call [`Self::validate`] before stitching.
    pub fn from_parts(
        tile_width: u32,
        tile_height: u32,
        bands: usize,
        planar: PlanarConfiguration,
        image_width: u32,
        image_height: u32,
    ) -> Self {
        Self {
            tile_width,
            tile_height,
            bands,
            planar,
            image_width,
            image_height,
            _state: PhantomData,
        }
    }

    /// Check nonzero tile and image dimensions and return the checked marker.
    /// Band counts and decoded array shapes are not checked here.
    ///
    /// # Errors
    /// Returns [`AgError::Tiff`] if any tile or image dimension is zero.
    pub fn validate(self) -> AgResult<IfdInfo<ValidatedIfdInfo>> {
        if self.tile_width == 0 || self.tile_height == 0 {
            return Err(AgError::Tiff("IFD has zero tile dimensions".into()));
        }
        if self.image_width == 0 || self.image_height == 0 {
            return Err(AgError::Tiff("IFD has zero image dimensions".into()));
        }
        Ok(IfdInfo {
            tile_width: self.tile_width,
            tile_height: self.tile_height,
            bands: self.bands,
            planar: self.planar,
            image_width: self.image_width,
            image_height: self.image_height,
            _state: PhantomData,
        })
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
    info: &IfdInfo<ValidatedIfdInfo>,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> AgResult<PixelData> {
    let planar = info.planar;
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
            stitch_upcast_to_f32(arrays, coords, info, col_off, row_off, width, height)
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
    info: &IfdInfo<ValidatedIfdInfo>,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> AgResult<PixelData> {
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

/// Stitch the first sample band of each mask tile into a `(height, width)`
/// validity array, treating nonzero values as valid. `coords` supplies `(tx, ty)`
/// for each matching array. The requested window uses IFD pixel coordinates;
/// cells not covered by a supplied tile retain `true`.
///
/// # Errors
/// Returns an error for zero or unrepresentable window dimensions, overflowing
/// window endpoints, zero IFD dimensions or unsupported sample layout.
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
    info: &IfdInfo<ValidatedIfdInfo>,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> AgResult<Array2<bool>> {
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
        let shape = arr.shape();
        let (a_h, a_w, stride) = match info.planar {
            PlanarConfiguration::Planar => (shape[1], shape[2], 1),
            PlanarConfiguration::Chunky => (shape[0], shape[1], shape[2]),
            _ => {
                return Err(AgError::UnsupportedPixel(
                    "unknown planar configuration".into(),
                ))
            }
        };

        let Some(win) = tile_copy_window(
            tile_footprint(tile_col, tile_row, a_w as i64, a_h as i64),
            requested,
        ) else {
            continue;
        };

        macro_rules! fill {
            ($values:expr, $predicate:expr) => {
                fill_mask_window(&mut out, $values, a_w, stride, win, $predicate)
            };
        }
        match arr.data() {
            TypedArray::Bool(v) => fill!(v, |&x| x),
            TypedArray::UInt8(v) => fill!(v, |&x| x != 0),
            TypedArray::UInt16(v) => fill!(v, |&x| x != 0),
            TypedArray::UInt32(v) => fill!(v, |&x| x != 0),
            TypedArray::UInt64(v) => fill!(v, |&x| x != 0),
            TypedArray::Int8(v) => fill!(v, |&x| x != 0),
            TypedArray::Int16(v) => fill!(v, |&x| x != 0),
            TypedArray::Int32(v) => fill!(v, |&x| x != 0),
            TypedArray::Int64(v) => fill!(v, |&x| x != 0),
            TypedArray::Float32(v) => fill!(v, |&x| x != 0.0),
            TypedArray::Float64(v) => fill!(v, |&x| x != 0.0),
        }
    }
    Ok(out)
}

/// Fill only the requested part of one decoded mask tile into the final mask.
///
/// Valid decoded arrays take the checked-row fast path: one bounds check per
/// source row followed by a linear destination write. The fallback preserves
/// the old behavior for malformed, truncated arrays, where missing cells were
/// materialized as `false` before the crop was copied.
fn fill_mask_window<T, F: Fn(&T) -> bool>(
    out: &mut Array2<bool>,
    src: &[T],
    src_width: usize,
    stride: usize,
    win: TileCopyWindow,
    pred: F,
) {
    let rows = win.src_r1 - win.src_r0;
    let cols = win.src_c1 - win.src_c0;
    debug_assert_eq!(rows, win.dst_r1 - win.dst_r0);
    debug_assert_eq!(cols, win.dst_c1 - win.dst_c0);
    if rows == 0 || cols == 0 {
        return;
    }

    let dst_width = out.ncols();
    let dst = out
        .as_slice_mut()
        .expect("stitch mask output is freshly allocated in standard layout");

    for row in 0..rows {
        let src_start = ((win.src_r0 + row) * src_width + win.src_c0) * stride;
        let src_span = (cols - 1) * stride + 1;
        let dst_start = (win.dst_r0 + row) * dst_width + win.dst_c0;
        let dst_row = &mut dst[dst_start..dst_start + cols];

        let src_row = if stride == 0 {
            None
        } else {
            src.get(src_start..src_start + src_span)
        };
        if let Some(src_row) = src_row {
            for (dst_cell, src_cell) in dst_row.iter_mut().zip(src_row.iter().step_by(stride)) {
                *dst_cell = pred(src_cell);
            }
        } else {
            for (col, dst_cell) in dst_row.iter_mut().enumerate() {
                *dst_cell = src
                    .get(src_start + col * stride)
                    .map(&pred)
                    .unwrap_or(false);
            }
        }
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
    use ndarray::{arr2, Array2};

    use super::{
        fill_mask_window, requested_window, tile_copy_window, tile_footprint,
        validate_window_shape, TileCopyWindow,
    };

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

    #[test]
    fn fill_mask_window_crops_planar_interior_into_destination() {
        let src = [
            1u8, 0, 1, 0, 1, // row 0
            0, 1, 0, 1, 0, // row 1
            1, 1, 0, 0, 1, // row 2
            0, 0, 1, 1, 0, // row 3
        ];
        let mut out = Array2::from_elem((4, 6), true);
        let win = TileCopyWindow {
            src_r0: 1,
            src_r1: 3,
            src_c0: 1,
            src_c1: 4,
            dst_r0: 1,
            dst_r1: 3,
            dst_c0: 2,
            dst_c1: 5,
        };

        fill_mask_window(&mut out, &src, 5, 1, win, |&value| value != 0);

        assert_eq!(
            out,
            arr2(&[
                [true, true, true, true, true, true],
                [true, true, true, false, true, true],
                [true, true, true, false, false, true],
                [true, true, true, true, true, true],
            ])
        );
    }

    #[test]
    fn fill_mask_window_uses_first_chunky_band_only() {
        let src = [
            0u8, 9, 9, 1, 9, 9, 0, 9, 9, // row 0
            1, 9, 9, 0, 9, 9, 1, 9, 9, // row 1
        ];
        let mut out = Array2::from_elem((2, 3), true);
        let win = TileCopyWindow {
            src_r0: 0,
            src_r1: 2,
            src_c0: 0,
            src_c1: 3,
            dst_r0: 0,
            dst_r1: 2,
            dst_c0: 0,
            dst_c1: 3,
        };

        fill_mask_window(&mut out, &src, 3, 3, win, |&value| value != 0);

        assert_eq!(out, arr2(&[[false, true, false], [true, false, true]]));
    }

    #[test]
    fn fill_mask_window_places_short_edge_tile_for_negative_request() {
        let src = [1u8, 0, 0, 1];
        let requested = requested_window(-1, -1, 3, 3);
        let win = tile_copy_window(tile_footprint(0, 0, 2, 2), requested).unwrap();
        let mut out = Array2::from_elem((4, 4), true);

        fill_mask_window(&mut out, &src, 2, 1, win, |&value| value != 0);

        assert_eq!(
            out,
            arr2(&[
                [true, true, true, true],
                [true, true, false, true],
                [true, false, true, true],
                [true, true, true, true],
            ])
        );
    }

    #[test]
    fn fill_mask_window_treats_truncated_source_cells_as_false() {
        let src = [1u8, 0, 1, 0];
        let mut out = Array2::from_elem((2, 3), true);
        let win = TileCopyWindow {
            src_r0: 0,
            src_r1: 2,
            src_c0: 0,
            src_c1: 3,
            dst_r0: 0,
            dst_r1: 2,
            dst_c0: 0,
            dst_c1: 3,
        };

        fill_mask_window(&mut out, &src, 3, 1, win, |&value| value != 0);

        assert_eq!(out, arr2(&[[true, false, true], [false, false, false]]));
    }
}
