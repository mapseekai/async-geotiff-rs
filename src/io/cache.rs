use std::sync::Arc;
use std::time::Duration;

use crate::pixel::{ImageData, PixelData};

/// Cache key for a decoded source-pixel window. One `CogReader` serves a
/// single COG, so the URL is not part of the key.
#[derive(Clone, Hash, PartialEq, Eq, Debug)]
pub(crate) struct WindowKey {
    pub(crate) level: usize,
    pub(crate) col: i64,
    pub(crate) row: i64,
    pub(crate) w: u64,
    pub(crate) h: u64,
}

pub(crate) type WindowCache = moka::future::Cache<WindowKey, Arc<ImageData>>;

/// Tuning for the decoded-window cache. `max_bytes` bounds total resident
/// pixel memory (moka evicts via TinyLFU). `ttl` is opt-in; COG data is
/// immutable so the default relies on process lifetime + capacity eviction.
#[derive(Clone, Copy, Debug)]
pub struct CacheConfig {
    pub max_bytes: u64,
    pub ttl: Option<Duration>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: 256 << 20,
            ttl: None,
        }
    }
}

/// Approximate resident byte size of an `ImageData`: pixel buffer
/// (element width × bands × h × w) plus the optional validity mask
/// (1 byte per pixel). Saturates into the `u32` moka weigher expects.
fn image_data_weight(d: &ImageData) -> u32 {
    let elem = match d.pixels {
        PixelData::U8(_) => 1,
        PixelData::U16(_) | PixelData::I16(_) => 2,
        PixelData::U32(_) | PixelData::I32(_) | PixelData::F32(_) => 4,
        PixelData::F64(_) => 8,
    };
    let px = elem * d.pixels.bands() * d.pixels.height() * d.pixels.width();
    let mask = d.mask.as_ref().map_or(0, |m| m.len());
    u32::try_from(px + mask).unwrap_or(u32::MAX)
}

pub(crate) fn build_window_cache(cfg: CacheConfig) -> WindowCache {
    let mut builder = moka::future::Cache::builder()
        .max_capacity(cfg.max_bytes)
        .weigher(|_k: &WindowKey, v: &Arc<ImageData>| image_data_weight(v));
    if let Some(ttl) = cfg.ttl {
        builder = builder.time_to_live(ttl);
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bounds::{Bbox, GeoTransform};
    use crate::crs::Crs;
    use crate::error::AgError;
    use ndarray::{Array2, Array3};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn dummy_image() -> ImageData {
        ImageData {
            pixels: PixelData::U8(Array3::zeros((3, 8, 8))),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            crs: Crs::new("UNKNOWN"),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 0.0, 0.0, -1.0]),
            nodata: None,
        }
    }

    #[test]
    fn weight_counts_pixel_bytes() {
        let mk = |pixels: PixelData, mask: Option<Array2<bool>>| ImageData {
            pixels,
            mask,
            bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            crs: Crs::new("UNKNOWN"),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 0.0, 0.0, -1.0]),
            nodata: None,
        };

        let d = mk(PixelData::U8(Array3::zeros((1, 4, 4))), None);
        assert_eq!(image_data_weight(&d), 16);

        let d = mk(PixelData::U16(Array3::zeros((3, 2, 2))), None);
        assert_eq!(image_data_weight(&d), 24);

        let d = mk(
            PixelData::U8(Array3::zeros((1, 4, 4))),
            Some(Array2::default((4, 4))),
        );
        assert_eq!(image_data_weight(&d), 32);
    }

    #[tokio::test]
    async fn cache_returns_same_arc_on_hit() {
        let cache = build_window_cache(CacheConfig::default());
        let key = WindowKey {
            level: 0,
            col: 0,
            row: 0,
            w: 8,
            h: 8,
        };
        let calls = AtomicUsize::new(0);
        let init = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, AgError>(Arc::new(dummy_image()))
        };
        let a = cache.try_get_with(key.clone(), init()).await.unwrap();
        let b = cache.try_get_with(key, init()).await.unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cache_single_flights_concurrent_identical_reads() {
        let cache = build_window_cache(CacheConfig::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let key = WindowKey {
            level: 2,
            col: 5,
            row: 7,
            w: 256,
            h: 256,
        };
        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let key = key.clone();
            let calls = Arc::clone(&calls);
            handles.push(tokio::spawn(async move {
                cache
                    .try_get_with(key, async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        Ok::<_, AgError>(Arc::new(dummy_image()))
                    })
                    .await
                    .unwrap()
            }));
        }
        let mut arcs = Vec::new();
        for h in handles {
            arcs.push(h.await.unwrap());
        }
        for a in &arcs[1..] {
            assert!(Arc::ptr_eq(&arcs[0], a));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
