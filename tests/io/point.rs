//! Public integration tests for src/io/point.rs.
#[path = "../support/fixtures.rs"]
mod fixtures;

use async_geotiff::{crs::Crs, io::CogReader};
use fixtures::{alpha_fixture, u32_at, world_fixture};
use object_store::{memory::InMemory, path::Path as ObjectPath, ObjectStoreExt};
use std::sync::Arc;

#[tokio::test]
async fn point_selects_output_bands_after_alpha_promotion() {
    for extra_samples in [&[][..], &[2], &[1, 0], &[0, 2]] {
        let store = Arc::new(InMemory::new());
        store
            .put(
                &ObjectPath::from("alpha"),
                alpha_fixture(extra_samples, false).into(),
            )
            .await
            .unwrap();
        let reader = CogReader::open_object_store(store, "alpha").await.unwrap();
        let mut expected = vec![3.0, 7.0, 11.0];
        expected.extend(
            extra_samples
                .iter()
                .filter(|&&kind| kind == 0)
                .map(|_| 41.0),
        );
        for col in 0..2 {
            let value = reader
                .point(col as f64 + 0.5, -0.5, reader.src_crs(), &[])
                .await
                .unwrap()
                .unwrap();
            assert_eq!(value.values, expected, "ExtraSamples={extra_samples:?}");
            assert_eq!(value.valid, col != 0 || extra_samples.is_empty());
        }
        let last = expected.len() as u32;
        let value = reader
            .point(1.5, -0.5, reader.src_crs(), &[last, 1, last])
            .await
            .unwrap()
            .unwrap();
        let last_value = expected[last as usize - 1];
        assert_eq!(value.values, vec![last_value, expected[0], last_value]);
        for bands in [&[0][..], &[last + 1]] {
            // Invalid selection is rejected even for a point outside the raster.
            assert!(reader
                .point(-1.0, 1.0, reader.src_crs(), bands)
                .await
                .is_err());
        }
    }
}

#[tokio::test]
async fn point_preserves_alpha_when_independent_mask_takes_precedence() {
    let store = Arc::new(InMemory::new());
    store
        .put(
            &ObjectPath::from("masked"),
            alpha_fixture(&[2], true).into(),
        )
        .await
        .unwrap();
    let reader = CogReader::open_object_store(store, "masked").await.unwrap();
    for (col, alpha, valid) in [(0, 0.0, true), (1, 255.0, false)] {
        let value = reader
            .point(col as f64 + 0.5, -0.5, reader.src_crs(), &[])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(value.values, vec![3.0, 7.0, 11.0, alpha]);
        assert_eq!(value.valid, valid);
        let alpha_value = reader
            .point(col as f64 + 0.5, -0.5, reader.src_crs(), &[4])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(alpha_value.values, vec![alpha]);
        assert_eq!(alpha_value.valid, valid);
    }
    assert!(reader
        .point(0.5, -0.5, reader.src_crs(), &[5])
        .await
        .is_err());
}

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
