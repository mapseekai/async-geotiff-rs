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
pub use source::{AssetRef, DiscoveredAssets, DiscoveryOptions, MosaicSource};
pub use ssrf::HttpAccessPolicy;
#[cfg(feature = "stac")]
pub use stac::StacSource;

use bytes::Bytes;

use std::sync::Arc;
use std::time::Duration;

use crate::error::{AgError, AgResult};
use crate::io::BlockCache;
use crate::pixel::ImageData;
use crate::request::{
    RequestContext, RequestLimiter, RequestOptions, DEFAULT_MAX_IN_FLIGHT_REQUESTS,
    DEFAULT_REQUEST_TIMEOUT,
};
use crate::resource::WorkBudget;
use crate::style::params::compile_cached;
use crate::style::TileStyle;
use crate::tiler::Tiler;
use crate::tms::TileCoord;

pub use reader_pool::Backend;

use reader_pool::ReaderPool;
use selection::{NonEmptyReducer, Reducer};

/// Tuning knobs for [`MosaicTiler`].
///
/// `chunk_size` bounds per-asset reads/warps except for `First`, which reads
/// sequentially until coverage is complete. All assets share the reader pool
/// CPU limiter. Median rejects requests whose retained inputs exceed 256 MiB.
#[derive(Clone, Copy, Debug)]
pub struct MosaicConfig {
    /// Assets fetched+warped in parallel per chunk. `First` always uses one
    /// asset at a time and stops once coverage is complete.
    pub chunk_size: usize,
    /// Max distinct opened COGs kept in the reader pool.
    pub reader_capacity: u64,
    /// Hard cap on assets processed for a single tile. A low-zoom request whose
    /// quadkey prefix matches a large fraction of the mosaic index could
    /// otherwise fan out to thousands of remote opens/reads/warps per tile (a
    /// fetch-amplification DoS). Requests exceeding this cap return an error. `0` disables the cap.
    pub max_assets_per_tile: usize,
    /// Maximum active public tile requests; excess requests fail immediately.
    pub max_in_flight_requests: usize,
    /// Whole request deadline, including source lookup and asset IO.
    pub request_timeout: Duration,
}

