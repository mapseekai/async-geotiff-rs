//! Tile encoders for interleaved RGBA `Array3<u8>` of shape `(H, W, 4)`.
//!
//! [`encode`] dispatches on [`TileFormat`]: PNG (always available), lossless
//! WebP (`webp` feature, pure-Rust `image-webp`), and JPEG (`jpeg` feature,
//! quality 75, alpha flattened over black since JPEG has no alpha channel).
//!
//! Tile servers favour fast encode over small files: a 256×256 RGBA tile
//! goes back over the wire to the browser which immediately repaints. The PNG
//! path configures `image::codecs::png::PngEncoder` with `CompressionType::Fast`
//! and `FilterType::NoFilter` — roughly 3-5× faster than the default.

use crate::error::{AgError, AgResult};
use bytes::Bytes;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ColorType, ImageEncoder};
use ndarray::Array3;

/// Output encoding for a rendered tile.
///
/// All three variants exist regardless of cargo features so public APIs can
/// carry a requested format explicitly. Requests for formats whose feature is
/// disabled fail via [`TileFormat::ensure_available`] or [`encode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TileFormat {
    #[default]
    Png,
    Webp,
    Jpeg,
}

impl TileFormat {
    /// MIME type for the HTTP `Content-Type` header.
    pub fn content_type(&self) -> &'static str {
        match self {
            TileFormat::Png => "image/png",
            TileFormat::Webp => "image/webp",
            TileFormat::Jpeg => "image/jpeg",
        }
    }

    /// Canonical file extension (without the dot).
    pub fn extension(&self) -> &'static str {
        match self {
            TileFormat::Png => "png",
            TileFormat::Webp => "webp",
            TileFormat::Jpeg => "jpg",
        }
    }

    /// Cargo feature required for this format, if any.
    pub fn feature_name(&self) -> Option<&'static str> {
        match self {
            TileFormat::Png => None,
            TileFormat::Webp => Some("webp"),
            TileFormat::Jpeg => Some("jpeg"),
        }
    }

    /// Whether this format can be encoded by the current build.
    pub fn is_available(&self) -> bool {
        match self {
            TileFormat::Png => true,
            TileFormat::Webp => cfg!(feature = "webp"),
            TileFormat::Jpeg => cfg!(feature = "jpeg"),
        }
    }

    /// Return an error before doing tile work when the requested encoder is
    /// not present in this build.
    pub fn ensure_available(&self) -> AgResult<()> {
        match self.feature_name() {
            Some(feature) if !self.is_available() => Err(AgError::UnsupportedPixel(format!(
                "{feature} output requires building with the \"{feature}\" feature"
            ))),
            _ => Ok(()),
        }
    }
}

/// JPEG quality for lossy encoding. Tile servers favour fast small tiles over
/// archival fidelity; 75 is the `image` crate default and a sensible balance.
#[cfg(feature = "jpeg")]
const JPEG_QUALITY: u8 = 75;

/// Encode an interleaved RGBA `(H, W, 4)` tile to `format`.
///
/// PNG is always available. WebP and JPEG require their respective cargo
/// features; requesting them in a build without the feature returns an error.
pub fn encode(rgba: &Array3<u8>, format: TileFormat) -> AgResult<Bytes> {
    format.ensure_available()?;
    match format {
        TileFormat::Png => encode_png(rgba),
        TileFormat::Webp => encode_webp(rgba),
        TileFormat::Jpeg => encode_jpeg(rgba),
    }
}

/// Lossless WebP via the pure-Rust `image-webp` encoder.
#[cfg(feature = "webp")]
fn encode_webp(rgba: &Array3<u8>) -> AgResult<Bytes> {
    use image::codecs::webp::WebPEncoder;

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
    let mut buf = Vec::new();
    WebPEncoder::new_lossless(&mut buf)
        .write_image(raw, w, h, ColorType::Rgba8.into())
        .map_err(|e| AgError::Tiff(format!("webp encode: {e}")))?;
    Ok(Bytes::from(buf))
}

#[cfg(not(feature = "webp"))]
fn encode_webp(_rgba: &Array3<u8>) -> AgResult<Bytes> {
    Err(AgError::UnsupportedPixel(
        "webp output requires building with the \"webp\" feature".into(),
    ))
}

/// JPEG at [`JPEG_QUALITY`]. JPEG has no alpha channel, so transparency is
/// flattened over a black background first.
#[cfg(feature = "jpeg")]
fn encode_jpeg(rgba: &Array3<u8>) -> AgResult<Bytes> {
    use image::codecs::jpeg::JpegEncoder;

    let rgb = flatten_to_rgb_black(rgba)?;
    let (h, w) = (rgb.shape()[0] as u32, rgb.shape()[1] as u32);
    let raw = rgb
        .as_slice()
        .expect("rgb array is contiguous after flatten");
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY)
        .write_image(raw, w, h, ColorType::Rgb8.into())
        .map_err(|e| AgError::Tiff(format!("jpeg encode: {e}")))?;
    Ok(Bytes::from(buf))
}

#[cfg(not(feature = "jpeg"))]
fn encode_jpeg(_rgba: &Array3<u8>) -> AgResult<Bytes> {
    Err(AgError::UnsupportedPixel(
        "jpeg output requires building with the \"jpeg\" feature".into(),
    ))
}

