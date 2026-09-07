//! Codec output is bounded by the encoded TIFF tile dimensions before
//! async-tiff applies predictors or constructs its pixel array.

use crate::error::{AgError, AgResult};
use async_tiff::decoder::{Decoder, DecoderRegistry};
use async_tiff::error::{AsyncTiffError, AsyncTiffResult};
use async_tiff::tags::{Compression, PhotometricInterpretation};
use async_tiff::{Array, Tile};
use bytes::Bytes;
use std::io::{Cursor, Read};

#[derive(Debug)]
struct BoundedDecoder {
    compression: Compression,
    dimensions: [u32; 2],
}

impl Decoder for BoundedDecoder {
    fn decode_tile(
        &self,
        buffer: Bytes,
        photometric: PhotometricInterpretation,
        jpeg_tables: Option<&[u8]>,
        samples: u16,
        bits: u16,
        _lerc_parameters: Option<&[u32]>,
    ) -> AsyncTiffResult<Vec<u8>> {
        let expected = u64::from(self.dimensions[0])
            .checked_mul(u64::from(self.dimensions[1]))
            .and_then(|n| n.checked_mul(u64::from(samples)))
            .and_then(|n| n.checked_mul(u64::from(bits)))
            .filter(|&n| n > 0)
            .and_then(|n| usize::try_from(n.div_ceil(8)).ok())
            .ok_or_else(|| AsyncTiffError::General("invalid decoded tile size".into()))?;
        let decoded = match self.compression {
            Compression::None => {
                if buffer.len() != expected {
                    return Err(size_error(expected, buffer.len()));
                }
                buffer.to_vec()
            }
            Compression::Deflate | Compression::OldDeflate => bounded_read(
                flate2::bufread::ZlibDecoder::new(Cursor::new(buffer)),
                expected,
            )?,
            Compression::ZSTD => {
                // Single-pass decoding uses the caller's output as history;
                // unlike streaming, it does not allocate the advertised window.
                // A legal frame can advertise a window larger than this tile.
                let mut output = output_buffer(expected)?;
                output.resize(expected, 0);
                let written = zstd::bulk::Decompressor::new()?
                    .decompress_to_buffer(&buffer, output.as_mut_slice())?;
                output.truncate(written);
                output
            }
            Compression::LZW => decode_lzw(&buffer, expected)?,
            Compression::ModernJPEG => {
                let mut tile_reader = Cursor::new(buffer);
                let reader: Box<dyn Read + '_> = match jpeg_tables {
                    Some(tables) => {
                        if tables.len() < 2 {
                            return Err(AsyncTiffError::General("truncated JPEG tables".into()));
                        }
                        tile_reader.read_exact(&mut [0; 2])?;
                        Box::new(Cursor::new(&tables[..tables.len() - 2]).chain(tile_reader))
                    }
                    None => Box::new(tile_reader),
                };
                let mut decoder = jpeg_decoder::Decoder::new(reader);
                decoder.set_max_decoding_buffer_size(expected);
                decoder.set_color_transform(match photometric {
                    PhotometricInterpretation::RGB => jpeg_decoder::ColorTransform::RGB,
                    PhotometricInterpretation::WhiteIsZero
                    | PhotometricInterpretation::BlackIsZero
                    | PhotometricInterpretation::TransparencyMask => {
                        jpeg_decoder::ColorTransform::None
                    }
                    PhotometricInterpretation::CMYK => jpeg_decoder::ColorTransform::CMYK,
                    PhotometricInterpretation::YCbCr => jpeg_decoder::ColorTransform::YCbCr,
                    _ => {
                        return Err(AsyncTiffError::General(
                            "unsupported JPEG photometric interpretation".into(),
                        ))
                    }
                });
                // jpeg-decoder's output limit is checked after coefficient allocation.
                // Inspect the SOF dimensions first, while only parsing metadata.
                decoder.read_info()?;
                let info = decoder
                    .info()
                    .ok_or_else(|| AsyncTiffError::General("missing JPEG frame".into()))?;
                let actual = usize::from(info.width)
                    .checked_mul(usize::from(info.height))
                    .and_then(|n| n.checked_mul(info.pixel_format.pixel_bytes()));
                if [u32::from(info.width), u32::from(info.height)] != self.dimensions
                    || actual != Some(expected)
                {
                    return Err(AsyncTiffError::General(
                        "JPEG frame does not match TIFF tile dimensions and samples".into(),
                    ));
                }
                decoder.decode()?
            }
            compression => {
                return Err(AsyncTiffError::General(format!(
                    "unsupported bounded codec {compression:?}"
                )))
            }
        };
        if decoded.len() != expected {
            return Err(size_error(expected, decoded.len()));
        }
        Ok(decoded)
    }
}

