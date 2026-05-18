//! `MosaicSource` trait + `AssetRef`.

use crate::bounds::Bbox;
use crate::error::AgResult;

/// One COG covering a requested tile. `bbox` is WGS84 `[w,s,e,n]` as a
/// `Bbox`; producing it is the source implementation's responsibility
/// (quadkey index needs none; STAC Items carry it).
#[derive(Debug, Clone)]
pub struct AssetRef {
    pub url: String,
    pub bbox: Bbox,
}

/// Asset-discovery index. One implementation = one mosaic definition.
#[async_trait::async_trait]
pub trait MosaicSource: Send + Sync {
    /// COGs covering tile `(z, x, y)`, in paint order (first = top).
    async fn assets_for_tile(&self, z: u8, x: u32, y: u32) -> AgResult<Vec<AssetRef>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Vec<AssetRef>);

    #[async_trait::async_trait]
    impl MosaicSource for Fixed {
        async fn assets_for_tile(&self, _z: u8, _x: u32, _y: u32) -> AgResult<Vec<AssetRef>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn trait_object_is_usable() {
        let s: Box<dyn MosaicSource> = Box::new(Fixed(vec![AssetRef {
            url: "https://example.com/a.tif".into(),
            bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
        }]));
        let got = s.assets_for_tile(5, 1, 2).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].url, "https://example.com/a.tif");
    }
}
