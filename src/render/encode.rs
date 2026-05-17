//! PNG encoder for interleaved RGBA `Array3<u8>` of shape `(H, W, 4)`.
//!
//! Tile servers favour fast encode over small files: a 256×256 RGBA tile
//! goes back over the wire to the browser which immediately repaints. We
//! configure `image::codecs::png::PngEncoder` with `CompressionType::Fast`
//! and `FilterType::NoFilter` — roughly 3-5× faster than the default.

use crate::error::{AgError, AgResult};
use bytes::Bytes;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ColorType, ImageEncoder};
use ndarray::Array3;

pub fn encode_png(rgba: &Array3<u8>) -> AgResult<Bytes> {
    let (h, w, c) = (
        rgba.shape()[0] as u32,
        rgba.shape()[1] as u32,
        rgba.shape()[2],
    );
    if c != 4 {
        return Err(AgError::UnsupportedPixel(format!(
            "expected RGBA channels, got {c}"
        )));
    }
    let standard = rgba.as_standard_layout();
    let raw = standard
        .as_slice()
        .expect("rgba array must be contiguous after as_standard_layout");
    let mut buf = Vec::with_capacity((w * h * 4) as usize);
    let encoder =
        PngEncoder::new_with_quality(&mut buf, CompressionType::Fast, FilterType::NoFilter);
    encoder
        .write_image(raw, w, h, ColorType::Rgba8.into())
        .map_err(|e| crate::error::AgError::Tiff(format!("png encode: {e}")))?;
    Ok(Bytes::from(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array3;

    #[test]
    fn encodes_solid_red_tile() {
        let mut arr = Array3::<u8>::zeros((4, 4, 4));
        for r in 0..4 {
            for c in 0..4 {
                arr[[r, c, 0]] = 255;
                arr[[r, c, 3]] = 255;
            }
        }
        let bytes = encode_png(&arr).unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));
    }

    #[test]
    fn rejects_non_rgba_without_panicking() {
        let arr = Array3::<u8>::zeros((4, 4, 3));
        let err = encode_png(&arr).unwrap_err();
        assert!(matches!(err, AgError::UnsupportedPixel(_)));
    }
}
