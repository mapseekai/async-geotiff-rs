//! CogReader regression tests.

use super::*;

#[tokio::test]
async fn read_concurrency_configuration_clamps_both_bounds() {
    for (configured, expected) in [(0, 1), (usize::MAX, tokio::sync::Semaphore::MAX_PERMITS)] {
        let config = ReaderConfig {
            max_concurrent_reads: configured,
            ..ReaderConfig::default()
        };
        let store = Arc::new(object_store::local::LocalFileSystem::new_with_prefix(".").unwrap());
        let reader = CogReader::builder()
            .reader_config(config)
            .open_object_store(store, "tests/fixtures/rgb_nodata.tif")
            .await
            .unwrap();
        assert_eq!(reader.reads.available_permits(), expected);
        assert!(reader.read_window(0, 0, 0, 1, 1).await.is_ok());
    }
}

// A small uncompressed TIFF writer keeps the alpha regression independent of
// optional local datasets. Four 256x256 blocks form a 512x512 RGBA window.
fn rgba_tiff() -> Vec<u8> {
    let shorts = |values: &[u16]| {
        values
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>()
    };
    let longs = |values: &[u32]| {
        values
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>()
    };
    let doubles = |values: &[f64]| {
        values
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>()
    };
    let block_bytes = 256 * 256 * 4;
    let mut tags = vec![
        (256u16, 4u16, 1u32, longs(&[512])),
        (257, 4, 1, longs(&[512])),
        (258, 3, 4, shorts(&[8; 4])),
        (259, 3, 1, shorts(&[1])),
        (262, 3, 1, shorts(&[2])),
        (277, 3, 1, shorts(&[4])),
        (284, 3, 1, shorts(&[1])),
        (322, 4, 1, longs(&[256])),
        (323, 4, 1, longs(&[256])),
        (324, 4, 4, longs(&[0; 4])),
        (325, 4, 4, longs(&[block_bytes; 4])),
        (338, 3, 1, shorts(&[2])),
        (33550, 12, 3, doubles(&[1., 1., 0.])),
        (33922, 12, 6, doubles(&[0., 0., 0., 0., 0., 0.])),
    ];
    let table_end = 8 + 2 + tags.len() * 12 + 4;
    let data_start = table_end
        + tags
            .iter()
            .filter(|tag| tag.3.len() > 4)
            .map(|tag| tag.3.len())
            .sum::<usize>();
    let offsets: Vec<u32> = (0..4)
        .map(|i| data_start as u32 + i * block_bytes)
        .collect();
    tags.iter_mut().find(|tag| tag.0 == 324).unwrap().3 = longs(&offsets);
    let mut bytes = b"II".to_vec();
    bytes.extend(42u16.to_le_bytes());
    bytes.extend(8u32.to_le_bytes());
    bytes.extend((tags.len() as u16).to_le_bytes());
    let mut payload = Vec::new();
    for (tag, kind, count, data) in tags {
        bytes.extend(tag.to_le_bytes());
        bytes.extend(kind.to_le_bytes());
        bytes.extend(count.to_le_bytes());
        if data.len() <= 4 {
            let mut inline = [0; 4];
            inline[..data.len()].copy_from_slice(&data);
            bytes.extend(inline);
        } else {
            bytes.extend(((table_end + payload.len()) as u32).to_le_bytes());
            payload.extend(data);
        }
    }
    bytes.extend(0u32.to_le_bytes());
    bytes.extend(payload);
    for _ in 0..4 * 256 * 256 {
        bytes.extend([3, 7, 11, 255]);
    }
    bytes[data_start + 3] = 0;
    bytes
}

#[tokio::test]
async fn alpha_assembly_obeys_cpu_budget_and_queued_cancellation() {
    use crate::request::{RequestLimiter, RequestOptions};
    use object_store::ObjectStoreExt;
    let store = Arc::new(object_store::memory::InMemory::new());
    store
        .put(&Path::from("rgba.tif"), rgba_tiff().into())
        .await
        .unwrap();
    let cpu = CpuLimiter::new(1);
    let budget = crate::resource::WorkBudget::new(128 * 1024 * 1024);
    let config = ReaderConfig {
        cpu_limiter: Some(cpu.clone()),
        work_budget: budget.clone(),
        ..ReaderConfig::default()
    };
    let reader = CogReader::builder()
        .reader_config(config)
        .open_object_store(store, "rgba.tif")
        .await
        .unwrap();
    let image = reader.read_window(0, 0, 0, 512, 512).await.unwrap();
    assert_eq!(image.pixels.bands(), 3);
    let pixels = image.pixels.to_f64();
    assert_eq!(
        [pixels[[0, 0, 1]], pixels[[1, 0, 1]], pixels[[2, 0, 1]]],
        [3., 7., 11.]
    );
    let mask = image.mask.as_ref().unwrap();
    assert!(!mask[[0, 0]]);
    assert!(mask[[0, 1]] && mask[[511, 511]]);
    // All four blocks are warm: an occupied CPU permit can only block the
    // alpha/stitch job, not decoding. Before the fix this window ran inline.
    let persistent = budget.used_bytes();
    let permit = cpu.acquire().await.unwrap();
    let requests = RequestLimiter::new(1);
    let result = requests
        .run(
            Duration::from_millis(30),
            &RequestOptions::default(),
            |_| reader.read_window(0, 0, 0, 512, 512),
        )
        .await;
    assert!(matches!(result, Err(AgError::DeadlineExceeded)));
    assert_eq!(requests.available_permits(), 1);
    assert_eq!(reader.reads.available_permits(), 8);
    assert_eq!(budget.used_bytes(), persistent);
    drop(permit);
    let again = reader.read_window(0, 0, 0, 512, 512).await.unwrap();
    assert_eq!(again.pixels.to_f64(), pixels);
    assert_eq!(again.mask, image.mask);
}

