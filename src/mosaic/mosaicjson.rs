//! `MosaicJsonSource` — read-only consumer of a pre-built cogeo-mosaic
//! MosaicJSON document (quadkey index). Building the index is out of scope.

use std::collections::HashMap;

use serde::Deserialize;

use crate::bounds::Bbox;
use crate::error::{AgError, AgResult};
use crate::mosaic::quadkey::{lookup, QkLookup};
use crate::mosaic::source::{AssetRef, MosaicSource};

/// MosaicJSON 0.0.x subset we consume. Unknown fields ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct MosaicJson {
    pub mosaicjson: String,
    pub minzoom: u8,
    pub maxzoom: u8,
    #[serde(default)]
    pub quadkey_zoom: Option<u8>,
    #[serde(default = "world_bounds")]
    pub bounds: [f64; 4],
    pub tiles: HashMap<String, Vec<String>>,
}

fn world_bounds() -> [f64; 4] {
    [-180.0, -90.0, 180.0, 90.0]
}

pub struct MosaicJsonSource {
    doc: MosaicJson,
    quadkey_zoom: u8,
    bbox: Bbox,
}

impl MosaicJsonSource {
    /// Parse a MosaicJSON document from raw JSON bytes/string.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(json: &str) -> AgResult<Self> {
        let doc: MosaicJson = serde_json::from_str(json)
            .map_err(|e| AgError::Mosaic(format!("parse MosaicJSON: {e}")))?;
        if doc.tiles.is_empty() {
            return Err(AgError::Mosaic("MosaicJSON has no tiles".into()));
        }
        let quadkey_zoom = doc.quadkey_zoom.unwrap_or(doc.minzoom);
        let b = doc.bounds;
        Ok(Self {
            quadkey_zoom,
            bbox: Bbox::new(b[0], b[1], b[2], b[3]),
            doc,
        })
    }

    /// WGS84 bounds of the whole mosaic (mosaic-level, not per-asset).
    pub fn bounds(&self) -> Bbox {
        self.bbox
    }
}

#[async_trait::async_trait]
impl MosaicSource for MosaicJsonSource {
    async fn assets_for_tile(&self, z: u8, x: u32, y: u32) -> AgResult<Vec<AssetRef>> {
        let mut urls: Vec<String> = Vec::new();
        match lookup(z, x, y, self.quadkey_zoom) {
            QkLookup::Exact(key) => {
                if let Some(v) = self.doc.tiles.get(&key) {
                    urls.extend(v.iter().cloned());
                }
            }
            QkLookup::Prefix(prefix) => {
                for (k, v) in &self.doc.tiles {
                    if k.starts_with(&prefix) {
                        urls.extend(v.iter().cloned());
                    }
                }
            }
        }
        urls.sort();
        urls.dedup();
        Ok(urls
            .into_iter()
            .map(|url| AssetRef {
                url,
                bbox: self.bbox,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{
      "mosaicjson": "0.0.3",
      "minzoom": 7,
      "maxzoom": 12,
      "quadkey_zoom": 7,
      "bounds": [0.0, 0.0, 10.0, 10.0],
      "tiles": {
        "1230123": ["https://x/a.tif", "https://x/b.tif"],
        "1230120": ["https://x/c.tif"]
      }
    }"#;

    #[tokio::test]
    async fn exact_zoom_returns_listed_assets() {
        let s = MosaicJsonSource::from_str(DOC).unwrap();
        let qk = "1230123";
        let (mut tx, mut ty) = (0u32, 0u32);
        for ch in qk.chars() {
            tx <<= 1;
            ty <<= 1;
            match ch {
                '0' => {}
                '1' => tx |= 1,
                '2' => ty |= 1,
                '3' => {
                    tx |= 1;
                    ty |= 1;
                }
                _ => unreachable!(),
            }
        }
        let got = s.assets_for_tile(7, tx, ty).await.unwrap();
        let urls: Vec<_> = got.iter().map(|a| a.url.as_str()).collect();
        assert_eq!(urls, vec!["https://x/a.tif", "https://x/b.tif"]);
    }

    #[test]
    fn empty_tiles_is_error() {
        let bad = r#"{"mosaicjson":"0.0.3","minzoom":1,"maxzoom":2,"tiles":{}}"#;
        assert!(MosaicJsonSource::from_str(bad).is_err());
    }
}
