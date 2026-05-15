use crate::bounds::{Bbox, GeoTransform};
use crate::crs::Crs;
use ndarray::{Array2, Array3};

/// Decoded pixel buffer in `(bands, height, width)` planar layout.
///
/// Always permuted to `(H, W, bands)` interleaved at the read/warp seam
/// before entering the render pipeline.
#[derive(Debug, Clone)]
pub enum PixelData {
    U8(Array3<u8>),
    U16(Array3<u16>),
    I16(Array3<i16>),
    U32(Array3<u32>),
    I32(Array3<i32>),
    F32(Array3<f32>),
    F64(Array3<f64>),
}

impl PixelData {
    pub fn bands(&self) -> usize {
        self.shape()[0]
    }

    pub fn height(&self) -> usize {
        self.shape()[1]
    }

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
    pub pixels: PixelData,
    pub mask: Option<Array2<bool>>, // true = valid
    pub bbox: Bbox,
    pub crs: Crs,
    pub transform: GeoTransform,
    pub nodata: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array3;

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