#[cfg(test)]
mod scale_offset_reader_tests {
    #[tokio::test]
    async fn band_scale_offset_defaults_to_identity() {
        use crate::io::{CogReader, ObjectReader};
        use object_store::local::LocalFileSystem;
        use std::sync::Arc;
        let fs = Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        let r = CogReader::builder()
            .snapshot_policy(crate::io::SnapshotPolicy::AssumeImmutable)
            .open(ObjectReader::new(
                fs,
                object_store::path::Path::from("tests/fixtures/rgb_nodata.tif"),
            ))
            .await
            .unwrap();
        // RGB fixture without SCALE/OFFSET metadata → identity per band.
        let so = r.band_scale_offset();
        assert!(so.iter().all(|s| s.is_identity()));
        assert_eq!(so.len(), 3);
    }
}

#[cfg(test)]
mod open_object_store_tests {
    use crate::io::{CogReader, ObjectReader};
    use object_store::local::LocalFileSystem;
    use std::sync::Arc;

    #[tokio::test]
    async fn open_object_store_matches_open_with_reader() {
        let fs = Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        let key = "tests/fixtures/rgb_nodata.tif";

        // New convenience path.
        let via_helper = CogReader::open_object_store(fs.clone(), key)
            .await
            .expect("open_object_store");

        // Old explicit path (must still produce an equivalent reader).
        let reader = ObjectReader::new(fs, object_store::path::Path::from(key));
        let via_reader = CogReader::builder()
            .snapshot_policy(crate::io::SnapshotPolicy::AssumeImmutable)
            .open(reader)
            .await
            .expect("open_with_reader");

        // GeoTransform does not impl PartialEq (wraps raffine::Affine);
        // compare wgs84_bounds() which returns [f64; 4] and does.
        assert_eq!(
            via_helper.wgs84_bounds().unwrap(),
            via_reader.wgs84_bounds().unwrap()
        );
        assert_eq!(via_helper.src_crs(), via_reader.src_crs());
    }
}

#[cfg(test)]
mod block_cache_tests {
    use super::*;

    async fn open_local_for_test(
        tif: &std::path::Path,
        source_id: &str,
        cache: std::sync::Arc<crate::io::block_cache::BlockCache>,
    ) -> CogReader {
        use async_tiff::reader::ObjectReader;
        use object_store::local::LocalFileSystem;
        use object_store::path::Path as OsPath;
        let os_path = OsPath::from_filesystem_path(tif).unwrap();
        let reader = ObjectReader::new(std::sync::Arc::new(LocalFileSystem::new()), os_path);
        CogReader::builder()
            .source_id(source_id)
            .block_cache(cache)
            .snapshot_policy(crate::io::SnapshotPolicy::AssumeImmutable)
            .open(reader)
            .await
            .unwrap()
    }

    #[test]
    fn ring_coords_clamps_to_grid_and_excludes_inner_rect() {
        use crate::io::window::IfdInfo;
        use async_tiff::tags::PlanarConfiguration;
        // 4×4 block grid (1024/256), window = block (0,0), ring = 1.
        let info = IfdInfo::from_parts(256, 256, 3, PlanarConfiguration::Planar, 1024, 1024)
            .validate()
            .unwrap();
        let mut got = super::ring_coords(&info, (0, 0), (0, 0), 1);
        got.sort_unstable();
        // Corner window: only the 3 in-grid neighbours; (−1, *) clamped away.
        assert_eq!(got, vec![(0, 1), (1, 0), (1, 1)]);

        // Interior window gets the full 8-neighbour ring.
        let mut got = super::ring_coords(&info, (1, 1), (1, 1), 1);
        got.sort_unstable();
        assert_eq!(got.len(), 8);
        assert!(!got.contains(&(1, 1)), "inner rect must be excluded");

        // Window already covering the whole grid → nothing to prefetch.
        assert!(super::ring_coords(&info, (0, 3), (0, 3), 1).is_empty());
    }