fn size_error(expected: usize, actual: usize) -> AsyncTiffError {
    AsyncTiffError::General(format!(
        "decoded tile size {actual} differs from declared {expected}"
    ))
}

fn output_buffer(expected: usize) -> AsyncTiffResult<Vec<u8>> {
    let limit = expected
        .checked_add(1)
        .ok_or_else(|| AsyncTiffError::General("decoded tile size overflow".into()))?;
    let mut output = Vec::new();
    output.try_reserve_exact(limit).map_err(|_| {
        AsyncTiffError::General("decoded tile allocation exceeds available memory".into())
    })?;
    Ok(output)
}

fn bounded_read(reader: impl Read, expected: usize) -> AsyncTiffResult<Vec<u8>> {
    let mut output = output_buffer(expected)?;
    reader.take(expected as u64 + 1).read_to_end(&mut output)?;
    Ok(output)
}

fn decode_lzw(buffer: &[u8], expected: usize) -> AsyncTiffResult<Vec<u8>> {
    let mut output = output_buffer(expected)?;
    output.resize(expected + 1, 0);
    let mut decoder = weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8);
    let (mut input_pos, mut output_pos) = (0, 0);
    loop {
        let result = decoder.decode_bytes(&buffer[input_pos..], &mut output[output_pos..]);
        input_pos += result.consumed_in;
        output_pos += result.consumed_out;
        if output_pos > expected {
            return Err(size_error(expected, output_pos));
        }
        match result.status {
            Ok(weezl::LzwStatus::Done) => {
                output.truncate(output_pos);
                return Ok(output);
            }
            Ok(weezl::LzwStatus::Ok) => {}
            Ok(weezl::LzwStatus::NoProgress) | Err(_) => {
                return Err(AsyncTiffError::General(
                    "invalid or truncated LZW stream".into(),
                ))
            }
        }
    }
}

