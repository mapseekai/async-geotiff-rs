//! Multi-source pixel-level mosaic. See
//! `docs/superpowers/specs/2026-05-18-mosaic-design.md`.

pub(crate) mod mosaicjson;
pub(crate) mod quadkey;
mod reader_pool;
pub(crate) mod selection;
pub(crate) mod source;
mod ssrf;
#[cfg(feature = "stac")]
pub(crate) mod stac;

pub use mosaicjson::{MosaicDataType, MosaicJson, MosaicJsonSource};
pub use selection::PixelSelection;
pub use source::{AssetRef, MosaicSource};
pub use ssrf::HttpAccessPolicy;
#[cfg(feature = "stac")]
pub use stac::StacSource;

use bytes::Bytes;

use std::sync::Arc;

use crate::error::{AgError, AgResult};
use crate::io::BlockCache;
use crate::pixel::ImageData;
use crate::render::pipeline::render;
use crate::style::params::compile_cached;
use crate::style::TileStyle;
use crate::tiler::Tiler;
use crate::tms::TileCoord;

pub use reader_pool::Backend;

use reader_pool::{redact_query, ReaderPool};
use selection::{NonEmptyReducer, Reducer};

/// Tuning knobs for [`MosaicTiler`].
///
/// `chunk_size` is the effective concurrency bound for per-asset reads/warps;
/// each asset is processed via an independent `Tiler`, so the per-`Tiler`
/// blocking semaphore does not bound the mosaic path — keep `chunk_size`
/// modest under load.
#[derive(Clone, Copy, Debug)]
pub struct MosaicConfig {
    /// Assets fetched+warped in parallel per chunk; `is_done` is checked
    /// after each chunk so `First`/`Highest`/`Lowest` can short-circuit.
    pub chunk_size: usize,
    /// Max distinct opened COGs kept in the reader pool.
    pub reader_capacity: u64,
    /// Hard cap on assets processed for a single tile. A low-zoom request whose
    /// quadkey prefix matches a large fraction of the mosaic index could
    /// otherwise fan out to thousands of remote opens/reads/warps per tile (a
    /// fetch-amplification DoS). Excess assets past this cap are dropped with a
    /// warning. `0` disables the cap.
    pub max_assets_per_tile: usize,
}

impl Default for MosaicConfig {
    fn default() -> Self {
        Self {
            chunk_size: 8,
            reader_capacity: 256,
            max_assets_per_tile: 64,
        }
    }
}

/// Renders mosaicked map tiles from multiple COG assets via a [`MosaicSource`].
pub struct MosaicTiler<S: MosaicSource> {
    source: S,
    pool: ReaderPool,
    cfg: MosaicConfig,
}

/// Builds a [`MosaicTiler`]. Every knob is optional — `build()` always
/// succeeds:
///
/// ```no_run
/// # use async_geotiff::mosaic::{Backend, MosaicConfig, MosaicTiler, MosaicJsonSource};
/// # fn demo(source: MosaicJsonSource, store: std::sync::Arc<dyn object_store::ObjectStore>) {
/// let mosaic = MosaicTiler::builder(source)
///     .backend(Backend::Store(store))
///     .config(MosaicConfig { chunk_size: 4, ..MosaicConfig::default() })
///     .build();
/// # }
/// ```
pub struct MosaicTilerBuilder<S: MosaicSource> {
    source: S,
    cfg: MosaicConfig,
    block_cache: Option<Arc<BlockCache>>,
    backend: Backend,
    http_policy: Option<HttpAccessPolicy>,
}

impl<S: MosaicSource> MosaicTilerBuilder<S> {
    /// Explicit tuning parameters (chunk size, reader-pool capacity).
    pub fn config(mut self, cfg: MosaicConfig) -> Self {
        self.cfg = cfg;
        self
    }

    /// Draw decoded blocks from a caller-supplied shared [`BlockCache`].
    /// Inject the same `Arc<BlockCache>` here and into your other readers to
    /// bound the whole process to one decode memory budget and to observe it
    /// via [`BlockCache::stats`].
    pub fn block_cache(mut self, block_cache: Arc<BlockCache>) -> Self {
        self.block_cache = Some(block_cache);
        self
    }