    #[tokio::test]
    async fn prefetch_ring_warms_neighbor_blocks() {
        use crate::io::block_cache::BlockCache;
        use crate::io::cache::CacheConfig;
        use crate::io::window::IfdInfo;
        use async_tiff::reader::ObjectReader;
        use object_store::local::LocalFileSystem;
        use object_store::path::Path as OsPath;

        let tif = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/rgb_nodata.tif");
        let cache = std::sync::Arc::new(BlockCache::new(CacheConfig::default()));
        let os_path = OsPath::from_filesystem_path(&tif).unwrap();
        let reader = ObjectReader::new(std::sync::Arc::new(LocalFileSystem::new()), os_path);
        let cog = CogReader::builder()
            .source_id("prefetch-test")
            .block_cache(std::sync::Arc::clone(&cache))
            .prefetch_ring(1)
            .snapshot_policy(crate::io::SnapshotPolicy::AssumeImmutable)
            .open(reader)
            .await
            .unwrap();

        // Need at least a 2-block grid at level 0 for a ring to exist.
        let info = IfdInfo::from(cog.data_ifd(0).unwrap()).validate().unwrap();
        let ring = super::ring_coords(&info, (0, 0), (0, 0), 1);
        assert!(!ring.is_empty(), "fixture must have neighboring blocks");

        // Read a window inside block (0, 0) only.
        cog.read_window(0, 0, 0, 16, 16).await.unwrap();

        // The detached prefetch task should eventually decode the ring blocks
        // (data kind at minimum). Poll with a deadline; no decode budget
        // contention exists in this test so it resolves in milliseconds.
        let want = 1 + ring.len() as u64; // foreground block + its ring
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let stats = cache.stats();
            if stats.decoded >= want {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "prefetch did not warm ring blocks in time: decoded={} want>={want}",
                stats.decoded
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn read_window_reuses_blocks_on_identical_window() {
        use crate::io::block_cache::BlockCache;
        use crate::io::cache::CacheConfig;
        let tif = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/rgb_nodata.tif");
        let cache = std::sync::Arc::new(BlockCache::new(CacheConfig::default()));
        let r = open_local_for_test(&tif, "src-A", cache.clone()).await;
        let a = r.read_window(0, 0, 0, 64, 64).await.unwrap();
        let n1 = cache.sync_count().await;
        let b = r.read_window(0, 0, 0, 64, 64).await.unwrap();
        let n2 = cache.sync_count().await;
        assert_eq!(
            a.pixels.to_f64(),
            b.pixels.to_f64(),
            "identical window → identical pixels"
        );
        assert!(n1 > 0, "blocks must be cached");
        assert_eq!(n1, n2, "identical window must not add blocks");
    }

    #[tokio::test]
    async fn read_window_rejects_out_of_bounds() {
        use crate::io::block_cache::BlockCache;
        use crate::io::cache::CacheConfig;
        let tif = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/rgb_nodata.tif");
        let cache = std::sync::Arc::new(BlockCache::new(CacheConfig::default()));
        let r = open_local_for_test(&tif, "src-A", cache.clone()).await;
        let (w, h) = r.level_dimensions(0).unwrap();
        // A window far larger than the level must be rejected before it can
        // drive a giant allocation — not clamped, not read.
        assert!(
            r.read_window(0, 0, 0, u64::from(w) + 100_000, u64::from(h) + 100_000)
                .await
                .is_err(),
            "oversized window must be rejected"
        );
        // Exactly-in-bounds still works.
        assert!(r
            .read_window(0, 0, 0, u64::from(w), u64::from(h))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn distinct_sources_add_separate_blocks() {
        use crate::io::block_cache::BlockCache;
        use crate::io::cache::CacheConfig;
        let tif = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/rgb_nodata.tif");
        let cache = std::sync::Arc::new(BlockCache::new(CacheConfig::default()));
        let a = open_local_for_test(&tif, "src-A", cache.clone()).await;
        let b = open_local_for_test(&tif, "src-B", cache.clone()).await;
        let _ = a.read_window(0, 0, 0, 32, 32).await.unwrap();
        let na = cache.sync_count().await;
        let _ = b.read_window(0, 0, 0, 32, 32).await.unwrap();
        let nb = cache.sync_count().await;
        assert!(
            nb > na,
            "different source ids must add separate block entries"
        );
    }

    #[tokio::test]
    async fn concurrent_identical_cold_reads_single_flight_the_fetch() {
        use crate::io::block_cache::BlockCache;
        use crate::io::cache::CacheConfig;
        let tif = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/rgb_nodata.tif");
        let cache = std::sync::Arc::new(BlockCache::new(CacheConfig::default()));
        let r = std::sync::Arc::new(open_local_for_test(&tif, "src-sf", cache.clone()).await);

        // Two requests race the SAME cold window. `claim` is synchronous, so the
        // first to enter the miss path owns every block's fetch; the second sees
        // the in-flight entries and awaits them instead of re-fetching.
        let r1 = std::sync::Arc::clone(&r);
        let r2 = std::sync::Arc::clone(&r);
        let (a, b) = tokio::join!(
            async move { r1.read_window(0, 0, 0, 64, 64).await },
            async move { r2.read_window(0, 0, 0, 64, 64).await },
        );
        let a = a.unwrap();
        let b = b.unwrap();
        assert_eq!(
            a.pixels.to_f64(),
            b.pixels.to_f64(),
            "both racing readers must observe identical pixels"
        );

        let blocks = cache.sync_count().await;
        let fetched = cache.stats().fetched;
        assert!(blocks > 0, "cold reads must populate the cache");
        // Single-flight: each cold block is fetched exactly once across the two
        // concurrent requests. Without dedup the loser would re-fetch every
        // block, driving `fetched` up to 2× the number of cached blocks.
        assert_eq!(
            fetched, blocks,
            "each cold block must be fetched exactly once despite the race"
        );
    }

    #[tokio::test]
    async fn level_gsds_are_cached_from_the_existing_formula() {
        use crate::io::block_cache::BlockCache;
        use crate::io::cache::CacheConfig;

        let tif = std::env::current_dir()
            .unwrap()
            .join("tests/fixtures/rgb_nodata.tif");
        let cache = std::sync::Arc::new(BlockCache::new(CacheConfig::default()));
        let reader = open_local_for_test(&tif, "level-gsd-cache", cache).await;

        assert!(reader.level_count() > 1, "fixture must contain overviews");
        assert_eq!(reader.level_gsds.len(), reader.level_count());
        for level in 0..reader.level_count() {
            // This is the formula `level_gsd` used before the eager cache.
            let tr = reader.level_transform(level).unwrap().0;
            let x_res = (tr.a.powi(2) + tr.d.powi(2)).sqrt();
            let y_res = (tr.b.powi(2) + tr.e.powi(2)).sqrt();
            let expected = x_res.max(y_res);

            assert_eq!(reader.level_gsds[level], expected);
            assert_eq!(reader.level_gsd(level).unwrap(), expected);
        }
        assert!(matches!(
            reader.level_gsd(reader.level_count()),
            Err(AgError::MissingGeo("overview level"))
        ));
    }

    #[test]
    fn overview_brackets_target_from_below_downsample() {
        // target 30 sits between L1(20) and L2(40): rio-tiler picks the
        // FINER bracket (L1) and downsamples 20→30. Not L2 (upsample).
        assert_eq!(pick_overview_from_gsds(&TEST_OVERVIEW_PYRAMID, 30.0), 1);
    }

    #[test]
    fn overview_zoomed_in_uses_full_res() {
        // target finer than native → full res.
        assert_eq!(pick_overview_from_gsds(&TEST_OVERVIEW_PYRAMID, 5.0), 0);
        assert_eq!(pick_overview_from_gsds(&TEST_OVERVIEW_PYRAMID, 10.0), 0);
    }

    #[test]
    fn overview_snaps_within_tolerance() {
        // target ≈ L2(40) within 0.1 → snap to L2.
        assert_eq!(pick_overview_from_gsds(&TEST_OVERVIEW_PYRAMID, 40.05), 2);
    }

    #[test]
    fn overview_coarser_than_all_uses_coarsest() {
        assert_eq!(pick_overview_from_gsds(&TEST_OVERVIEW_PYRAMID, 200.0), 3);
    }

    #[test]
    fn overview_single_level_is_zero() {
        assert_eq!(pick_overview_from_gsds(&[10.0], 100.0), 0);
    }

    #[test]
    fn overview_selection_is_independent_of_coordinate_units() {
        assert_eq!(
            pick_overview_from_gsds(&[0.0001, 0.0002, 0.0004, 0.0008], 0.0005),
            2
        );
        assert_eq!(pick_overview_from_gsds(&[1.0, 2.0, 4.0, 8.0], 5.0), 2);
    }

    #[test]
    fn reader_config_clamps_decode_limit_to_one() {
        let cfg = ReaderConfig {
            max_decode_tasks: 0,
            ..ReaderConfig::default()
        };
        assert_eq!(cfg.decode_task_limit(), 1);
    }

    #[test]
    fn split_http_url_extracts_parts() {
        let (base, path) = split_http_url("https://example.com/foo/bar.tif").unwrap();
        assert_eq!(base, "https://example.com");
        assert_eq!(path, "foo/bar.tif");
    }

    #[test]
    fn split_http_url_keeps_port() {
        let (base, _path) = split_http_url("http://localhost:8080/data/scene.tif").unwrap();
        assert_eq!(base, "http://localhost:8080");
    }

    #[test]
    fn non_ascii_path_round_trips_via_from_url_path() {
        use object_store::path::Path;

        // Mirror server::dataset_url: the `url` crate percent-encodes a
        // non-ASCII filename into the path segment.
        let mut u = url::Url::parse("http://127.0.0.1:8080").unwrap();
        u.path_segments_mut()
            .unwrap()
            .push("data")
            .push("测试-中文.tif");
        let (_base, path) = split_http_url(u.as_str()).unwrap();
        assert!(path.contains('%'), "split keeps the URL-encoded path");

        // The trap: `Path::from` treats input as a DECODED logical path,
        // so it re-encodes the `%`, double-encoding the filename.
        assert_ne!(Path::from(path.as_str()).as_ref(), "data/测试-中文.tif");

        // The fix: `Path::from_url_path` decodes the URL-encoded path, so
        // object_store re-encodes it exactly once for the loopback GET and
        // the server's `/data/{name}` handler sees the original filename.
        assert_eq!(
            Path::from_url_path(&path).unwrap().as_ref(),
            "data/测试-中文.tif"
        );
    }
}

#[cfg(test)]
mod audit_metadata_tests {
    use super::*;
    use async_tiff::{reader::Endianness, tags::Tag, TagValue};

    fn ifd(
        w: u32,
        h: u32,
        reduced: bool,
        bands: u16,
        point: bool,
        matrix: bool,
    ) -> ImageFileDirectory {
        let mut tags = HashMap::from([
            (Tag::ImageWidth, TagValue::Unsigned(w)),
            (Tag::ImageLength, TagValue::Unsigned(h)),
            (Tag::SamplesPerPixel, TagValue::Short(bands)),
            (
                Tag::BitsPerSample,
                TagValue::List(vec![TagValue::Short(8); bands as usize]),
            ),
            (Tag::Compression, TagValue::Short(1)),
            (Tag::PhotometricInterpretation, TagValue::Short(1)),
            (Tag::NewSubfileType, TagValue::Unsigned(u32::from(reduced))),
            (Tag::TileWidth, TagValue::Unsigned(16)),
            (Tag::TileLength, TagValue::Unsigned(16)),
            (
                Tag::GeoKeyDirectory,
                TagValue::List(
                    [1, 1, 0, 1, 1025, 0, 1, if point { 2 } else { 1 }]
                        .into_iter()
                        .map(TagValue::Short)
                        .collect(),
                ),
            ),
        ]);
        let doubles = |v: &[f64]| TagValue::List(v.iter().copied().map(TagValue::Double).collect());
        if matrix {
            tags.insert(
                Tag::ModelTransformation,
                doubles(&[
                    2., 0.5, 0., 100., 0.25, -3., 0., 200., 0., 0., 1., 0., 0., 0., 0., 1.,
                ]),
            );
        } else {
            tags.insert(Tag::ModelTiepoint, doubles(&[0., 0., 0., 100., 200., 0.]));
            tags.insert(
                Tag::ModelPixelScale,
                doubles(&[128. / f64::from(w), 192. / f64::from(h), 0.]),
            );
        }
        ImageFileDirectory::from_tags(tags, Endianness::LittleEndian).unwrap()
    }

    #[test]
    fn audit_pixel_is_point_normalizes_both_georeferencing_forms() {
        for matrix in [false, true] {
            let area = TIFF::new(
                vec![ifd(64, 64, false, 1, false, matrix)],
                Endianness::LittleEndian,
            );
            let point = TIFF::new(
                vec![ifd(64, 64, false, 1, true, matrix)],
                Endianness::LittleEndian,
            );
            let area_gt = derive_geo(&area).unwrap().0;
            let point_gt = derive_geo(&point).unwrap().0;
            assert_eq!(point_gt.forward(0.5, 0.5), area_gt.forward(0., 0.));
        }
    }

    #[test]
    fn audit_unrelated_pages_are_not_overviews() {
        let tiff = TIFF::new(
            vec![
                ifd(64, 64, false, 1, false, false),
                ifd(64, 64, false, 1, false, false),
                ifd(128, 128, true, 1, false, false),
                ifd(32, 32, true, 3, false, false),
                ifd(32, 16, true, 1, false, false),
                ifd(32, 32, false, 1, false, false),
                ifd(32, 32, true, 1, false, false),
            ],
            Endianness::LittleEndian,
        );
        assert_eq!(classify_ifds(&tiff).unwrap().0, vec![0, 6]);
    }
}

#[cfg(test)]
mod audit_http_tests {
    use super::*;
    #[tokio::test]
    async fn audit_signed_url_reaches_range_server_unchanged() {
        use axum::{
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };
        let fixture = include_bytes!("../../tests/fixtures/rgb_nodata.tif");
        let app = Router::new().route(
            "/signed.tif",
            get(move |uri: axum::http::Uri, headers: HeaderMap| async move {
                assert_eq!(uri.query(), Some("signature=a%2Fb&versionId=v1"));
                let range = headers
                    .get("range")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .strip_prefix("bytes=")
                    .unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let start: usize = start.parse().unwrap();
                let end: usize = end.parse::<usize>().unwrap().min(fixture.len() - 1);
                (
                    StatusCode::PARTIAL_CONTENT,
                    [
                        ("etag", "\"fixture-v1\"".to_string()),
                        (
                            "content-range",
                            format!("bytes {start}-{end}/{}", fixture.len()),
                        ),
                    ],
                    fixture[start..=end].to_vec(),
                )
            }),
        );
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
        let result = CogReader::builder()
            .open_http(&format!(
                "http://{addr}/signed.tif?signature=a%2Fb&versionId=v1"
            ))
            .await;
        server.abort();
        assert!(result.is_ok(), "signed URL open failed: {:?}", result.err());
    }
}

#[cfg(test)]
mod audit_limit_tests {
    use super::*;
    async fn configured(config: ReaderConfig) -> AgResult<CogReader> {
        let store = Arc::new(object_store::local::LocalFileSystem::new_with_prefix(".").unwrap());
        CogReader::builder()
            .reader_config(config)
            .open_object_store(store, "tests/fixtures/rgb_nodata.tif")
            .await
    }
    #[tokio::test]
    async fn audit_compressed_ranges_are_bounded_before_fetch() {
        let mut bytes = include_bytes!("../../tests/fixtures/rgb_nodata.tif").to_vec();
        let ifd = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let entries = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
        let entry = (0..entries)
            .map(|i| ifd + 2 + i * 12)
            .find(|&i| u16::from_le_bytes(bytes[i..i + 2].try_into().unwrap()) == 325)
            .unwrap();
        let counts = u32::from_le_bytes(bytes[entry + 8..entry + 12].try_into().unwrap()) as usize;
        bytes[counts..counts + 4].copy_from_slice(&9000u32.to_le_bytes());
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), bytes).unwrap();
        let cfg = ReaderConfig {
            max_window_bytes: 8000,
            ..ReaderConfig::default()
        };
        let cache = cfg.block_cache.clone();
        let source = ObjectReader::new(
            Arc::new(object_store::local::LocalFileSystem::new()),
            Path::from_filesystem_path(file.path()).unwrap(),
        );
        let reader = CogReader::builder()
            .reader_config(cfg)
            .snapshot_policy(crate::io::SnapshotPolicy::AssumeImmutable)
            .open(source)
            .await
            .unwrap();
        let result = reader.read_window(0, 0, 0, 1, 1).await;
        assert_eq!(
            cache.stats().fetched,
            0,
            "oversized declared compressed range reached the store"
        );
        assert!(result.is_err());

        let cfg = ReaderConfig {
            max_window_bytes: 5000,
            ..ReaderConfig::default()
        };
        let cache = cfg.block_cache.clone();
        let reader = configured(cfg).await.unwrap();
        let result = reader.read_window(0, 31, 0, 2, 1).await;
        assert_eq!(
            cache.stats().fetched,
            0,
            "batch sum exceeded compressed byte limit"
        );
        assert!(result.is_err());
    }
    #[tokio::test]
    async fn audit_window_limits_reject_before_fetch() {
        for kind in 0..3 {
            let mut cfg = ReaderConfig::default();
            match kind {
                0 => cfg.max_window_pixels = 100,
                1 => cfg.max_window_bytes = 100,
                _ => cfg.max_window_blocks = 1,
            };
            let cache = cfg.block_cache.clone();
            let reader = configured(cfg).await.unwrap();
            assert!(reader.read_window(0, 0, 0, 64, 64).await.is_err());
            assert_eq!(cache.stats().fetched, 0);
        }
    }
    #[tokio::test]
    async fn audit_declared_decoded_block_limit_rejects_on_open() {
        let cfg = ReaderConfig {
            max_decoded_block_bytes: 100,
            ..ReaderConfig::default()
        };
        assert!(configured(cfg).await.is_err());
    }
    #[tokio::test]
    async fn audit_prefetch_does_not_queue_behind_foreground() {
        let cfg = ReaderConfig {
            max_concurrent_reads: 1,
            prefetch_ring: 1,
            ..ReaderConfig::default()
        };
        let cache = cfg.block_cache.clone();
        let reader = configured(cfg).await.unwrap();
        reader.read_window(0, 0, 0, 16, 16).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            cache.stats().fetched,
            1,
            "speculation must skip when foreground owns the budget"
        );
    }
    #[tokio::test]
    async fn audit_shared_read_limiter_applies_backpressure() {
        let limiter = Arc::new(tokio::sync::Semaphore::new(0));
        let cfg = ReaderConfig {
            read_limiter: Some(limiter.clone()),
            ..ReaderConfig::default()
        };
        let reader = configured(cfg).await.unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            reader.read_window(0, 0, 0, 16, 16)
        )
        .await
        .is_err());
        limiter.add_permits(1);
        assert!(reader.read_window(0, 0, 0, 16, 16).await.is_ok());
    }
}