/// Decode one fetched native tile under its declared dimension/output bound.
/// Callers reserve working bytes and hold their CPU permit around this call.
pub(crate) fn decode_one(tile: Tile, dimensions: [u32; 2]) -> AgResult<Array> {
    let compression = tile.compression_method();
    let mut registry = DecoderRegistry::empty();
    registry.as_mut().insert(
        compression,
        Box::new(BoundedDecoder {
            compression,
            dimensions,
        }),
    );
    tile.decode(&registry)
        .map_err(|e| AgError::Tiff(format!("decode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_tiff::tags::{Compression, PhotometricInterpretation};
    use bytes::Bytes;
    use std::io::Write;

    fn decode(
        compression: Compression,
        pixels: usize,
        buffer: Vec<u8>,
    ) -> async_tiff::error::AsyncTiffResult<Vec<u8>> {
        BoundedDecoder {
            compression,
            dimensions: [pixels as u32, 1],
        }
        .decode_tile(
            Bytes::from(buffer),
            PhotometricInterpretation::BlackIsZero,
            None,
            1,
            8,
            None,
        )
    }

    fn deflate(data: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn deflate_bomb_stops_at_declared_output() {
        let compressed = deflate(&vec![0; 1024 * 1024]);
        assert!(compressed.len() < 8192);
        assert!(decode(Compression::Deflate, 32, compressed).is_err());
    }

    #[test]
    fn zstd_bomb_stops_at_declared_output() {
        let compressed = zstd::bulk::compress(&vec![0; 1024 * 1024], 1).unwrap();
        assert!(compressed.len() < 8192);
        assert!(decode(Compression::ZSTD, 32, compressed).is_err());
    }

    #[test]
    fn zstd_stream_window_can_exceed_tile_output_size() {
        let data: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        encoder.window_log(23).unwrap();
        encoder.include_contentsize(false).unwrap();
        encoder.write_all(&data).unwrap();
        let compressed = encoder.finish().unwrap();
        assert_eq!(
            decode(Compression::ZSTD, data.len(), compressed).unwrap(),
            data
        );
    }

    #[test]
    fn corrupt_lzw_returns_error_without_panicking() {
        let result = std::panic::catch_unwind(|| decode(Compression::LZW, 32, vec![0xff; 8]));
        assert!(matches!(result, Ok(Err(_))));
    }

    #[test]
    fn short_uncompressed_data_is_rejected_before_predictor() {
        assert!(decode(Compression::None, 32, vec![0; 31]).is_err());
    }

    #[test]
    fn valid_lossless_codecs_preserve_bytes() {
        let data: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
        let lzw = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
            .encode(&data)
            .unwrap();
        for (compression, compressed) in [
            (Compression::None, data.clone()),
            (Compression::Deflate, deflate(&data)),
            (Compression::OldDeflate, deflate(&data)),
            (Compression::ZSTD, zstd::bulk::compress(&data, 1).unwrap()),
            (Compression::LZW, lzw),
        ] {
            assert_eq!(
                decode(compression, data.len(), compressed).unwrap(),
                data,
                "{compression:?}"
            );
        }
    }
    #[test]
    fn lzw_bomb_stops_at_declared_output() {
        let compressed = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
            .encode(&vec![0; 1024 * 1024])
            .unwrap();
        assert!(compressed.len() < 8192);
        assert!(decode(Compression::LZW, 32, compressed).is_err());
    }

    #[test]
    fn jpeg_header_dimensions_checked_before_entropy_allocation() {
        let header = vec![
            0xff, 0xd8, 0xff, 0xc0, 0, 17, 8, 0x10, 0, 0x10, 0, 3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11,
            0, 0xff, 0xd9,
        ];
        let decoder = BoundedDecoder {
            compression: Compression::ModernJPEG,
            dimensions: [8, 8],
        };
        let error = decoder
            .decode_tile(
                Bytes::from(header),
                PhotometricInterpretation::YCbCr,
                None,
                3,
                8,
                None,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("JPEG frame does not match"),
            "{error}"
        );
    }

    #[test]
    fn truncated_jpeg_tables_return_error_without_panicking() {
        for tables in [&[][..], &[0xff][..]] {
            let result = std::panic::catch_unwind(|| {
                BoundedDecoder {
                    compression: Compression::ModernJPEG,
                    dimensions: [1, 1],
                }
                .decode_tile(
                    Bytes::from_static(&[0xff, 0xd8]),
                    PhotometricInterpretation::BlackIsZero,
                    Some(tables),
                    1,
                    8,
                    None,
                )
            });
            assert!(matches!(result, Ok(Err(_))));
        }
    }

    #[cfg(feature = "jpeg")]
    #[test]
    fn jpeg_pixels_and_shared_tables_decode_with_bounds() {
        let pixels = [10u8, 90, 200].repeat(64);
        let mut encoded = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, 100)
            .encode(&pixels, 8, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        let decoder = BoundedDecoder {
            compression: Compression::ModernJPEG,
            dimensions: [8, 8],
        };
        let expected = decoder
            .decode_tile(
                Bytes::from(encoded.clone()),
                PhotometricInterpretation::YCbCr,
                None,
                3,
                8,
                None,
            )
            .unwrap();
        assert_eq!(expected.len(), pixels.len());
        assert!(expected
            .iter()
            .zip(&pixels)
            .all(|(&a, &b)| a.abs_diff(b) <= 3));
        // Move DQT/DHT segments into TIFF JPEGTables; each stream has its own SOI.
        let mut tables = vec![0xff, 0xd8];
        let mut tile = vec![0xff, 0xd8];
        let mut pos = 2;
        while encoded[pos + 1] != 0xda {
            let len = usize::from(u16::from_be_bytes([encoded[pos + 2], encoded[pos + 3]])) + 2;
            if matches!(encoded[pos + 1], 0xdb | 0xc4) {
                tables.extend_from_slice(&encoded[pos..pos + len]);
            } else {
                tile.extend_from_slice(&encoded[pos..pos + len]);
            }
            pos += len;
        }
        tables.extend_from_slice(&[0xff, 0xd9]);
        tile.extend_from_slice(&encoded[pos..]);
        let actual = decoder
            .decode_tile(
                Bytes::from(tile),
                PhotometricInterpretation::YCbCr,
                Some(&tables),
                3,
                8,
                None,
            )
            .unwrap();
        assert_eq!(actual, expected);
    }
    #[test]
    fn sample_count_and_bit_depth_define_output_bound() {
        let decoder = BoundedDecoder {
            compression: Compression::None,
            dimensions: [4, 2],
        };
        assert_eq!(
            decoder
                .decode_tile(
                    Bytes::from(vec![0; 48]),
                    PhotometricInterpretation::RGB,
                    None,
                    3,
                    16,
                    None
                )
                .unwrap()
                .len(),
            48
        );
        assert!(decoder
            .decode_tile(
                Bytes::from(vec![0; 47]),
                PhotometricInterpretation::RGB,
                None,
                3,
                16,
                None
            )
            .is_err());
        // async-tiff calls the decoder separately with samples=1 for planar data.
        assert_eq!(
            decoder
                .decode_tile(
                    Bytes::from(vec![0; 16]),
                    PhotometricInterpretation::RGB,
                    None,
                    1,
                    16,
                    None
                )
                .unwrap()
                .len(),
            16
        );
    }

    #[test]
    fn zstd_output_bound_applies_with_allowed_history_window() {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        encoder.window_log(10).unwrap();
        encoder.write_all(&vec![0; 65536]).unwrap();
        let compressed = encoder.finish().unwrap();
        assert!(compressed.len() < 8192);
        assert!(decode(Compression::ZSTD, 32, compressed).is_err());
    }
}
