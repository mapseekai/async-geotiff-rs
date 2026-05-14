//! `CogReader` — async-tiff wrapper with GeoTransform/CRS derivation.

use std::sync::Arc;

use async_tiff::metadata::{cache::ReadaheadMetadataCache, TiffMetadataReader};
use async_tiff::reader::ObjectReader;
use async_tiff::{ImageFileDirectory, TIFF};
use object_store::http::HttpBuilder;
use object_store::path::Path;
use object_store::ObjectStore;

use crate::bounds::{Bbox, GeoTransform};
use crate::crs::Crs;
use crate::error::{AgError, AgResult};
use crate::io::window::{stitch, tile_range_for_window};
use crate::pixel::ImageData;

pub struct CogReader {
    pub(crate) tiff: TIFF,
    pub(crate) reader: ObjectReader,
    pub(crate) src_crs: Crs,
    pub(crate) transform: GeoTransform,
}

impl CogReader {
    /// Open a remote COG over HTTPS (or any URL `object_store::http` supports).
    pub async fn open_http(url: &str) -> AgResult<Self> {
        let (base, path) = split_http_url(url)?;
        let store: Arc<dyn ObjectStore> = Arc::new(
            HttpBuilder::new()
                .with_url(base)
                .build()
                .map_err(|e| AgError::Tiff(format!("http store: {e}")))?,
        );
        let reader = ObjectReader::new(store, Path::from(path));
        Self::open_with_reader(reader).await
    }

    /// Open from an existing `ObjectReader` (lets callers wire any object_store).
    pub async fn open_with_reader(reader: ObjectReader) -> AgResult<Self> {
        let cache = ReadaheadMetadataCache::new(reader.clone());
        let mut md = TiffMetadataReader::try_open(&cache)
            .await
            .map_err(|e| AgError::Tiff(format!("metadata open: {e}")))?;
        let ifds = md
            .read_all_ifds(&cache)
            .await
            .map_err(|e| AgError::Tiff(format!("read ifds: {e}")))?;
        let tiff = TIFF::new(ifds, md.endianness());

        let (transform, src_crs) = derive_geo(&tiff)?;
        Ok(Self {
            tiff,
            reader,
            src_crs,
            transform,
        })
    }

    pub fn src_crs(&self) -> &Crs {
        &self.src_crs
    }

    pub fn transform(&self) -> GeoTransform {
        self.transform
    }

    pub fn overview_count(&self) -> usize {
        self.tiff.ifds().len()
    }

    /// Read a pixel window from the chosen overview level.
    /// Returns ImageData (planar (C,H,W) layout from async-tiff).
    pub async fn read_window(
        &self,
        level: usize,
        col_off: i64,
        row_off: i64,
        width: u64,
        height: u64,
    ) -> AgResult<ImageData> {
        use async_tiff::decoder::DecoderRegistry;

        let ifd = self
            .tiff
            .ifds()
            .get(level)
            .ok_or(AgError::MissingGeo("overview level"))?;
        let ((tx0, tx1), (ty0, ty1)) =
            tile_range_for_window(ifd, col_off, row_off, width, height)
                .ok_or_else(|| AgError::Tiff("ifd lacks tile dims".into()))?;

        let coords: Vec<(usize, usize)> = (tx0..=tx1)
            .flat_map(|x| (ty0..=ty1).map(move |y| (x as usize, y as usize)))
            .collect();
        let tiles = ifd
            .fetch_tiles(&coords, &self.reader)
            .await
            .map_err(|e| AgError::Tiff(format!("fetch_tiles: {e}")))?;

        // Decode each tile on a blocking pool (async-tiff decode is CPU-bound & sync).
        let arrays: Vec<async_tiff::Array> = tokio::task::spawn_blocking(move || {
            let registry = DecoderRegistry::default();
            tiles
                .into_iter()
                .map(|t| t.decode(&registry))
                .collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(|e| AgError::Tiff(format!("join: {e}")))?
        .map_err(|e| AgError::Tiff(format!("decode: {e}")))?;

        let stitched = stitch(&arrays, &coords, ifd, col_off, row_off, width, height)?;
        let bbox_world = self.transform_bbox(col_off, row_off, width, height);
        let nodata = ifd.gdal_nodata().and_then(|s| s.parse::<f64>().ok());

        Ok(ImageData {
            pixels: stitched,
            mask: None,
            bbox: bbox_world,
            crs: self.src_crs.clone(),
            transform: self.transform,
            nodata,
        })
    }

    fn transform_bbox(&self, col: i64, row: i64, w: u64, h: u64) -> Bbox {
        let (x0, y0) = self.transform.forward(col as f64, row as f64);
        let (x1, y1) = self
            .transform
            .forward((col + w as i64) as f64, (row + h as i64) as f64);
        Bbox::new(x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
    }
}

fn split_http_url(url: &str) -> AgResult<(String, String)> {
    let u = url::Url::parse(url).map_err(|e| AgError::Tiff(format!("invalid url: {e}")))?;
    let scheme = u.scheme();
    let host = u.host_str().ok_or_else(|| AgError::Tiff("url missing host".into()))?;
    let port = u.port().map(|p| format!(":{}", p)).unwrap_or_default();
    let base = format!("{}://{}{}", scheme, host, port);
    let path = u.path().trim_start_matches('/').to_string();
    Ok((base, path))
}

fn derive_geo(tiff: &TIFF) -> AgResult<(GeoTransform, Crs)> {
    let ifd = tiff.ifds().first().ok_or(AgError::MissingGeo("no IFD"))?;

    let gt = if let Some(t) = ifd.model_transformation() {
        // 4x4 row-major; for a north-up image use indices [3, 0, 1, 7, 4, 5].
        if t.len() < 16 {
            return Err(AgError::MissingGeo("model_transformation truncated"));
        }
        GeoTransform::from_gdal(&[t[3], t[0], t[1], t[7], t[4], t[5]])
    } else {
        let tp = ifd
            .model_tiepoint()
            .ok_or(AgError::MissingGeo("model_tiepoint"))?;
        let ps = ifd
            .model_pixel_scale()
            .ok_or(AgError::MissingGeo("model_pixel_scale"))?;
        if tp.len() < 6 || ps.len() < 2 {
            return Err(AgError::MissingGeo("tiepoint/scale truncated"));
        }
        let (i, j, x, y) = (tp[0], tp[1], tp[3], tp[4]);
        let (sx, sy) = (ps[0], ps[1]);
        // World origin = (x - i*sx, y + j*sy) for north-up images.
        GeoTransform::from_gdal(&[x - i * sx, sx, 0.0, y + j * sy, 0.0, -sy])
    };

    let crs = ifd
        .geo_key_directory()
        .and_then(|gk| gk.epsg_code())
        .map(|c| Crs::epsg(c as u32))
        .unwrap_or_else(|| Crs::from_str("UNKNOWN"));

    Ok((gt, crs))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

// `ImageFileDirectory` referenced by `read_window` via async-tiff path; keep import live.
#[allow(dead_code)]
fn _ifd_type_check(ifd: &ImageFileDirectory) -> u16 {
    ifd.samples_per_pixel()
}
