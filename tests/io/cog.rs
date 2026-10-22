//! Public integration tests for src/io/cog.rs.
use async_geotiff::io::CogReader;
use object_store::local::LocalFileSystem;
use std::sync::Arc;

#[tokio::test]
async fn internal_masks_use_their_own_block_grid() {
    for name in ["mask_16_16.tif", "mask_32_16.tif", "mask_16_32.tif"] {
        let store = Arc::new(LocalFileSystem::new_with_prefix(env!("CARGO_MANIFEST_DIR")).unwrap());
        let reader = CogReader::open_object_store(store, &format!("tests/fixtures/{name}"))
            .await
            .unwrap();
        for (col, row, width, height) in
            [(0, 0, 32, 32), (0, 0, 2, 2), (20, 20, 2, 2), (14, 14, 4, 4)]
        {
            let image = reader
                .read_window(0, col, row, width, height)
                .await
                .unwrap();
            let mask = image.mask.as_ref().unwrap();
            for ((_, x), &valid) in mask.indexed_iter() {
                assert_eq!(
                    valid,
                    col + (x as i64) < 16,
                    "{name} column {}",
                    col + x as i64
                );
            }
        }
    }
}

#[tokio::test]
async fn tiled_pixels_overview_and_nodata_match_known_samples() {
    for (name, bands, multiplier) in [
        ("rgb_nodata.tif", 3, 1),
        ("rgb_deflate.tif", 3, 1),
        ("uint16.tif", 1, 100),
        ("projected.tif", 3, 1),
    ] {
        let store = Arc::new(LocalFileSystem::new_with_prefix(env!("CARGO_MANIFEST_DIR")).unwrap());
        let reader = CogReader::open_object_store(store, &format!("tests/fixtures/{name}"))
            .await
            .expect("required checked-in fixture must open");
        assert_eq!(reader.level_count(), 2);
        for (level, size) in [(0, 64), (1, 32)] {
            let image = reader.read_window(level, 0, 0, size, size).await.unwrap();
            let pixels = image.pixels.to_f64();
            assert_eq!(pixels.shape(), &[bands, size as usize, size as usize]);
            for (x, y) in [(0, 0), (31, 15), (size as usize - 1, size as usize - 1)] {
                for band in 0..bands {
                    let expected = (1
                        + (x * (1 << level) + 3 * y * (1 << level) + band * 61) % 254)
                        * multiplier;
                    assert_eq!(
                        pixels[[band, y, x]],
                        expected as f64,
                        "{name} level {level}"
                    );
                }
            }
            assert_eq!(pixels[[0, size as usize - 1, 0]], 0.0);
            assert_eq!(image.nodata, Some(0.0));
        }
        // Crossing both tile boundaries exercises stitching independently of
        // full-image reads; each corner comes from a different source block.
        let image = reader.read_window(0, 30, 30, 4, 4).await.unwrap();
        let pixels = image.pixels.to_f64();
        for (x, y) in [(0, 0), (3, 0), (0, 3), (3, 3)] {
            assert_eq!(
                pixels[[0, y, x]],
                ((1 + (30 + x + 3 * (30 + y)) % 254) * multiplier) as f64
            );
        }
    }
}
