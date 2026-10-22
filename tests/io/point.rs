//! Public integration tests for src/io/point.rs.
#[path = "../support/fixtures.rs"]
mod fixtures;

use async_geotiff::{crs::Crs, io::CogReader};
use fixtures::{u32_at, world_fixture};
use object_store::{memory::InMemory, path::Path as ObjectPath, ObjectStoreExt};
use std::sync::Arc;

#[tokio::test]
async fn point_masks_infinite_nodata() {
    // Append F32 tile payloads, preserving all existing directory positions.
    let mut bytes = world_fixture(0.0, Some([0, 10, 20]));
    let mut ifd = 8;
    while ifd != 0 {
        let entries = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
        let mut offsets = (0, 0);
        let mut counts = 0;
        for index in 0..entries {
            let entry = ifd + 2 + 12 * index;
            let tag = u16::from_le_bytes(bytes[entry..entry + 2].try_into().unwrap());
            let count = u32_at(&bytes, entry + 4) as usize;
            let value = u32_at(&bytes, entry + 8) as usize;
            match tag {
                258 | 339 => {
                    let field: u16 = if tag == 258 { 32 } else { 3 };
                    for b in 0..count {
                        bytes[value + 2 * b..value + 2 * b + 2]
                            .copy_from_slice(&field.to_le_bytes());
                    }
                }
                324 => offsets = (if count == 1 { entry + 8 } else { value }, count),
                325 => counts = if count == 1 { entry + 8 } else { value },
                42113 => {
                    bytes[entry + 4..entry + 8].copy_from_slice(&4u32.to_le_bytes());
                    bytes[entry + 8..entry + 12].copy_from_slice(b"inf\0");
                }
                _ => {}
            }
        }
        for tile in 0..offsets.1 {
            let offset = u32_at(&bytes, offsets.0 + 4 * tile) as usize;
            let len = u32_at(&bytes, counts + 4 * tile) as usize;
            let data: Vec<u8> = bytes[offset..offset + len]
                .iter()
                .flat_map(|&v| if v == 0 { f32::INFINITY } else { f32::from(v) }.to_le_bytes())
                .collect();
            let new_offset = bytes.len() as u32;
            bytes[offsets.0 + 4 * tile..offsets.0 + 4 * tile + 4]
                .copy_from_slice(&new_offset.to_le_bytes());
            bytes[counts + 4 * tile..counts + 4 * tile + 4]
                .copy_from_slice(&(data.len() as u32).to_le_bytes());
            bytes.extend(data);
        }
        ifd = u32_at(&bytes, ifd + 2 + 12 * entries) as usize;
    }
    let store = Arc::new(InMemory::new());
    store
        .put(&ObjectPath::from("float"), bytes.into())
        .await
        .unwrap();
    let reader = CogReader::open_object_store(store, "float").await.unwrap();
    let value = reader
        .point(-179.0, 89.0, &Crs::epsg(4326), &[1])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(value.values, vec![f64::INFINITY]);
    assert!(!value.valid);
    assert!(
        reader
            .point(-179.0, 89.0, &Crs::epsg(4326), &[2])
            .await
            .unwrap()
            .unwrap()
            .valid
    );
}
