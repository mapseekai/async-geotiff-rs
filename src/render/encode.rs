//! PNG encoder for interleaved RGBA `Array3<u8>` of shape `(H, W, 4)`.

use crate::error::AgResult;
use bytes::Bytes;
use image::{ImageBuffer, ImageFormat, Rgba};
use ndarray::Array3;
use std::io::Cursor;

pub fn encode_png(rgba: &Array3<u8>) -> AgResult<Bytes> {
    let (h, w, c) = (
        rgba.shape()[0] as u32,
        rgba.shape()[1] as u32,
        rgba.shape()[2],
    );
    assert_eq!(c, 4, "expected RGBA channels");
    let raw = rgba
        .as_standard_layout()
        .as_slice()
        .expect("rgba array must be contiguous")
        .to_vec();
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(w, h, raw).expect("buffer size mismatches dims");
    let mut buf = Vec::with_capacity((w * h * 4) as usize);
    img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)?;
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
}
