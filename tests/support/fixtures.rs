// Each integration target compiles only the fixture helpers it uses.
#![allow(dead_code)]

use async_geotiff::tms::{TileCoord, TileMatrixSet};
use object_store::{memory::InMemory, path::Path, ObjectStoreExt};
use std::sync::Arc;

pub(crate) fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

// A 16x16 RGB TIFF with optional ExtraSamples and an independent mask IFD.
// The first alpha sample is zero at pixel 0. The independent mask, when
// present, instead invalidates pixel 1 so tests can distinguish their precedence.
pub(crate) fn alpha_fixture(extra_samples: &[u16], with_mask: bool) -> Vec<u8> {
    type Tag = (u16, u16, u32, Vec<u8>);

    fn shorts(tag: u16, values: &[u16]) -> Tag {
        (
            tag,
            3,
            values.len() as u32,
            values.iter().flat_map(|v| v.to_le_bytes()).collect(),
        )
    }

    fn long(tag: u16, value: u32) -> Tag {
        (tag, 4, 1, value.to_le_bytes().to_vec())
    }

    fn doubles(tag: u16, values: &[f64]) -> Tag {
        (
            tag,
            12,
            values.len() as u32,
            values.iter().flat_map(|v| v.to_le_bytes()).collect(),
        )
    }

    fn append_ifd(bytes: &mut Vec<u8>, mut tags: Vec<Tag>, pixels: &[u8], has_next: bool) {
        tags.sort_unstable_by_key(|tag| tag.0);
        let table_end = bytes.len() + 2 + tags.len() * 12 + 4;
        let pixel_start = table_end
            + tags
                .iter()
                .filter(|tag| tag.3.len() > 4)
                .map(|tag| tag.3.len())
                .sum::<usize>();
        bytes.extend_from_slice(&(tags.len() as u16).to_le_bytes());
        let mut payload = Vec::new();
        for (tag, typ, count, mut value) in tags {
            if tag == 324 {
                value = (pixel_start as u32).to_le_bytes().to_vec();
            }
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&typ.to_le_bytes());
            bytes.extend_from_slice(&count.to_le_bytes());
            if value.len() <= 4 {
                value.resize(4, 0);
                bytes.extend_from_slice(&value);
            } else {
                bytes.extend_from_slice(&((table_end + payload.len()) as u32).to_le_bytes());
                payload.extend(value);
            }
        }
        let next = if has_next {
            pixel_start + pixels.len()
        } else {
            0
        };
        bytes.extend_from_slice(&(next as u32).to_le_bytes());
        bytes.extend(payload);
        bytes.extend_from_slice(pixels);
    }

    fn image_tags(bands: u16, photo: u16, pixel_bytes: u32) -> Vec<Tag> {
        vec![
            long(256, 16),
            long(257, 16),
            shorts(258, &vec![8; bands as usize]),
            shorts(259, &[1]),
            shorts(262, &[photo]),
            shorts(277, &[bands]),
            shorts(284, &[1]),
            long(322, 16),
            long(323, 16),
            long(324, 0),
            long(325, pixel_bytes),
        ]
    }

    let mut sample = vec![3, 7, 11];
    sample.extend(
        extra_samples
            .iter()
            .map(|&kind| if kind == 0 { 41 } else { 255 }),
    );
    let mut pixels = sample.repeat(16 * 16);
    if let Some(alpha) = extra_samples
        .iter()
        .position(|&kind| kind == 1 || kind == 2)
    {
        pixels[3 + alpha] = 0;
    }
    let mut tags = image_tags(sample.len() as u16, 2, pixels.len() as u32);
    if !extra_samples.is_empty() {
        tags.push(shorts(338, extra_samples));
    }
    tags.push(doubles(33550, &[1.0, 1.0, 0.0]));
    tags.push(doubles(33922, &[0.0; 6]));
    tags.push(shorts(
        34735,
        &[1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326],
    ));
    let mut bytes = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
    append_ifd(&mut bytes, tags, &pixels, with_mask);
    if with_mask {
        let mut mask = vec![255; 16 * 16];
        mask[1] = 0;
        let mut tags = image_tags(1, 4, mask.len() as u32);
        tags.push(long(254, 4));
        append_ifd(&mut bytes, tags, &mask, false);
    }
    bytes
}

