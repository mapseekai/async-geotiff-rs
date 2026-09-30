//! `MosaicSource` trait + `AssetRef`.

use crate::bounds::Bbox;
use crate::error::AgResult;
use crate::tms::TileCoord;
use crate::{
    blocking::CpuLimiter,
    resource::{WorkBudget, WorkReservation},
};
use std::{collections::HashSet, sync::OnceLock};

/// Discovery limits, separate from the decoded raster working set.
/// Zero `max_assets` disables only the count limit; byte/page limits still apply.
#[derive(Clone, Debug)]
pub struct DiscoveryOptions {
    pub max_assets: usize,
    pub max_asset_bytes: usize,
    pub max_response_bytes: usize,
    pub max_total_response_bytes: usize,
    pub max_pages: usize,
    pub work_budget: WorkBudget,
    pub cpu_limiter: CpuLimiter,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        static CPU: OnceLock<CpuLimiter> = OnceLock::new();
        Self {
            max_assets: 64,
            max_asset_bytes: 8 * 1024 * 1024,
            max_response_bytes: 1024 * 1024,
            max_total_response_bytes: 8 * 1024 * 1024,
            max_pages: 128,
            work_budget: WorkBudget::global(),
            cpu_limiter: CPU.get_or_init(CpuLimiter::default).clone(),
        }
    }
}

/// Assets in paint order, retaining discovery working-memory reservations.
/// The caller owns unbudgeted output after calling `into_assets`.
pub struct DiscoveredAssets {
    pub(crate) assets: Vec<AssetRef>,
    _work: Vec<WorkReservation>,
}

impl DiscoveredAssets {
    pub fn assets(&self) -> &[AssetRef] {
        &self.assets
    }
    pub fn into_assets(self) -> Vec<AssetRef> {
        self.assets
    }
    pub fn from_assets(assets: Vec<AssetRef>, options: &DiscoveryOptions) -> AgResult<Self> {
        let mut builder = AssetDiscovery::new(options.clone());
        for asset in assets {
            builder.push(&asset.url, &asset.source_id, asset.bbox)?;
        }
        Ok(builder.finish())
    }
}

pub(crate) struct AssetDiscovery {
    options: DiscoveryOptions,
    seen: HashSet<String>,
    assets: Vec<AssetRef>,
    work: Vec<WorkReservation>,
    bytes: usize,
}

impl AssetDiscovery {
    pub(crate) fn new(options: DiscoveryOptions) -> Self {
        Self {
            options,
            seen: HashSet::new(),
            assets: Vec::new(),
            work: Vec::new(),
            bytes: 0,
        }
    }
    pub(crate) fn push(&mut self, url: &str, source_id: &str, bbox: Bbox) -> AgResult<()> {
        if let Some(request) = crate::request::RequestContext::current() {
            request.check()?;
        }
        if self.seen.contains(url) {
            return Ok(());
        }
        let count = self.assets.len() + 1;
        if self.options.max_assets > 0 && count > self.options.max_assets {
            return Err(crate::error::AgError::Mosaic(format!(
                "tile matches at least {count} assets, exceeding limit {}",
                self.options.max_assets
            )));
        }
        // Include URL identity, hash table, Vec capacity and lease overhead.
        let bytes = url
            .len()
            .checked_mul(2)
            .and_then(|n| n.checked_add(source_id.len()))
            .and_then(|n| n.checked_add(512))
            .and_then(|n| self.bytes.checked_add(n))
            .filter(|n| *n <= self.options.max_asset_bytes)
            .ok_or_else(|| {
                crate::error::AgError::ResourceLimit(
                    "discovered asset metadata exceeds byte limit".into(),
                )
            })?;
        self.work.push(
            self.options
                .work_budget
                .try_reserve((bytes - self.bytes) as u64)?,
        );
        self.bytes = bytes;
        self.seen.insert(url.to_owned());
        self.assets.push(AssetRef {
            url: url.to_owned(),
            source_id: source_id.to_owned(),
            bbox,
        });
        Ok(())
    }
    pub(crate) fn finish(self) -> DiscoveredAssets {
        DiscoveredAssets {
            assets: self.assets,
            _work: self.work,
        }
    }
}

