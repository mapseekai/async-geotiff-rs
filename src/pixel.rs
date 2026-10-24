use crate::bounds::{Bbox, GeoTransform};
use crate::crs::Crs;
use ndarray::{Array2, Array3};

/// Decoded pixel buffer in `(bands, height, width)` planar layout.
///
/// Reading, warping, and mosaic reduction retain this planar layout. Renderers
/// interleave selected channels when producing encoded image bytes.
#[derive(Debug, Clone)]
pub enum PixelData {
    /// Unsigned 8-bit samples in `(bands, height, width)` order.
    U8(Array3<u8>),
    /// Unsigned 16-bit samples in `(bands, height, width)` order.
    U16(Array3<u16>),
    /// Signed 16-bit samples in `(bands, height, width)` order.
    I16(Array3<i16>),
    /// Unsigned 32-bit samples in `(bands, height, width)` order.
    U32(Array3<u32>),
    /// Signed 32-bit samples in `(bands, height, width)` order.
    I32(Array3<i32>),
    /// 32-bit floating-point samples in `(bands, height, width)` order.
    F32(Array3<f32>),
    /// 64-bit floating-point samples in `(bands, height, width)` order.
    F64(Array3<f64>),
}

impl PixelData {
    /// Number of sample bands (the first array axis).
    pub fn bands(&self) -> usize {
        self.shape()[0]
    }

    /// Number of pixel rows (the second array axis).
    pub fn height(&self) -> usize {
        self.shape()[1]
    }

    /// Number of pixel columns (the third array axis).
    pub fn width(&self) -> usize {
        self.shape()[2]
    }

    fn shape(&self) -> &[usize] {
        match self {
            Self::U8(a) => a.shape(),
            Self::U16(a) => a.shape(),
            Self::I16(a) => a.shape(),
            Self::U32(a) => a.shape(),
            Self::I32(a) => a.shape(),
            Self::F32(a) => a.shape(),
            Self::F64(a) => a.shape(),
        }
    }

    /// View every pixel as f64 — used by the render hot path.
    /// Allocates one f64 array; caller should cache if iterating.
    pub fn to_f64(&self) -> Array3<f64> {
        match self {
            Self::U8(a) => a.mapv(|v| v as f64),
            Self::U16(a) => a.mapv(|v| v as f64),
            Self::I16(a) => a.mapv(|v| v as f64),
            Self::U32(a) => a.mapv(|v| v as f64),
            Self::I32(a) => a.mapv(|v| v as f64),
            Self::F32(a) => a.mapv(|v| v as f64),
            Self::F64(a) => a.clone(),
        }
    }

    /// Convert a single band `b` to an owned `Array2<f64>`. Cheaper than
    /// [`to_f64`](Self::to_f64) when only a few of many bands are rendered: it
    /// touches one band, not the whole `(bands, H, W)` cube. Panics if `b` is
    /// out of range — callers validate `bidx` first.
    pub fn band_to_f64(&self, b: usize) -> Array2<f64> {
        use ndarray::Axis;
        macro_rules! conv {
            ($a:expr) => {
                $a.index_axis(Axis(0), b).mapv(|v| v as f64)
            };
        }
        match self {
            Self::U8(a) => conv!(a),
            Self::U16(a) => conv!(a),
            Self::I16(a) => conv!(a),
            Self::U32(a) => conv!(a),
            Self::I32(a) => conv!(a),
            Self::F32(a) => conv!(a),
            Self::F64(a) => a.index_axis(Axis(0), b).to_owned(),
        }
    }

