//! `MosaicJsonSource` — read-only consumer of a pre-built cogeo-mosaic
//! MosaicJSON document (quadkey index). Building the index is out of scope.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Deserialize;
use serde_json::Value;

use crate::bounds::Bbox;
use crate::error::{AgError, AgResult};
use crate::mosaic::quadkey::{lookup, QkLookup};
use crate::mosaic::source::{AssetRef, MosaicSource};
use crate::tms::{TileCoord, TileMatrixSet};

// MosaicJson mirrors the MosaicJSON 0.0.3 schema. Some fields are not
// interpreted by the renderer yet, but they are preserved so crate users can
// expose metadata or implement their own policy on top.
/// MosaicJSON 0.0.3 document.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct MosaicJson {
    pub mosaicjson: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub attribution: Option<String>,
    pub minzoom: u8,
    pub maxzoom: u8,
    #[serde(default)]
    pub quadkey_zoom: Option<u8>,
    #[serde(default = "world_bounds")]
    pub bounds: [f64; 4],
    #[serde(default)]
    pub center: Option<[f64; 3]>,
    pub tiles: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub asset_type: Option<String>,
    #[serde(default)]
    pub asset_prefix: Option<String>,
    #[serde(default)]
    pub data_type: Option<String>,
    #[serde(default)]
    pub colormap: BTreeMap<String, [f64; 4]>,
    #[serde(default)]
    pub tilematrixset: Option<Value>,
    #[serde(default)]
    pub layers: BTreeMap<String, Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MosaicDataType {
    Int8,
    Int16,
    Int32,
    Int64,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    Float16,
    Float32,
    Float64,
    CInt16,
    CInt32,
    CFloat32,
    CFloat64,
    Other,
}

impl MosaicDataType {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "int8" => Some(Self::Int8),
            "int16" => Some(Self::Int16),
            "int32" => Some(Self::Int32),
            "int64" => Some(Self::Int64),
            "uint8" => Some(Self::UInt8),
            "uint16" => Some(Self::UInt16),
            "uint32" => Some(Self::UInt32),
            "uint64" => Some(Self::UInt64),
            "float16" => Some(Self::Float16),
            "float32" => Some(Self::Float32),
            "float64" => Some(Self::Float64),
            "cint16" => Some(Self::CInt16),
            "cint32" => Some(Self::CInt32),
            "cfloat32" => Some(Self::CFloat32),
            "cfloat64" => Some(Self::CFloat64),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

fn world_bounds() -> [f64; 4] {
    [-180.0, -90.0, 180.0, 90.0]
}

fn default_version() -> String {
    "1.0.0".into()
}

/// A [`MosaicSource`] backed by a cogeo-mosaic MosaicJSON quadkey index.
///
/// MosaicJSON quadkey indices are defined in the WebMercatorQuad tile matrix;
/// using this source with another TMS is not meaningful.
pub struct MosaicJsonSource {
    doc: MosaicJson,
    tile_keys: Vec<String>,
    quadkey_zoom: u8,
    bbox: Bbox,
}

impl MosaicJsonSource {
    /// Parse a MosaicJSON document from raw JSON bytes/string.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(json: &str) -> AgResult<Self> {
        let mut doc: MosaicJson = serde_json::from_str(json)
            .map_err(|e| AgError::Mosaic(format!("parse MosaicJSON: {e}")))?;
        validate_document(&mut doc)?;
        let quadkey_zoom = doc.quadkey_zoom.unwrap_or(doc.minzoom);
        let b = doc.bounds;
        let mut tile_keys: Vec<_> = doc.tiles.keys().cloned().collect();
        tile_keys.sort_unstable();
        Ok(Self {
            tile_keys,
            quadkey_zoom,
            bbox: Bbox::new(b[0], b[1], b[2], b[3]),
            doc,
        })
    }

    /// WGS84 bounds of the whole mosaic (mosaic-level, not per-asset).
    pub fn bounds(&self) -> Bbox {
        self.bbox
    }

    /// Parsed MosaicJSON document, including metadata and unknown extension
    /// fields preserved in [`MosaicJson::extra`].
    pub fn document(&self) -> &MosaicJson {
        &self.doc
    }
}

fn prefix_key_range(keys: &[String], prefix: &str) -> std::ops::Range<usize> {
    let start = keys.partition_point(|key| key.as_str() < prefix);
    let mut upper = prefix.to_owned();
    upper.push('4');
    let end = keys.partition_point(|key| key < &upper);
    start..end
}