/// One COG covering a requested tile. `bbox` is WGS84 `[w,s,e,n]` as a
/// `Bbox`; producing it is the source implementation's responsibility
/// (quadkey index needs none; STAC Items carry it).
#[derive(Debug, Clone)]
pub struct AssetRef {
    /// Address of the COG. Interpretation depends on how the `MosaicTiler` was
    /// built: by default (HTTP mode) this is an HTTP(S) URL; when the tiler is
    /// constructed with `MosaicTiler::with_object_store`, it is instead a
    /// *decoded object key* within the injected store (e.g. `"scenes/a.tif"`),
    /// not a URL — no scheme/host, and not percent-encoded.
    pub url: String,
    /// Stable cache identity for this COG, supplied by the `MosaicSource`.
    /// Decoded blocks are keyed by it, so use the **same** `source_id` here and
    /// when opening the same COG via single-dataset `ReaderConfig` to reuse
    /// decoded blocks across both paths (given a shared `BlockCache`). The
    /// library's built-in sources set it to the asset URL.
    pub source_id: String,
    /// Source-provided bounding box (WGS84) intended for the `MosaicSource`
    /// implementation's own spatial filtering. The orchestrator does not
    /// currently consume this field — it is part of the contract by design
    /// so implementations can use it without a separate lookup.
    pub bbox: Bbox,
}

/// Asset-discovery index. One implementation = one mosaic definition.
#[async_trait::async_trait]
pub trait MosaicSource: Send + Sync {
    /// COGs covering `coord`, in paint order (first = top).
    async fn assets_for_tile(&self, coord: TileCoord) -> AgResult<Vec<AssetRef>>;

    /// Built-in sources enforce these limits before materializing assets.
    /// Custom sources should override this to bound their own discovery IO and
    /// allocations; this compatibility default can only validate returned data.
    async fn assets_for_tile_with_options(
        &self,
        coord: TileCoord,
        options: &DiscoveryOptions,
    ) -> AgResult<DiscoveredAssets> {
        DiscoveredAssets::from_assets(self.assets_for_tile(coord).await?, options)
    }

    /// Bounded counterpart of `statistics_assets`; custom sources should override
    /// it when constructing their fixed collection can allocate substantial data.
    async fn statistics_assets_with_options(
        &self,
        options: &DiscoveryOptions,
    ) -> AgResult<DiscoveredAssets> {
        DiscoveredAssets::from_assets(self.statistics_assets().await?, options)
    }

    /// Complete immutable source collection in paint order for shared statistics.
    /// Its full-footprint composite must match the tile selection semantics;
    /// sources with spatial index masks or regional ordering must return an error.
    /// Dynamic sources must bind a finite collection before enabling statistical stretch.
    async fn statistics_assets(&self) -> AgResult<Vec<AssetRef>> {
        Err(crate::error::AgError::query(
            "statistics",
            "this mosaic source does not bind a complete fixed asset collection",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Vec<AssetRef>);

    #[async_trait::async_trait]
    impl MosaicSource for Fixed {
        async fn assets_for_tile(&self, _coord: TileCoord) -> AgResult<Vec<AssetRef>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn trait_object_is_usable() {
        let s: Box<dyn MosaicSource> = Box::new(Fixed(vec![AssetRef {
            url: "https://example.com/a.tif".into(),
            source_id: "https://example.com/a.tif".into(),
            bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
        }]));
        let got = s
            .assets_for_tile(TileCoord {
                tms: crate::tms::TileMatrixSet::WebMercatorQuad,
                z: 5,
                x: 1,
                y: 2,
            })
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].url, "https://example.com/a.tif");
    }
}