/// Flatten an RGBA `(H, W, 4)` tile to RGB `(H, W, 3)` by compositing over a
/// black background: `out = src * alpha / 255`. Fully transparent → black.
#[cfg(feature = "jpeg")]
fn flatten_to_rgb_black(rgba: &Array3<u8>) -> AgResult<Array3<u8>> {
    let (h, w, c) = (rgba.shape()[0], rgba.shape()[1], rgba.shape()[2]);
    if c != 4 {
        return Err(AgError::UnsupportedPixel(format!(
            "expected RGBA channels, got {c}"
        )));
    }
    let standard = rgba.as_standard_layout();
    let raw = standard
        .as_slice()
        .expect("rgba array must be contiguous after as_standard_layout");
    let mut rgb = Vec::with_capacity(h * w * 3);
    for pixel in raw.chunks_exact(4) {
        let alpha = u32::from(pixel[3]);
        rgb.extend_from_slice(&[
            ((u32::from(pixel[0]) * alpha + 127) / 255) as u8,
            ((u32::from(pixel[1]) * alpha + 127) / 255) as u8,
            ((u32::from(pixel[2]) * alpha + 127) / 255) as u8,
        ]);
    }
    Ok(Array3::from_shape_vec((h, w, 3), rgb)
        .expect("flattened RGB length must match the output shape"))
}

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

    fn solid_rgba(r: u8, g: u8, b: u8, a: u8) -> Array3<u8> {
        let mut arr = Array3::<u8>::zeros((4, 4, 4));
        for row in 0..4 {
            for col in 0..4 {
                arr[[row, col, 0]] = r;
                arr[[row, col, 1]] = g;
                arr[[row, col, 2]] = b;
                arr[[row, col, 3]] = a;
            }
        }
        arr
    }

    #[test]
    fn content_type_matches_format() {
        assert_eq!(TileFormat::Png.content_type(), "image/png");
        assert_eq!(TileFormat::Webp.content_type(), "image/webp");
        assert_eq!(TileFormat::Jpeg.content_type(), "image/jpeg");
    }

    #[test]
    fn extension_matches_format() {
        assert_eq!(TileFormat::Png.extension(), "png");
        assert_eq!(TileFormat::Webp.extension(), "webp");
        assert_eq!(TileFormat::Jpeg.extension(), "jpg");
    }

    #[test]
    fn png_is_always_available() {
        assert!(TileFormat::Png.is_available());
        assert_eq!(TileFormat::Png.feature_name(), None);
    }

    #[test]
    fn encode_png_format_produces_png() {
        let arr = solid_rgba(255, 0, 0, 255);
        let bytes = encode(&arr, TileFormat::Png).unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));
    }

    #[cfg(feature = "jpeg")]
    #[test]
    fn flatten_to_rgb_black_premultiplies_alpha() {
        let half = flatten_to_rgb_black(&solid_rgba(255, 0, 0, 128)).unwrap();
        assert_eq!(half.shape(), &[4, 4, 3]);
        // round(255 * 128 / 255) = 128
        assert_eq!(half[[0, 0, 0]], 128);
        assert_eq!(half[[0, 0, 1]], 0);
        assert_eq!(half[[0, 0, 2]], 0);

        let transparent = flatten_to_rgb_black(&solid_rgba(255, 255, 255, 0)).unwrap();
        assert_eq!(transparent[[0, 0, 0]], 0);
        assert_eq!(transparent[[0, 0, 1]], 0);
        assert_eq!(transparent[[0, 0, 2]], 0);

        let opaque = flatten_to_rgb_black(&solid_rgba(10, 20, 30, 255)).unwrap();
        assert_eq!(opaque[[1, 1, 0]], 10);
        assert_eq!(opaque[[1, 1, 1]], 20);
        assert_eq!(opaque[[1, 1, 2]], 30);
    }

    #[cfg(feature = "jpeg")]
    #[test]
    fn flatten_to_rgb_black_accepts_non_standard_layout() {
        let mut rgba = Array3::<u8>::zeros((2, 3, 4));
        rgba[[1, 2, 0]] = 200;
        rgba[[1, 2, 1]] = 100;
        rgba[[1, 2, 2]] = 50;
        rgba[[1, 2, 3]] = 128;
        let transposed = rgba.permuted_axes([1, 0, 2]);

        let rgb = flatten_to_rgb_black(&transposed).unwrap();

        assert_eq!(rgb.shape(), &[3, 2, 3]);
        assert_eq!(rgb[[2, 1, 0]], 100);
        assert_eq!(rgb[[2, 1, 1]], 50);
        assert_eq!(rgb[[2, 1, 2]], 25);
    }

    #[cfg(feature = "webp")]
    #[test]
    fn encode_webp_format_produces_webp() {
        let arr = solid_rgba(0, 255, 0, 255);
        let bytes = encode(&arr, TileFormat::Webp).unwrap();
        assert!(bytes.starts_with(b"RIFF"));
        assert_eq!(&bytes[8..12], b"WEBP");
    }

    #[cfg(not(feature = "webp"))]
    #[test]
    fn encode_webp_without_feature_errors() {
        let arr = solid_rgba(0, 255, 0, 255);
        let err = encode(&arr, TileFormat::Webp).unwrap_err();
        assert!(matches!(err, AgError::UnsupportedPixel(_)));
    }

    #[cfg(feature = "jpeg")]
    #[test]
    fn encode_jpeg_format_produces_jpeg() {
        let arr = solid_rgba(0, 0, 255, 255);
        let bytes = encode(&arr, TileFormat::Jpeg).unwrap();
        assert!(bytes.starts_with(b"\xFF\xD8\xFF"));
    }

    #[cfg(not(feature = "jpeg"))]
    #[test]
    fn encode_jpeg_without_feature_errors() {
        let arr = solid_rgba(0, 0, 255, 255);
        let err = encode(&arr, TileFormat::Jpeg).unwrap_err();
        assert!(matches!(err, AgError::UnsupportedPixel(_)));
    }
}