fn validate_document(doc: &mut MosaicJson) -> AgResult<()> {
    if doc.mosaicjson != "0.0.3" {
        return Err(AgError::Mosaic(format!(
            "unsupported MosaicJSON version {}; expected 0.0.3",
            doc.mosaicjson
        )));
    }
    if doc.minzoom > 30 || doc.maxzoom > 30 {
        return Err(AgError::Mosaic(
            "minzoom/maxzoom must be between 0 and 30".into(),
        ));
    }
    if doc.maxzoom < doc.minzoom {
        return Err(AgError::Mosaic("maxzoom must be >= minzoom".into()));
    }
    let quadkey_zoom = doc.quadkey_zoom.unwrap_or(doc.minzoom);
    if quadkey_zoom > 30 || quadkey_zoom > doc.maxzoom {
        return Err(AgError::Mosaic(
            "quadkey_zoom must be between 0 and maxzoom".into(),
        ));
    }
    if doc.tiles.is_empty() {
        return Err(AgError::Mosaic("MosaicJSON has no tiles".into()));
    }
    for key in doc.tiles.keys() {
        if key.len() != usize::from(quadkey_zoom)
            || key.len() > 30
            || !key.bytes().all(|b| matches!(b, b'0'..=b'3'))
        {
            return Err(AgError::Mosaic(format!(
                "invalid quadkey {key:?}; expected length {quadkey_zoom}"
            )));
        }
    }
    if doc.bounds[0] > doc.bounds[2] || doc.bounds[1] > doc.bounds[3] {
        return Err(AgError::Mosaic(
            "bounds must be [left, bottom, right, top]".into(),
        ));
    }
    if let Some(center) = doc.center {
        let [lon, lat, zoom] = center;
        if lon < doc.bounds[0]
            || lon > doc.bounds[2]
            || lat < doc.bounds[1]
            || lat > doc.bounds[3]
            || zoom < f64::from(doc.minzoom)
            || zoom > f64::from(doc.maxzoom)
        {
            doc.center = None;
        }
    }
    if doc
        .data_type
        .as_deref()
        .and_then(MosaicDataType::parse)
        .is_none()
    {
        doc.data_type = None;
    }
    Ok(())
}