#[cfg(test)]
mod audit_http_error_tests {
    use super::*;
    #[derive(Clone)]
    struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
        type Writer = Self;
        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }
    #[tokio::test]
    async fn audit_http_range_errors_and_prefetch_logs_hide_signed_query() {
        use axum::{
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };
        use std::sync::atomic::{AtomicBool, Ordering};
        let failing = Arc::new(AtomicBool::new(false));
        let state = failing.clone();
        let fixture = include_bytes!("../../tests/fixtures/rgb_nodata.tif");
        let app = Router::new().route(
            "/signed.tif",
            get(move |headers: HeaderMap| {
                let state = state.clone();
                async move {
                    if state.load(Ordering::SeqCst) {
                        return (StatusCode::FORBIDDEN, HeaderMap::new(), Vec::new());
                    }
                    let range = headers["range"]
                        .to_str()
                        .unwrap()
                        .strip_prefix("bytes=")
                        .unwrap();
                    let (start, end) = range.split_once('-').unwrap();
                    let start: usize = start.parse().unwrap();
                    let end = end.parse::<usize>().unwrap().min(fixture.len() - 1);
                    let mut out = HeaderMap::new();
                    out.insert("etag", "\"fixture-v1\"".parse().unwrap());
                    out.insert(
                        "content-range",
                        format!("bytes {start}-{end}/{}", fixture.len())
                            .parse()
                            .unwrap(),
                    );
                    (
                        StatusCode::PARTIAL_CONTENT,
                        out,
                        fixture[start..=end].to_vec(),
                    )
                }
            }),
        );
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
        let url = format!("http://{addr}/signed.tif?signature=SECRET_RANGE_TOKEN");
        let direct = Arc::new(CogReader::builder().open_http(&url).await.unwrap());
        #[allow(unused_mut)]
        let mut readers = vec![direct];
        failing.store(true, Ordering::SeqCst);
        let logs = LogBuffer(Arc::new(std::sync::Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::DEBUG)
            .without_time()
            .with_ansi(false)
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);
        let mut errors = Vec::new();
        errors.push(
            CogReader::builder()
                .open_http(&url)
                .await
                .err()
                .expect("HTTP open should now fail")
                .to_string(),
        );
        for reader in readers {
            errors.push(
                reader
                    .read_window(0, 0, 0, 16, 16)
                    .await
                    .unwrap_err()
                    .to_string(),
            );
            prefetch_blocks(
                reader.data_ifd(0).unwrap(),
                BlockKind::Data,
                0,
                &[(1, 1)],
                &reader.source_id,
                &reader.block_cache,
                &reader.cpu,
                &reader.reader,
                reader.redact_transport_errors,
                reader.max_window_bytes,
                &reader.work_budget,
                &reader.metadata_reservations,
            )
            .await;
        }
        server.abort();
        let log_text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(
            log_text.contains("block prefetch fetch failed"),
            "prefetch failure was not captured"
        );
        assert!(
            !errors.join("\n").contains("SECRET_RANGE_TOKEN"),
            "range error leaked query: {errors:?}"
        );
        assert!(
            !log_text.contains("SECRET_RANGE_TOKEN"),
            "tracing leaked signed query: {log_text}"
        );
    }
}