    /// Like [`to_f64`](Self::to_f64) but borrows the buffer for the
    /// already-`f64` case (`Cow::Borrowed`) instead of cloning a full
    /// `bands×H×W` array. The scalar cross-CRS warp only reads the
    /// promoted array, so f64 COGs pay zero extra allocation.
    pub fn as_f64(&self) -> std::borrow::Cow<'_, Array3<f64>> {
        match self {
            Self::F64(a) => std::borrow::Cow::Borrowed(a),
            other => std::borrow::Cow::Owned(other.to_f64()),
        }
    }

    /// Consume the pixels, reusing the allocation when already F64.
    pub fn into_f64(self) -> Array3<f64> {
        match self {
            Self::F64(a) => a,
            other => other.to_f64(),
        }
    }

    /// Split off band `b` as an `Array2<bool>` (truthy = non-zero) and
    /// return the remaining planar `(bands-1, H, W)` buffer. Used by the
    /// CogReader to promote an `ExtraSamples=Alpha` band to a validity
    /// mask after stitching, mirroring rio-tiler's alpha-band priority.
    pub fn split_off_band(self, b: usize) -> Option<(Self, ndarray::Array2<bool>)> {
        use ndarray::{s, Axis};
        macro_rules! split {
            ($arr:expr, $wrap:ident, $nz:expr) => {{
                let bands = $arr.shape()[0];
                if b >= bands {
                    return None;
                }
                let alpha = $arr.index_axis(Axis(0), b).mapv($nz);
                let h = $arr.shape()[1];
                let w = $arr.shape()[2];
                // Drop band b: select all rows except b.
                let mut keep = Array3::default((bands - 1, h, w));
                let mut k = 0usize;
                for src_b in 0..bands {
                    if src_b == b {
                        continue;
                    }
                    keep.slice_mut(s![k, .., ..])
                        .assign(&$arr.index_axis(Axis(0), src_b));
                    k += 1;
                }
                Some((Self::$wrap(keep), alpha))
            }};
        }
        match self {
            Self::U8(a) => split!(a, U8, |v: u8| v != 0),
            Self::U16(a) => split!(a, U16, |v: u16| v != 0),
            Self::I16(a) => split!(a, I16, |v: i16| v != 0),
            Self::U32(a) => split!(a, U32, |v: u32| v != 0),
            Self::I32(a) => split!(a, I32, |v: i32| v != 0),
            Self::F32(a) => split!(a, F32, |v: f32| v != 0.0),
            Self::F64(a) => split!(a, F64, |v: f64| v != 0.0),
        }
    }
}

/// The single data-bearing type passed between io → warp → render.
#[derive(Debug, Clone)]
pub struct ImageData {
    /// Planar sample array. Readers preserve raw values; SCALE/OFFSET is not applied.
    pub pixels: PixelData,
    /// Optional `(height, width)` validity mask on the same grid as `pixels`.
    /// `true` means valid; `None` imposes no mask, though `nodata` still applies.
    pub mask: Option<Array2<bool>>,
    /// Axis-aligned extent of this image in `crs` coordinates.
    pub bbox: Bbox,
    /// Coordinate reference system for `bbox` and `transform`.
    pub crs: Crs,
    /// Maps pixel-corner coordinates on this image's grid into `crs` coordinates.
    pub transform: GeoTransform,
    /// Optional raw-sample marker for invalid pixels, before SCALE/OFFSET.
    pub nodata: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array3;

    #[test]
    fn f64_views_borrow_and_consuming_conversion_reuses_strided_storage() {
        let array = Array3::from_shape_vec((1, 2, 3), (0..6).map(f64::from).collect())
            .unwrap()
            .permuted_axes([0, 2, 1]);
        let pointer = array.as_ptr();
        let pixels = PixelData::F64(array);
        assert!(matches!(pixels.as_f64(), std::borrow::Cow::Borrowed(_)));
        assert_eq!(pixels.as_f64().as_ptr(), pointer);
        let converted = pixels.into_f64();
        assert_eq!(converted.as_ptr(), pointer);
        assert_eq!(converted[[0, 2, 1]], 5.0);

        let integer = PixelData::U16(Array3::from_elem((1, 2, 3), 1234));
        assert!(matches!(integer.as_f64(), std::borrow::Cow::Owned(_)));
        assert!(integer.into_f64().iter().all(|&v| v == 1234.0));
    }

    #[test]
    fn pixel_data_shape_helpers() {
        let arr = Array3::<u8>::zeros((3, 256, 256));
        let p = PixelData::U8(arr);
        assert_eq!(p.bands(), 3);
        assert_eq!(p.height(), 256);
        assert_eq!(p.width(), 256);
    }

    #[test]
    fn to_f64_preserves_values() {
        let arr = Array3::<u16>::from_shape_vec((1, 1, 2), vec![100, 200]).unwrap();
        let p = PixelData::U16(arr);
        let f = p.to_f64();
        assert_eq!(f[[0, 0, 0]], 100.0);
        assert_eq!(f[[0, 0, 1]], 200.0);
    }
}