#[async_trait::async_trait]
impl MosaicSource for MosaicJsonSource {
    async fn assets_for_tile(&self, coord: TileCoord) -> AgResult<Vec<AssetRef>> {
        if coord.tms != TileMatrixSet::WebMercatorQuad {
            return Err(AgError::query(
                "tms",
                "MosaicJSON quadkey indexes only support WebMercatorQuad",
            ));
        }

        let mut urls: Vec<&str> = Vec::new();
        match lookup(coord.z, coord.x, coord.y, self.quadkey_zoom) {
            QkLookup::Exact(key) => {
                if let Some(v) = self.doc.tiles.get(&key) {
                    urls.extend(v.iter().map(String::as_str));
                }
            }
            QkLookup::Prefix(prefix) => {
                for key in &self.tile_keys[prefix_key_range(&self.tile_keys, &prefix)] {
                    if let Some(v) = self.doc.tiles.get(key) {
                        urls.extend(v.iter().map(String::as_str));
                    }
                }
            }
        }
        // Asset lists carry paint priority. Prefix queries traverse sorted keys,
        // but preserve the first occurrence of each asset within those lists.
        let mut seen = HashSet::new();
        urls.retain(|url| seen.insert(*url));
        Ok(urls
            .into_iter()
            .map(|url| {
                let url = match &self.doc.asset_prefix {
                    Some(prefix) => format!("{prefix}{url}"),
                    None => url.to_owned(),
                };
                AssetRef {
                    source_id: url.clone(),
                    url,
                    bbox: self.bbox,
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{
      "mosaicjson": "0.0.3",
      "name": "fixture",
      "version": "1.2.3",
      "attribution": "test",
      "minzoom": 7,
      "maxzoom": 12,
      "quadkey_zoom": 7,
      "bounds": [0.0, 0.0, 10.0, 10.0],
      "center": [5.0, 5.0, 8],
      "asset_type": "COG",
      "asset_prefix": "https://x/",
      "data_type": "uint16",
      "colormap": {"0": [0, 0, 0, 0]},
      "tilematrixset": {"id": "WebMercatorQuad"},
      "layers": {"rgb": {"bidx": ["b1", "b2", "b3"]}},
      "vendor:extension": {"ok": true},
      "tiles": {
        "1230123": ["a.tif", "b.tif"],
        "1230120": ["c.tif"]
      }
    }"#;

    fn coord_for_quadkey(quadkey: &str) -> TileCoord {
        let (mut x, mut y) = (0u32, 0u32);
        for ch in quadkey.chars() {
            x <<= 1;
            y <<= 1;
            match ch {
                '0' => {}
                '1' => x |= 1,
                '2' => y |= 1,
                '3' => {
                    x |= 1;
                    y |= 1;
                }
                _ => unreachable!(),
            }
        }
        TileCoord {
            tms: TileMatrixSet::WebMercatorQuad,
            z: quadkey.len() as u8,
            x,
            y,
        }
    }

    #[test]
    fn prefix_key_range_limits_candidates() {
        let keys = ["0000000", "1230120", "1230123", "2000000"]
            .map(str::to_owned)
            .to_vec();

        let matching = prefix_key_range(&keys, "123012");
        assert_eq!(&keys[matching], &["1230120", "1230123"]);

        let all = prefix_key_range(&keys, "");
        assert_eq!(all, 0..keys.len());
    }

    #[tokio::test]
    async fn lower_zoom_unions_matching_prefix_assets() {
        let source = MosaicJsonSource::from_str(DOC).unwrap();
        let got = source
            .assets_for_tile(coord_for_quadkey("123012"))
            .await
            .unwrap();
        let urls: Vec<_> = got.iter().map(|asset| asset.url.as_str()).collect();

        assert_eq!(
            urls,
            vec!["https://x/c.tif", "https://x/a.tif", "https://x/b.tif"]
        );
    }

    #[tokio::test]
    async fn zoom_zero_empty_prefix_returns_all_assets() {
        let source = MosaicJsonSource::from_str(DOC).unwrap();
        let got = source.assets_for_tile(coord_for_quadkey("")).await.unwrap();
        let urls: Vec<_> = got.iter().map(|asset| asset.url.as_str()).collect();

        assert_eq!(
            urls,
            vec!["https://x/c.tif", "https://x/a.tif", "https://x/b.tif"]
        );
    }

    #[tokio::test]
    async fn exact_zoom_returns_listed_assets() {
        let s = MosaicJsonSource::from_str(DOC).unwrap();
        let got = s
            .assets_for_tile(coord_for_quadkey("1230123"))
            .await
            .unwrap();
        let urls: Vec<_> = got.iter().map(|a| a.url.as_str()).collect();
        assert_eq!(urls, vec!["https://x/a.tif", "https://x/b.tif"]);
    }

    #[test]
    fn preserves_mosaicjson_003_metadata_and_unknown_fields() {
        let s = MosaicJsonSource::from_str(DOC).unwrap();
        let doc = s.document();
        assert_eq!(doc.name.as_deref(), Some("fixture"));
        assert_eq!(doc.version, "1.2.3");
        assert_eq!(doc.attribution.as_deref(), Some("test"));
        assert_eq!(doc.center, Some([5.0, 5.0, 8.0]));
        assert_eq!(doc.asset_type.as_deref(), Some("COG"));
        assert_eq!(doc.asset_prefix.as_deref(), Some("https://x/"));
        assert_eq!(doc.data_type.as_deref(), Some("uint16"));
        assert_eq!(doc.colormap["0"], [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(doc.tilematrixset.as_ref().unwrap()["id"], "WebMercatorQuad");
        assert!(doc.layers.contains_key("rgb"));
        assert!(doc.extra.contains_key("vendor:extension"));
    }

    #[test]
    fn empty_tiles_is_error() {
        let bad = r#"{"mosaicjson":"0.0.3","minzoom":1,"maxzoom":2,"bounds":[-180,-90,180,90],"tiles":{}}"#;
        assert!(MosaicJsonSource::from_str(bad).is_err());
    }

    #[tokio::test]
    async fn rejects_non_webmercator_tile_matrix_set() {
        let s = MosaicJsonSource::from_str(DOC).unwrap();
        let err = s
            .assets_for_tile(TileCoord {
                tms: TileMatrixSet::WorldCrs84Quad,
                z: 7,
                x: 0,
                y: 0,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, AgError::Query { name, .. } if name == "tms"));
    }

    #[test]
    fn invalid_optional_data_type_is_ignored() {
        let doc = r#"{
          "mosaicjson": "0.0.3",
          "minzoom": 1,
          "maxzoom": 2,
          "quadkey_zoom": 1,
          "bounds": [-180, -90, 180, 90],
          "data_type": "bogus",
          "tiles": {"0": ["a.tif"]}
        }"#;
        let s = MosaicJsonSource::from_str(doc).unwrap();
        assert!(s.document().data_type.is_none());
    }

    #[test]
    fn invalid_quadkey_zoom_or_key_is_error() {
        let bad_zoom = r#"{
          "mosaicjson": "0.0.3",
          "minzoom": 1,
          "maxzoom": 2,
          "quadkey_zoom": 3,
          "bounds": [-180, -90, 180, 90],
          "tiles": {"000": ["a.tif"]}
        }"#;
        assert!(MosaicJsonSource::from_str(bad_zoom).is_err());

        let bad_key = r#"{
          "mosaicjson": "0.0.3",
          "minzoom": 1,
          "maxzoom": 2,
          "quadkey_zoom": 1,
          "bounds": [-180, -90, 180, 90],
          "tiles": {"04": ["a.tif"]}
        }"#;
        assert!(MosaicJsonSource::from_str(bad_key).is_err());
    }
}