#[cfg(test)]
mod snapshot_regression_tests {
    use super::*;
    use object_store::ObjectStoreExt;
    #[tokio::test]
    async fn snapshot_open_preserves_resource_limit_classification() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let path = Path::from("x.tif");
        store
            .put(
                &path,
                include_bytes!("../../tests/fixtures/rgb_nodata.tif")
                    .as_slice()
                    .into(),
            )
            .await
            .unwrap();
        let config = ReaderConfig {
            work_budget: crate::resource::WorkBudget::new(0),
            ..ReaderConfig::default()
        };
        assert!(matches!(
            CogReader::builder()
                .reader_config(config)
                .open_object_store(store, path.as_ref())
                .await,
            Err(AgError::ResourceLimit(_))
        ));
    }
    #[tokio::test]
    async fn snapshot_rejects_object_mutation_between_metadata_and_pixels() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let path = Path::from("mutable.tif");
        let bytes = include_bytes!("../../tests/fixtures/rgb_nodata.tif");
        store.put(&path, bytes.as_slice().into()).await.unwrap();
        let reader = CogReader::builder()
            .open_object_store(store.clone(), path.as_ref())
            .await
            .unwrap();
        let mut changed = bytes.to_vec();
        let last = changed.len() - 1;
        changed[last] ^= 1;
        store.put(&path, changed.into()).await.unwrap();
        assert!(
            reader.read_window(0, 0, 0, 16, 16).await.is_err(),
            "must not combine old metadata with a new object version"
        );
    }
}
#[tokio::test]
async fn working_budget_rejects_read_before_fetch_and_releases_on_return() {
    let budget = crate::resource::WorkBudget::new(256 * 1024);
    let cfg = ReaderConfig {
        work_budget: budget.clone(),
        ..ReaderConfig::default()
    };
    let cache = cfg.block_cache.clone();
    let store = Arc::new(object_store::local::LocalFileSystem::new_with_prefix(".").unwrap());
    let reader = CogReader::builder()
        .reader_config(cfg)
        .open_object_store(store, "tests/fixtures/rgb_nodata.tif")
        .await
        .unwrap();
    let persistent = budget.used_bytes();
    let busy = budget
        .try_reserve(budget.limit_bytes() - persistent)
        .unwrap();
    assert!(reader.read_window(0, 0, 0, 16, 16).await.is_err());
    assert_eq!(cache.stats().fetched, 0);
    drop(busy);
    assert!(reader.read_window(0, 0, 0, 16, 16).await.is_ok());
    assert_eq!(budget.used_bytes(), persistent);
    drop(reader);
    assert_eq!(budget.used_bytes(), 0);
    assert!(budget.peak_bytes() <= budget.limit_bytes());
}