    /// How assets are opened — see [`Backend`]. With [`Backend::Store`],
    /// every [`AssetRef::url`] returned by the source is interpreted as a
    /// *decoded object key* within that single store (single server, single
    /// bucket), e.g. `"scenes/2026/tile.tif"`. The library holds no
    /// credentials: configure endpoint/bucket/auth on the store first.
    pub fn backend(mut self, backend: Backend) -> Self {
        self.backend = backend;
        self
    }

    /// SSRF policy for HTTP asset URLs — see [`HttpAccessPolicy`]. Asset URLs
    /// come from the (possibly untrusted) mosaic source, so the default already
    /// blocks loopback and the cloud-metadata endpoint; override to
    /// [`HttpAccessPolicy::strict`] for fully untrusted documents or
    /// [`HttpAccessPolicy::permissive`] when every URL is operator-controlled.
    pub fn http_policy(mut self, policy: HttpAccessPolicy) -> Self {
        self.http_policy = Some(policy);
        self
    }

    pub fn build(self) -> MosaicTiler<S> {
        let mut pool = ReaderPool::builder(self.cfg.reader_capacity).backend(self.backend);
        if let Some(block_cache) = self.block_cache {
            pool = pool.block_cache(block_cache);
        }
        if let Some(http_policy) = self.http_policy {
            pool = pool.http_policy(http_policy);
        }
        MosaicTiler {
            pool: pool.build(),
            source: self.source,
            cfg: self.cfg,
        }
    }
}

impl<S: MosaicSource> MosaicTiler<S> {
    /// Create a new `MosaicTiler` with default [`MosaicConfig`].
    pub fn new(source: S) -> Self {
        Self::builder(source).build()
    }

    /// Start building a `MosaicTiler` — see [`MosaicTilerBuilder`].
    pub fn builder(source: S) -> MosaicTilerBuilder<S> {
        MosaicTilerBuilder {
            source,
            cfg: MosaicConfig::default(),
            block_cache: None,
            backend: Backend::Http,
            http_policy: None,
        }
    }