// Adapt the checked-in tiny RGB TIFF to exactly cover WorldCRS84Quad 0/0/0.
// Optional constant samples make expected reducer outputs independent of render helpers.
pub(crate) fn world_fixture(shift: f64, samples: Option<[u8; 3]>) -> Vec<u8> {
    georeferenced_fixture(4326, [-180.0 + shift, -90.0, shift, 90.0], samples)
}

pub(crate) fn georeferenced_fixture(
    epsg: u16,
    bounds: [f64; 4],
    samples: Option<[u8; 3]>,
) -> Vec<u8> {
    let mut bytes = include_bytes!("../fixtures/rgb_nodata.tif").to_vec();
    let mut ifd = 8;
    while ifd != 0 {
        let entries = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
        let mut size = 0;
        let mut offsets = Vec::new();
        let mut lengths = Vec::new();
        for index in 0..entries {
            let entry = ifd + 2 + 12 * index;
            let tag = u16::from_le_bytes(bytes[entry..entry + 2].try_into().unwrap());
            let count = u32_at(&bytes, entry + 4) as usize;
            let value = u32_at(&bytes, entry + 8) as usize;
            match tag {
                256 => size = value,
                324 | 325 => {
                    let location = if count == 1 { entry + 8 } else { value };
                    let values = (0..count)
                        .map(|i| u32_at(&bytes, location + 4 * i) as usize)
                        .collect();
                    if tag == 324 {
                        offsets = values;
                    } else {
                        lengths = values;
                    }
                }
                33550 => {
                    for (i, v) in [
                        (bounds[2] - bounds[0]) / size as f64,
                        (bounds[3] - bounds[1]) / size as f64,
                        0.0,
                    ]
                    .iter()
                    .enumerate()
                    {
                        bytes[value + 8 * i..value + 8 * (i + 1)].copy_from_slice(&v.to_le_bytes());
                    }
                }
                33922 => {
                    for (i, v) in [0.0, 0.0, 0.0, bounds[0], bounds[3], 0.0]
                        .iter()
                        .enumerate()
                    {
                        bytes[value + 8 * i..value + 8 * (i + 1)].copy_from_slice(&v.to_le_bytes());
                    }
                }
                34735 => {
                    for (key, values) in [
                        (1024u16, (1024u16, if epsg == 4326 { 2u16 } else { 1u16 })),
                        (2048, (if epsg == 4326 { 2048 } else { 3072 }, epsg)),
                    ] {
                        for offset in (value + 8..value + count * 2).step_by(8) {
                            if u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
                                == key
                            {
                                bytes[offset..offset + 2].copy_from_slice(&values.0.to_le_bytes());
                                bytes[offset + 6..offset + 8]
                                    .copy_from_slice(&values.1.to_le_bytes());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(samples) = samples {
            for (offset, len) in offsets.into_iter().zip(lengths) {
                for pixel in bytes[offset..offset + len].chunks_exact_mut(3) {
                    pixel.copy_from_slice(&samples);
                }
            }
        }
        ifd = u32_at(&bytes, ifd + 2 + 12 * entries) as usize;
    }
    bytes
}

pub(crate) fn coord() -> TileCoord {
    TileCoord {
        tms: TileMatrixSet::WorldCrs84Quad,
        z: 0,
        x: 0,
        y: 0,
    }
}

pub(crate) async fn fixture_store() -> Arc<InMemory> {
    let store = Arc::new(InMemory::new());
    for name in ["a", "b"] {
        store
            .put(
                &Path::from(name),
                bytes::Bytes::from_static(include_bytes!("../fixtures/rgb_nodata.tif")).into(),
            )
            .await
            .unwrap();
    }
    store
}

pub(crate) fn local_coord() -> TileCoord {
    TileCoord {
        tms: TileMatrixSet::WorldCrs84Quad,
        z: 10,
        x: 1081,
        y: 398,
    }
}
