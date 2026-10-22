// Each integration target compiles only the fixture helpers it uses.
#![allow(dead_code)]

use async_geotiff::tms::{TileCoord, TileMatrixSet};
use object_store::{memory::InMemory, path::Path, ObjectStoreExt};
use std::sync::Arc;

pub(crate) fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

// Adapt the checked-in tiny RGB TIFF to exactly cover WorldCRS84Quad 0/0/0.
// Optional constant samples make expected reducer outputs independent of render helpers.
pub(crate) fn world_fixture(shift: f64, samples: Option<[u8; 3]>) -> Vec<u8> {
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
                    for (i, v) in [180.0 / size as f64, 180.0 / size as f64, 0.0]
                        .iter()
                        .enumerate()
                    {
                        bytes[value + 8 * i..value + 8 * (i + 1)].copy_from_slice(&v.to_le_bytes());
                    }
                }
                33922 => {
                    for (i, v) in [0.0, 0.0, 0.0, -180.0 + shift, 90.0, 0.0]
                        .iter()
                        .enumerate()
                    {
                        bytes[value + 8 * i..value + 8 * (i + 1)].copy_from_slice(&v.to_le_bytes());
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