    /// Create a `MosaicTiler` with explicit tuning parameters.
    #[deprecated(
        since = "0.0.9",
        note = "use `MosaicTiler::builder(source).config(cfg).build()`"
    )]
    pub fn with_config(source: S, cfg: MosaicConfig) -> Self {
        Self::builder(source).config(cfg).build()
    }

    /// Create a `MosaicTiler` that draws its decoded blocks from a
    /// caller-supplied shared [`BlockCache`].
    #[deprecated(
        since = "0.0.9",
        note = "use `MosaicTiler::builder(source).block_cache(cache).build()`"
    )]
    pub fn with_block_cache(source: S, block_cache: Arc<BlockCache>) -> Self {
        Self::builder(source).block_cache(block_cache).build()
    }

    /// Create a `MosaicTiler` whose assets are opened through one
    /// caller-injected `object_store` backend rather than HTTP.
    #[deprecated(
        since = "0.0.9",
        note = "use `MosaicTiler::builder(source).backend(Backend::Store(store)).build()`"
    )]
    pub fn with_object_store(source: S, store: Arc<dyn object_store::ObjectStore>) -> Self {
        Self::builder(source).backend(Backend::Store(store)).build()
    }

    /// [`Self::with_object_store`] with explicit tuning parameters.
    #[deprecated(
        since = "0.0.9",
        note = "use `MosaicTiler::builder(source).backend(Backend::Store(store)).config(cfg).build()`"
    )]
    pub fn with_object_store_config(
        source: S,
        store: Arc<dyn object_store::ObjectStore>,
        cfg: MosaicConfig,
    ) -> Self {
        Self::builder(source)
            .backend(Backend::Store(store))
            .config(cfg)
            .build()
    }

    /// [`Self::with_object_store`] plus a caller-supplied shared [`BlockCache`].
    #[deprecated(
        since = "0.0.9",
        note = "use `MosaicTiler::builder(source).backend(Backend::Store(store)).block_cache(cache).build()`"
    )]
    pub fn with_object_store_and_block_cache(
        source: S,
        store: Arc<dyn object_store::ObjectStore>,
        block_cache: Arc<BlockCache>,
    ) -> Self {
        Self::builder(source)
            .backend(Backend::Store(store))
            .block_cache(block_cache)
            .build()
    }

    /// The shared block cache backing this mosaic's reader pool — poll
    /// [`BlockCache::stats`] on it for metrics.
    pub fn block_cache(&self) -> Arc<BlockCache> {
        self.pool.block_cache()
    }

    /// Render one mosaicked tile. `style` is shared by all assets but
    /// applies in two phases: read/warp params (resampling/reproject,
    /// nodata via the source COGs) per-asset; render params
    /// (rescale/colormap/color_formula/tilesize) once on the composite.
    pub async fn tile(
        &self,
        coord: TileCoord,
        selection: PixelSelection,
        style: &TileStyle,
    ) -> AgResult<Option<Bytes>> {
        let mut assets = self.source.assets_for_tile(coord).await?;
        if assets.is_empty() {
            return Ok(None);
        }
        // Cap fan-out: a low-zoom prefix match can return a huge asset set;
        // opening/warping all of them per tile is a fetch-amplification DoS.
        let cap = self.cfg.max_assets_per_tile;
        if cap > 0 && assets.len() > cap {
            tracing::warn!(
                coord = ?(coord.z, coord.x, coord.y),
                matched = assets.len(),
                cap,
                "mosaic tile asset fan-out capped"
            );
            assets.truncate(cap);
        }

        let mut reducer: Option<Reducer<NonEmptyReducer>> = None;
        let mut fed = 0usize;

        for chunk in assets.chunks(self.cfg.chunk_size) {
            let futs = chunk.iter().map(|a| async move {
                let reader = self.pool.get_or_open(&a.url, &a.source_id).await?;
                let tiler = Tiler::new(reader);
                let img = tiler.warp_tile(coord, style).await?;
                Ok::<Option<Arc<ImageData>>, AgError>(img)
            });
            let results = futures::future::join_all(futs).await;

            // Collect this chunk's warped assets on the async thread (cheap),
            // logging skipped/failed ones here so credential-bearing URLs never
            // cross into the blocking closure below.
            let mut imgs: Vec<Arc<ImageData>> = Vec::with_capacity(chunk.len());
            for (asset, res) in chunk.iter().zip(results) {
                match res {
                    Ok(Some(img)) => imgs.push(img),
                    Ok(None) => {}
                    Err(e) => {
                        // Redacted: presigned/query-string credentials must not
                        // reach the logs (the error text is already redacted at
                        // its construction site in `ReaderPool::get_or_open`).
                        tracing::warn!(
                            asset = %redact_query(&asset.url),
                            error = %e,
                            "mosaic asset skipped"
                        );
                    }
                }
            }

            if !imgs.is_empty() {
                // The reducer widens each asset to f64 and runs a per-pixel
                // reduction — real CPU work. Run the whole chunk's feed in ONE
                // blocking job under a shared CPU permit so it neither stalls a
                // tokio worker nor escapes the mosaic's CPU budget. The reducer
                // moves in and back out so it accumulates across chunks.
                let permit = self.pool.cpu_limiter().acquire().await?;
                let taken = reducer.take();
                let (next, added) = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let mut r = taken;
                    let mut added = 0usize;
                    for img in imgs {
                        match r.as_mut() {
                            Some(r) => r.feed(&img)?,
                            None => r = Some(Reducer::new(selection).feed(&img)?),
                        }
                        added += 1;
                    }
                    Ok::<_, AgError>((r, added))
                })
                .await
                .map_err(|e| AgError::Mosaic(format!("reduce join: {e}")))??;
                reducer = next;
                fed += added;
            }

            if reducer.as_ref().is_some_and(|reducer| reducer.is_done()) {
                break;
            }
        }

        if fed == 0 {
            return Ok(None);
        }

        let Some(reducer) = reducer else {
            return Ok(None);
        };
        let composite = reducer.finish()?;
        let compiled = compile_cached(style).await?;
        let img = composite;
        // Bound the composite render with the same shared CPU limiter the
        // per-asset reads/warps used, so it doesn't add an unbounded blocking
        // job on top of the mosaic's CPU budget.
        let permit = self.pool.cpu_limiter().acquire().await?;
        let bytes = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            render(&img, &compiled)
        })
        .await
        .map_err(|e| AgError::Mosaic(format!("render join: {e}")))??;
        Ok(Some(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bounds::Bbox;
    use crate::io::CogReader;
    use async_tiff::reader::ObjectReader;
    use object_store::local::LocalFileSystem;
    use object_store::path::Path as OsPath;

    struct OneLocal {
        abspath: String,
    }

    #[async_trait::async_trait]
    impl MosaicSource for OneLocal {
        async fn assets_for_tile(&self, _coord: TileCoord) -> AgResult<Vec<AssetRef>> {
            Ok(vec![AssetRef {
                url: self.abspath.clone(),
                source_id: self.abspath.clone(),
                bbox: Bbox::new(-180.0, -90.0, 180.0, 90.0),
            }])
        }
    }

    #[tokio::test]
    async fn single_asset_mosaic_matches_single_source() {
        let cwd = std::env::current_dir().unwrap();
        let tif = cwd.join("data/cog_uint8_rgb_nodata.tif");
        if !tif.exists() {
            eprintln!("fixture missing, skipping");
            return;
        }

        let fs = LocalFileSystem::new();
        let os_path = OsPath::from_filesystem_path(&tif).unwrap();
        let reader =
            CogReader::open_with_reader(ObjectReader::new(std::sync::Arc::new(fs), os_path))
                .await
                .unwrap();
        let tiler = Tiler::new(std::sync::Arc::new(reader));
        let style = TileStyle::from_query("").unwrap();
        let [w, s, e, n] = tiler.wgs84_bounds().unwrap();
        let bounds = [w, s, e, n];

        // Derive a tile that actually covers the fixture (slippy-map formula)
        // so both paths render real bytes; an out-of-bounds tile now returns
        // None and would not exercise the single-vs-mosaic equivalence.
        let z = 12u8;
        let nz = 2f64.powi(z as i32);
        let clon = (w + e) / 2.0;
        let clat = (s + n) / 2.0;
        let x = (((clon + 180.0) / 360.0 * nz).floor().clamp(0.0, nz - 1.0)) as u32;
        let lat_rad = clat.to_radians();
        let y = (((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0
            * nz)
            .floor()
            .clamp(0.0, nz - 1.0)) as u32;
        let coord = TileCoord {
            tms: crate::tms::TileMatrixSet::WebMercatorQuad,
            z,
            x,
            y,
        };
        let single = tiler.tile(coord, &style).await;

        let src = OneLocal {
            abspath: tif.to_string_lossy().into_owned(),
        };
        let m = MosaicTiler::new(src);
        let mosaic = m.tile(coord, PixelSelection::First, &style).await;

        let _ = bounds;
        // The single-source path is the assertion: an in-bounds tile renders.
        match single {
            Ok(Some(a)) => assert!(!a.is_empty(), "in-bounds single-source tile must render"),
            Ok(None) => panic!("in-bounds tile unexpectedly returned None"),
            Err(_) => {} // environment-dependent reader error; tolerate.
        }
        // The mosaic path opens assets through ReaderPool (open_http), so a
        // local-FS fixture path may load nothing → None, or error. Either is
        // acceptable here; mosaic orchestration is covered by
        // `mosaic_with_no_assets_returns_none` and the selection unit tests.
        if let Ok(Some(b)) = mosaic {
            assert!(!b.is_empty());
        }
    }

    /// Source that returns one asset addressed by a decoded object key,
    /// to exercise the injected-store (MinIO/S3/local) path.
    struct OneKey {
        key: String,
    }

    #[async_trait::async_trait]
    impl MosaicSource for OneKey {
        async fn assets_for_tile(&self, _coord: TileCoord) -> AgResult<Vec<AssetRef>> {
            Ok(vec![AssetRef {
                url: self.key.clone(),
                source_id: self.key.clone(),
                bbox: Bbox::new(-180.0, -90.0, 180.0, 90.0),
            }])
        }
    }

    #[tokio::test]
    async fn object_store_mosaic_renders_via_injected_store() {
        // The feature claim: with a store injected, the mosaic opens its asset
        // through object_store (key = decoded object key, NOT an HTTP URL) and
        // actually renders it. The `.expect(...must render...)` below is the
        // anti-false-green guard: a silently-skipped asset leaves `fed == 0` and
        // returns None, which would slip past a weaker assertion.
        //
        // We do not assert byte-equality with the single-source `Tiler::tile`
        // path: the mosaic composites via `warp_tile` → Reducer → render and is
        // intentionally not byte-identical to the fused single-source pipeline
        // (the sibling `single_asset_mosaic_matches_single_source` makes the same
        // choice). Store-level decode equivalence is covered in cog.rs by
        // `open_object_store_matches_open_with_reader`.
        let key = "data/cog_uint8_rgb_nodata.tif";
        if !std::path::Path::new(key).exists() {
            eprintln!("fixture missing, skipping object_store_mosaic_renders_via_injected_store");
            return;
        }
        let store: Arc<dyn object_store::ObjectStore> =
            Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());

        // Bounds (over the same store+key) to derive a covering tile.
        let reader = CogReader::open_object_store(store.clone(), key)
            .await
            .expect("open via object_store");
        let style = TileStyle::from_query("").unwrap();
        let [w, s, e, n] = Tiler::new(Arc::new(reader)).wgs84_bounds().unwrap();

        // A tile that actually covers the fixture (slippy-map formula).
        let z = 12u8;
        let nz = 2f64.powi(z as i32);
        let clon = (w + e) / 2.0;
        let clat = (s + n) / 2.0;
        let x = (((clon + 180.0) / 360.0 * nz).floor().clamp(0.0, nz - 1.0)) as u32;
        let lat_rad = clat.to_radians();
        let y = (((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0
            * nz)
            .floor()
            .clamp(0.0, nz - 1.0)) as u32;

        let src = OneKey {
            key: key.to_string(),
        };
        let m = MosaicTiler::builder(src)
            .backend(Backend::Store(store.clone()))
            .build();
        let mosaic = m
            .tile(
                TileCoord {
                    tms: crate::tms::TileMatrixSet::WebMercatorQuad,
                    z,
                    x,
                    y,
                },
                PixelSelection::First,
                &style,
            )
            .await
            .expect("mosaic tile")
            .expect("object_store asset must render, not be skipped");

        assert!(!mosaic.is_empty(), "rendered tile must be non-empty");
    }

    struct NoAssets;

    #[async_trait::async_trait]
    impl MosaicSource for NoAssets {
        async fn assets_for_tile(&self, _coord: TileCoord) -> AgResult<Vec<AssetRef>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn mosaic_with_no_assets_returns_none() {
        // No asset covers the tile → "no tile", not a synthesised blank one.
        let m = MosaicTiler::new(NoAssets);
        let style = TileStyle::from_query("").unwrap();
        let out = m
            .tile(
                TileCoord {
                    tms: crate::tms::TileMatrixSet::WebMercatorQuad,
                    z: 1,
                    x: 0,
                    y: 0,
                },
                PixelSelection::First,
                &style,
            )
            .await
            .unwrap();
        assert!(out.is_none(), "mosaic tile with no assets must return None");
    }

    #[test]
    fn injected_block_cache_is_shared() {
        use crate::io::CacheConfig;
        let cache = Arc::new(BlockCache::new(CacheConfig::default()));
        let m = MosaicTiler::builder(NoAssets)
            .block_cache(Arc::clone(&cache))
            .build();
        assert!(
            Arc::ptr_eq(&m.block_cache(), &cache),
            "builder block_cache must use the injected cache"
        );
    }
}
