//! `MosaicSource` trait + `AssetRef`.

use crate::bounds::Bbox;
use crate::error::AgResult;
use crate::tms::TileMatrixSet;

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
    /// COGs covering tile `(tile_matrix_set, z, x, y)`, in paint order (first = top).
    async fn assets_for_tile(
        &self,
        tile_matrix_set: TileMatrixSet,
        z: u8,
        x: u32,
        y: u32,
    ) -> AgResult<Vec<AssetRef>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Vec<AssetRef>);

    #[async_trait::async_trait]
    impl MosaicSource for Fixed {
        async fn assets_for_tile(
            &self,
            _tile_matrix_set: TileMatrixSet,
            _z: u8,
            _x: u32,
            _y: u32,
        ) -> AgResult<Vec<AssetRef>> {
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
            .assets_for_tile(TileMatrixSet::WebMercatorQuad, 5, 1, 2)
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].url, "https://example.com/a.tif");
    }
}