#[cfg(test)]
mod prefetch_shutdown_tests {
    use super::*;
    #[tokio::test]
    async fn dropping_reader_cancels_inflight_prefetch() {
        use axum::{
            http::{HeaderMap, StatusCode},
            routing::get,
            Router,
        };
        use std::sync::atomic::{AtomicBool, Ordering};
        let pixels_started = Arc::new(AtomicBool::new(false));
        let flag = pixels_started.clone();
        let (notify, mut started) = tokio::sync::mpsc::unbounded_channel();
        let fixture = include_bytes!("../../tests/fixtures/rgb_nodata.tif");
        let app = Router::new().route(
            "/image",
            get(move |headers: HeaderMap| {
                let flag = flag.clone();
                let notify = notify.clone();
                async move {
                    let range = headers["range"]
                        .to_str()
                        .unwrap()
                        .strip_prefix("bytes=")
                        .unwrap();
                    let (start, end) = range.split_once('-').unwrap();
                    let start: usize = start.parse().unwrap();
                    let end = end.parse::<usize>().unwrap().min(fixture.len() - 1);
                    if flag.load(Ordering::SeqCst) {
                        let _ = notify.send(());
                        std::future::pending::<()>().await;
                    }
                    let mut out = HeaderMap::new();
                    out.insert("etag", "\"fixed\"".parse().unwrap());
                    out.insert(
                        "content-range",
                        format!("bytes {start}-{end}/{}", fixture.len())
                            .parse()
                            .unwrap(),
                    );
                    (
                        StatusCode::PARTIAL_CONTENT,
                        out,
                        fixture[start..=end].to_vec(),
                    )
                }
            }),
        );
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
        let reader = CogReader::builder()
            .open_http(&format!("http://{addr}/image"))
            .await
            .unwrap();
        let reads = reader.reads.clone();
        pixels_started.store(true, Ordering::SeqCst);
        // Start exactly the reader-owned speculative operation; no foreground
        // request owns the reader or its read permit when it is dropped below.
        let mut reader = reader;
        reader.prefetch_ring = 1;
        reader.maybe_spawn_prefetch(0, (0, 0), (0, 0));
        tokio::time::timeout(Duration::from_secs(1), started.recv())
            .await
            .unwrap()
            .unwrap();
        drop(reader);
        let stopped = tokio::time::timeout(Duration::from_millis(200), async {
            while reads.available_permits() != 8 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        server.abort();
        assert!(
            stopped.is_ok(),
            "reader drop must abort its speculative fetch and release its permit"
        );
    }
}

#[cfg(test)]
mod metadata_limit_tests {
    use super::*;
    #[tokio::test]
    async fn malformed_directory_cycles_and_huge_counts_are_rejected() {
        use object_store::ObjectStoreExt;
        let original = include_bytes!("../../tests/fixtures/rgb_nodata.tif").to_vec();
        let offset = u32::from_le_bytes(original[4..8].try_into().unwrap()) as usize;
        let count = u16::from_le_bytes(original[offset..offset + 2].try_into().unwrap()) as usize;
        let mut cyclic = original.clone();
        let next = offset + 2 + count * 12;
        cyclic[next..next + 4].copy_from_slice(&(offset as u32).to_le_bytes());
        let mut values = original;
        values[offset + 2 + 4..offset + 2 + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut big = b"II".to_vec();
        big.extend(43u16.to_le_bytes());
        big.extend(8u16.to_le_bytes());
        big.extend(0u16.to_le_bytes());
        big.extend(16u64.to_le_bytes());
        big.extend(u64::MAX.to_le_bytes());
        for bytes in [cyclic, values, big] {
            let store = Arc::new(object_store::memory::InMemory::new());
            let path = Path::from("invalid.tif");
            store.put(&path, bytes.into()).await.unwrap();
            assert!(CogReader::builder()
                .open_object_store(store, path.as_ref())
                .await
                .is_err());
        }
    }
    #[tokio::test]
    async fn parser_limits_apply_before_ifd_materialization() {
        for entries in [false, true] {
            let mut config = ReaderConfig::default();
            if entries {
                config.max_ifd_entries = 1;
            } else {
                config.max_ifds = 1;
            }
            let store =
                Arc::new(object_store::local::LocalFileSystem::new_with_prefix(".").unwrap());
            assert!(CogReader::builder()
                .reader_config(config)
                .open_object_store(store, "tests/fixtures/rgb_nodata.tif")
                .await
                .is_err());
        }
    }
}
