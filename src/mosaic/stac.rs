//! `StacSource` — dynamic asset discovery via a STAC API `/search`.
//! Behind `feature = "stac"` (pulls `stac` / `stac-client`).
//!
//! ## Real API (stac-client 0.3.0)
//!
//! - `stac_client::Client::new(url) -> Result<Client>` — synchronous constructor.
//! - `stac_client::SearchBuilder::new().collections(…).bbox(…).build()` produces
//!   `SearchParams`; passed by reference to `client.search(&params).await`.
//! - `client.search(…)` returns `Result<ItemCollection>` (a plain `Vec<Item>` in
//!   `.features`), **not** a stream.  No `futures::StreamExt` needed.
//! - `stac_client::models::Item` — `assets: HashMap<String, Asset>`,
//!   `bbox: Option<Vec<f64>>` (4- or 6-element STAC bbox).
//!
//! The `stac` 0.17 crate is also a dependency but `stac_client` 0.3 bundles its
//! own `Item`/`Asset` models in `stac_client::models`; the top-level `stac` crate
//! types are not referenced here.

use stac_client::{Client, SearchBuilder};

use crate::bounds::Bbox;
use crate::error::{AgError, AgResult};
use crate::mosaic::source::{AssetRef, MosaicSource};
use crate::tms::{Tms, TmsKind};

/// A [`MosaicSource`] that discovers COG assets by querying a STAC API's
/// `POST /search` endpoint for items intersecting the requested tile's bbox.
///
/// The client is cheap to clone (internally `Arc`-wrapped), so `StacSource`
/// is `Clone` and `Send + Sync`.
#[derive(Clone, Debug)]
pub struct StacSource {
    client: Client,
    collections: Vec<String>,
    asset_key: String,
}

impl StacSource {
    /// Create a new `StacSource`.
    ///
    /// `api_url` must be a valid URL (e.g. `"https://example.com/stac/v1"`).
    /// `collections` is the list of STAC collection IDs to search within.
    /// `asset_key` is the key inside each item's `assets` map to use as the
    /// COG href (e.g. `"data"` or `"B04"`).
    ///
    /// # Errors
    ///
    /// Returns `AgError::Mosaic` if `api_url` is not a valid URL.
    pub fn new(
        api_url: &str,
        collections: Vec<String>,
        asset_key: impl Into<String>,
    ) -> AgResult<Self> {
        let client = Client::new(api_url)
            .map_err(|e| AgError::Mosaic(format!("stac client: {e}")))?;
        Ok(Self {
            client,
            collections,
            asset_key: asset_key.into(),
        })
    }
}

#[async_trait::async_trait]
impl MosaicSource for StacSource {
    async fn assets_for_tile(&self, z: u8, x: u32, y: u32) -> AgResult<Vec<AssetRef>> {
        // 1. Compute tile bbox in Web Mercator (EPSG:3857), then convert to
        //    WGS84 (EPSG:4326) for the STAC bbox filter.
        let tms = Tms::new(TmsKind::WebMercatorQuad)?;
        let tb = tms.tile_bbox(z, x, y)?;

        // Four corners of the tile in EPSG:3857.
        let mut corners = [
            (tb.xmin, tb.ymin),
            (tb.xmax, tb.ymin),
            (tb.xmax, tb.ymax),
            (tb.xmin, tb.ymax),
        ];
        // convert_batch handles the 3857→4326 fast path without PROJ.
        crate::warp::projection::convert_batch(
            &tms.crs(),
            &crate::crs::Crs::epsg(4326),
            &mut corners,
        )?;
        let wb = Bbox::from_corners(&corners);

        // 2. Build search params: bbox + collections.
        //    `SearchBuilder::bbox` takes `Vec<f64>` in [west, south, east, north] order.
        let params = SearchBuilder::new()
            .collections(self.collections.clone())
            .bbox(vec![wb.xmin, wb.ymin, wb.xmax, wb.ymax])
            .build();

        // 3. Execute POST /search and collect results.
        let item_collection = self
            .client
            .search(&params)
            .await
            .map_err(|e| AgError::Mosaic(format!("stac search: {e}")))?;

        // 4. For each returned item, resolve asset href + bbox.
        let mut out = Vec::new();
        for item in &item_collection.features {
            let Some(asset) = item.assets.get(&self.asset_key) else {
                continue;
            };

            // STAC bbox is [w, s, e, n] (4 elements) or [w, s, minz, e, n, maxz] (6 elements).
            let bbox = item
                .bbox
                .as_ref()
                .and_then(|b| {
                    if b.len() == 4 {
                        Some(Bbox::new(b[0], b[1], b[2], b[3]))
                    } else if b.len() >= 6 {
                        // 6-element form: w=0, s=1, minz=2, e=3, n=4, maxz=5
                        Some(Bbox::new(b[0], b[1], b[3], b[4]))
                    } else {
                        None
                    }
                })
                .unwrap_or(wb);

            out.push(AssetRef {
                url: asset.href.clone(),
                bbox,
            });
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_rejects_invalid_url() {
        let result = StacSource::new("not-a-url", vec![], "data");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("stac client"), "unexpected error: {msg}");
    }

    #[test]
    fn constructor_accepts_valid_url() {
        let src = StacSource::new(
            "https://planetarycomputer.microsoft.com/api/stac/v1",
            vec!["sentinel-2-l2a".to_string()],
            "B04",
        );
        assert!(src.is_ok());
    }
}