impl Default for MosaicConfig {
    fn default() -> Self {
        Self {
            chunk_size: 8,
            reader_capacity: 256,
            max_assets_per_tile: 64,
            max_in_flight_requests: DEFAULT_MAX_IN_FLIGHT_REQUESTS,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

/// Renders mosaicked map tiles from multiple COG assets via a [`MosaicSource`].
pub struct MosaicTiler<S: MosaicSource> {
    source: S,
    pool: ReaderPool,
    cfg: MosaicConfig,
    requests: RequestLimiter,
    work_budget: WorkBudget,
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
    requests: Option<RequestLimiter>,
    work_budget: WorkBudget,
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

    pub fn request_limiter(mut self, requests: RequestLimiter) -> Self {
        self.requests = Some(requests);
        self
    }

    pub fn work_budget(mut self, budget: WorkBudget) -> Self {
        self.work_budget = budget;
        self
    }

    pub fn build(self) -> MosaicTiler<S> {
        let mut pool = ReaderPool::builder(self.cfg.reader_capacity)
            .backend(self.backend)
            .work_budget(self.work_budget.clone());
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
            requests: self.requests.unwrap_or_else(|| {
                if self.cfg.max_in_flight_requests == DEFAULT_MAX_IN_FLIGHT_REQUESTS {
                    RequestLimiter::default()
                } else {
                    RequestLimiter::new(self.cfg.max_in_flight_requests)
                }
            }),
            work_budget: self.work_budget,
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
            requests: None,
            work_budget: WorkBudget::global(),
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

    /// Explicitly open a new asset snapshot on its next use.
    pub async fn invalidate_source(&self, key: &str, source_id: &str) {
        self.pool.invalidate(key, source_id).await;
    }

    /// Render one mosaicked tile using the document's mosaic selection and stage.
    /// Read/warp each asset, merge before or after channel expressions, then
    /// apply stretch, colors, Gamma and opacity once to the result.
    /// Required asset failures and fan-out overflow return errors, never partial tiles.
    /// `First` does not open lower-priority assets once coverage is complete.
    pub async fn tile(&self, coord: TileCoord, style: &TileStyle) -> AgResult<Option<Bytes>> {
        self.tile_with_options(coord, style, &RequestOptions::default())
            .await
    }

    /// Render with a per-call deadline override and explicit cancellation handle.
    pub async fn tile_with_options(
        &self,
        coord: TileCoord,
        style: &TileStyle,
        options: &RequestOptions,
    ) -> AgResult<Option<Bytes>> {
        self.requests
            .run(self.cfg.request_timeout, options, |context| async move {
                self.work_budget
                    .run_request(&context, |attempt| async move {
                        self.tile_in_request(coord, style, &attempt).await
                    })
                    .await
            })
            .await
    }

    async fn tile_in_request(
        &self,
        coord: TileCoord,
        style: &TileStyle,
        context: &RequestContext,
    ) -> AgResult<Option<Bytes>> {
        let mosaic = style.raster.mosaic.as_ref().ok_or_else(|| {
            AgError::query(
                "/raster:mosaic",
                "multi-source requests require pixel_selection and stage",
            )
        })?;
        let selection = mosaic.selection();
        let raster_rank = mosaic.rank;
        let mut context = context.clone();
        context.check()?;
        let compiled = compile_cached(style).await?;
        if self.cfg.chunk_size == 0 {
            return Err(AgError::Mosaic("chunk_size must be non-zero".into()));
        }
        style.raster.statistics.check_scope(true)?;
        let statistical = style.raster.stretch.needs_statistics();
        let discovery_options = DiscoveryOptions {
            max_assets: self.cfg.max_assets_per_tile,
            work_budget: self.work_budget.clone(),
            cpu_limiter: self.pool.cpu_limiter(),
            ..DiscoveryOptions::default()
        };
        let discovered = self
            .source
            .assets_for_tile_with_options(coord, &discovery_options)
            .await?;
        let assets = &discovered.assets;
        context.check()?;
        if assets.is_empty() {
            return Ok(None);
        }
        // Cap fan-out: a low-zoom prefix match can return a huge asset set;
        // opening/warping all of them per tile is a fetch-amplification DoS.
        let cap = self.cfg.max_assets_per_tile;
        if cap > 0 && assets.len() > cap {
            return Err(AgError::Mosaic(format!(
                "tile matches {} assets, exceeding limit {cap}",
                assets.len()
            )));
        }
        let mut reducer: Option<Reducer<NonEmptyReducer>> = None;
        let raster = if statistical {
            let discovered_statistics = self
                .source
                .statistics_assets_with_options(&discovery_options)
                .await?;
            let statistics_assets = &discovered_statistics.assets;
            if cap > 0 && statistics_assets.len() > cap {
                return Err(AgError::Mosaic(format!(
                    "statistics binds {} assets, exceeding limit {cap}",
                    statistics_assets.len()
                )));
            }
            let mut readers = Vec::with_capacity(statistics_assets.len());
            for asset in statistics_assets {
                context.check()?;
                readers.push(self.pool.get_or_open(&asset.url, &asset.source_id).await?);
            }
            crate::style::statistics::resolve(
                style,
                &readers,
                true,
                &self.pool.cpu_limiter(),
                &self.work_budget,
                &context,
            )
            .await?
        } else {
            compiled.raster.clone()
        };
        let mut fed = 0usize;
        let mut retained_bytes = 0usize;
        // First usually needs only one STAC scene. Check coverage before opening
        // the next asset instead of speculatively downloading a whole chunk.
        let chunk_size = if selection == PixelSelection::First {
            1
        } else {
            self.cfg.chunk_size
        };
        for chunk in assets.chunks(chunk_size) {
            context.check()?;
            let futs = chunk.iter().map(|a| {
                let context = &context;
                async move {
                    context.check()?;
                    let reader = self.pool.get_or_open(&a.url, &a.source_id).await?;
                    context.check()?;
                    let bands = u64::from(reader.data_ifd(0)?.samples_per_pixel())
                        .max(style.raster.mosaic_band_count() as u64);
                    // Warped image plus its temporary F64 reducer input.
                    // Streaming reducers release this after the chunk is fed;
                    // Median retains its samples until finish.
                    let bytes = u64::from(style.tilesize)
                        .checked_pow(2)
                        .and_then(|pixels| {
                            bands
                                .checked_mul(16)
                                .and_then(|n| n.checked_add(2))
                                .and_then(|n| pixels.checked_mul(n))
                        })
                        .ok_or_else(|| {
                            AgError::ResourceLimit("mosaic working-set estimate overflow".into())
                        })?;
                    let reservation = self.work_budget.try_reserve(bytes)?;
                    let child = context.with_work(reservation.clone());
                    let tiler = Tiler::new(reader)
                        .with_request_limiter(self.requests.clone())
                        .with_work_budget(self.work_budget.clone());
                    let img = tiler.warp_tile_in_request(coord, style, &child).await?;
                    Ok::<_, AgError>((img, reservation))
                }
            });
            let results = futures::future::try_join_all(futs).await?;

            // Propagate failures: partial output is not a successful tile.
            let mut imgs: Vec<Arc<ImageData>> = Vec::with_capacity(chunk.len());
            let mut chunk_work = Vec::with_capacity(chunk.len());
            context.check()?;
            for (image, reservation) in results {
                if let Some(img) = image {
                    if reducer.is_none() && imgs.is_empty() {
                        // One accumulator/final-render allowance for the entire mosaic.
                        let bytes = (img.pixels.bands() as u64)
                            .checked_mul(64)
                            .and_then(|n| n.checked_add(48))
                            .and_then(|n| n.checked_mul(img.pixels.width() as u64))
                            .and_then(|n| n.checked_mul(img.pixels.height() as u64))
                            .ok_or_else(|| {
                                AgError::ResourceLimit(
                                    "mosaic accumulator estimate overflow".into(),
                                )
                            })?;
                        context = context.with_work(self.work_budget.try_reserve(bytes)?);
                    }
                    if selection == PixelSelection::Median {
                        context = context.with_work(reservation);
                        retained_bytes = median_retained_bytes(
                            retained_bytes,
                            img.pixels.bands(),
                            img.pixels.height(),
                            img.pixels.width(),
                        )?;
                    } else {
                        chunk_work.push(reservation);
                    }
                    imgs.push(img);
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
                let cancellation = context.clone();
                let (next, added) = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let _chunk_work = chunk_work;
                    cancellation.check()?;
                    let mut r = taken;
                    let mut added = 0usize;
                    for img in imgs {
                        cancellation.check()?;
                        match r.as_mut() {
                            Some(r) => r.feed(&img)?,
                            None => {
                                let reducer = Reducer::new_raster(selection, raster_rank);
                                r = Some(reducer.feed(&img)?);
                            }
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

        // Bound the composite render with the same shared CPU limiter the
        // per-asset reads/warps used, so it doesn't add an unbounded blocking
        // job on top of the mosaic's CPU budget.
        let permit = self.pool.cpu_limiter().acquire().await?;
        let cancellation = context.clone();
        let bytes = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            cancellation.check()?;
            let img = reducer.finish()?;
            cancellation.check()?;
            raster.render_mosaic(&img, compiled.format)
        })
        .await
        .map_err(|e| AgError::Mosaic(format!("render join: {e}")))??;
        Ok(Some(bytes))
    }
}

fn median_retained_bytes(
    current: usize,
    bands: usize,
    height: usize,
    width: usize,
) -> AgResult<usize> {
    // f64 pixels plus validity masks; conservative one mask byte per band.
    bands
        .checked_mul(height)
        .and_then(|n| n.checked_mul(width))
        .and_then(|n| n.checked_mul(9))
        .and_then(|n| current.checked_add(n))
        .filter(|&n| n <= 256 * 1024 * 1024)
        .ok_or_else(|| AgError::Mosaic("median retained memory budget exceeds 256 MiB".into()))
}

#[cfg(test)]
mod tests {
    fn mosaic_style(selection: PixelSelection, style: &TileStyle) -> TileStyle {
        let mut document: serde_json::Value =
            serde_json::from_str(style.raster_style_json()).unwrap();
        document["raster:mosaic"] = serde_json::json!({"pixel_selection": format!("{selection:?}").to_lowercase(), "stage": "after_channels"});
        let declarations = document
            .as_object_mut()
            .unwrap()
            .entry("extensions")
            .or_insert_with(|| serde_json::json!([]));
        declarations
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!(raster_style::MOSAIC_SCHEMA_URL));
        TileStyle::from_json(&document.to_string()).unwrap()
    }

    use super::*;
    use crate::bounds::Bbox;
    use crate::io::CogReader;

    use object_store::local::LocalFileSystem;

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
        for key in [
            "tests/fixtures/rgb_nodata.tif",
            "tests/fixtures/uint16.tif",
            "tests/fixtures/projected.tif",
        ] {
            let store: Arc<dyn object_store::ObjectStore> =
                Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());

            // Bounds (over the same store+key) to derive a covering tile.
            let reader = CogReader::builder()
                .snapshot_policy(crate::io::SnapshotPolicy::AssumeImmutable)
                .open_object_store(store.clone(), key)
                .await
                .expect("open via object_store");
            let single = Tiler::new(Arc::new(reader));
            let [w, s, e, n] = single.wgs84_bounds().unwrap();

            // A tile that actually covers the fixture (slippy-map formula).
            let z = 12u8;
            let nz = 2f64.powi(z as i32);
            let clon = (w + e) / 2.0;
            let clat = (s + n) / 2.0;
            let x = (((clon + 180.0) / 360.0 * nz).floor().clamp(0.0, nz - 1.0)) as u32;
            let lat_rad = clat.to_radians();
            let y = (((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI)
                / 2.0
                * nz)
                .floor()
                .clamp(0.0, nz - 1.0)) as u32;

            let src = OneKey {
                key: key.to_string(),
            };
            let m = MosaicTiler::builder(src)
                .backend(Backend::Store(store.clone()))
                .build();
            let coord = TileCoord {
                tms: crate::tms::TileMatrixSet::WebMercatorQuad,
                z,
                x,
                y,
            };
            for query in [
                "version=3.0&type=gray&bidx=1",
                "version=3.0&type=gray&bidx=1&method=linear&rescale=0,1000",
                "version=3.0&type=gray&expression=b1*2",
                "version=3.0&type=pseudocolor&bidx=1&rescale=0,200&colormap_name=viridis&method=linear",
                "version=3.0&type=gray&nodata=100&reproject=bilinear&bidx=1",
            ] {
                let style = TileStyle::from_raster_query(query).unwrap();
                let expected = single
                    .tile(coord, &style)
                    .await
                    .unwrap()
                    .expect("fixture must cover tile");
                let actual = m
                    .tile(coord, &mosaic_style(PixelSelection::First, &style))
                    .await
                    .unwrap()
                    .expect("asset must render");
                assert_eq!(expected, actual, "single asset equivalence: {key} {query}");
            }
        }
    }

    struct Keys(Vec<String>);

    #[tokio::test]
    async fn streaming_reducers_release_finished_chunk_reservations() {
        let budget = WorkBudget::new(64 * 1024 * 1024);
        let store = Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        let mosaic = MosaicTiler::builder(Keys(vec!["tests/fixtures/rgb_nodata.tif".into(); 20]))
            .backend(Backend::Store(store))
            .work_budget(budget.clone())
            .config(MosaicConfig {
                chunk_size: 1,
                ..Default::default()
            })
            .build();
        let coord = TileCoord {
            tms: crate::tms::TileMatrixSet::WorldCrs84Quad,
            z: 10,
            x: 1081,
            y: 398,
        };
        assert!(mosaic
            .tile(
                coord,
                &mosaic_style(PixelSelection::Mean, &TileStyle::default())
            )
            .await
            .unwrap()
            .is_some());
        assert!(budget.peak_bytes() <= budget.limit_bytes());
        drop(mosaic);
        assert_eq!(budget.used_bytes(), 0);
    }
    #[async_trait::async_trait]
    impl MosaicSource for Keys {
        async fn assets_for_tile(&self, _: TileCoord) -> AgResult<Vec<AssetRef>> {
            Ok(self
                .0
                .iter()
                .map(|key| AssetRef {
                    url: key.clone(),
                    source_id: key.clone(),
                    bbox: Bbox::new(-180.0, -90.0, 180.0, 90.0),
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn cap_overflow_is_error_before_assets_are_opened() {
        let mosaic = MosaicTiler::builder(Keys(vec!["missing-a".into(), "missing-b".into()]))
            .config(MosaicConfig {
                max_assets_per_tile: 1,
                ..Default::default()
            })
            .build();
        let coord = TileCoord {
            tms: crate::tms::TileMatrixSet::WebMercatorQuad,
            z: 1,
            x: 0,
            y: 0,
        };
        let error = mosaic
            .tile(
                coord,
                &mosaic_style(PixelSelection::Mean, &TileStyle::default()),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeding limit"));
    }

    #[tokio::test]
    async fn failed_asset_does_not_return_a_partial_composite() {
        let store = Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        let mosaic = MosaicTiler::builder(Keys(vec![
            "tests/fixtures/rgb_nodata.tif".into(),
            "missing.tif".into(),
        ]))
        .backend(Backend::Store(store))
        .config(MosaicConfig {
            chunk_size: 1,
            ..Default::default()
        })
        .build();
        // Fixture is near 10E,20N; first chunk contributes pixels before failure.
        let coord = TileCoord {
            tms: crate::tms::TileMatrixSet::WorldCrs84Quad,
            z: 10,
            x: 1081,
            y: 398,
        };
        assert!(mosaic
            .tile(
                coord,
                &mosaic_style(PixelSelection::Mean, &TileStyle::default())
            )
            .await
            .is_err());
    }

    #[test]
    fn median_budget_counts_all_chunks_and_checks_overflow() {
        let first = median_retained_bytes(0, 3, 2048, 2048).unwrap();
        let second = median_retained_bytes(first, 3, 2048, 2048).unwrap();
        assert!(median_retained_bytes(second, 3, 2048, 2048).is_err());
        assert!(median_retained_bytes(0, usize::MAX, 2, 2).is_err());
    }

    struct StalledSource;
    #[async_trait::async_trait]
    impl MosaicSource for StalledSource {
        async fn assets_for_tile(&self, _: TileCoord) -> AgResult<Vec<AssetRef>> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn source_io_stall_obeys_request_deadline() {
        let mosaic = MosaicTiler::builder(StalledSource)
            .config(MosaicConfig {
                request_timeout: Duration::from_millis(10),
                ..Default::default()
            })
            .build();
        let coord = TileCoord {
            tms: crate::tms::TileMatrixSet::WebMercatorQuad,
            z: 1,
            x: 0,
            y: 0,
        };
        assert!(matches!(
            mosaic
                .tile(
                    coord,
                    &mosaic_style(PixelSelection::First, &TileStyle::default())
                )
                .await,
            Err(AgError::DeadlineExceeded)
        ));
    }

    #[tokio::test]
    async fn child_assets_share_one_admission_without_reentering() {
        let limiter = RequestLimiter::new(1);
        let store = Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        for count in [1, 2] {
            let mosaic =
                MosaicTiler::builder(Keys(vec!["tests/fixtures/rgb_nodata.tif".into(); count]))
                    .backend(Backend::Store(store.clone()))
                    .request_limiter(limiter.clone())
                    .build();
            let coord = TileCoord {
                tms: crate::tms::TileMatrixSet::WorldCrs84Quad,
                z: 10,
                x: 1081,
                y: 398,
            };
            assert!(mosaic
                .tile(
                    coord,
                    &mosaic_style(PixelSelection::Mean, &TileStyle::default())
                )
                .await
                .unwrap()
                .is_some());
            assert_eq!(limiter.available_permits(), 1);
        }
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
        let style = TileStyle::from_raster_query("version=3.0&type=gray&bidx=1").unwrap();
        let out = m
            .tile(
                TileCoord {
                    tms: crate::tms::TileMatrixSet::WebMercatorQuad,
                    z: 1,
                    x: 0,
                    y: 0,
                },
                &mosaic_style(PixelSelection::First, &style),
            )
            .await
            .unwrap();
        assert!(out.is_none(), "mosaic tile with no assets must return None");
    }

    #[tokio::test]
    async fn invalid_style_and_config_fail_before_source_lookup() {
        let coord = TileCoord {
            tms: crate::tms::TileMatrixSet::WebMercatorQuad,
            z: 1,
            x: 0,
            y: 0,
        };
        let style = TileStyle::default();
        assert!(MosaicTiler::new(NoAssets)
            .tile(coord, &style)
            .await
            .is_err());
        let mosaic = MosaicTiler::builder(NoAssets)
            .config(MosaicConfig {
                chunk_size: 0,
                ..Default::default()
            })
            .build();
        assert!(mosaic
            .tile(
                coord,
                &mosaic_style(PixelSelection::First, &TileStyle::default())
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn failed_asset_is_error_not_empty_coverage() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let mosaic = MosaicTiler::builder(OneKey {
            key: "missing.tif".into(),
        })
        .backend(Backend::Store(store))
        .build();
        let coord = TileCoord {
            tms: crate::tms::TileMatrixSet::WebMercatorQuad,
            z: 1,
            x: 0,
            y: 0,
        };
        assert!(mosaic
            .tile(
                coord,
                &mosaic_style(PixelSelection::First, &TileStyle::default())
            )
            .await
            .is_err());
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
