//! `CogReader` — async-tiff wrapper with GeoTransform/CRS derivation.

use std::collections::HashMap;
use std::sync::Arc;

use async_tiff::metadata::{cache::ReadaheadMetadataCache, TiffMetadataReader};
use async_tiff::reader::ObjectReader;
use async_tiff::tags::PhotometricInterpretation;
use async_tiff::{ImageFileDirectory, TIFF};
use object_store::http::HttpBuilder;
use object_store::path::Path;
use object_store::ObjectStore;

use crate::bounds::{Bbox, GeoTransform};
use crate::crs::Crs;
use crate::error::{AgError, AgResult};
use crate::io::window::{stitch, stitch_mask, tile_range_for_window};
use crate::pixel::ImageData;

pub struct CogReader {
    pub(crate) tiff: TIFF,
    pub(crate) reader: ObjectReader,
    pub(crate) src_crs: Crs,
    pub(crate) transform: GeoTransform,
    data_ifds: Vec<usize>,
    mask_ifds: Vec<Option<usize>>,
    tile_cache: tokio::sync::RwLock<HashMap<TileCacheKey, Arc<async_tiff::Array>>>,
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
        let (data_ifds, mask_ifds) = classify_ifds(&tiff)?;
        Ok(Self {
            tiff,
            reader,
            src_crs,
            transform,
            data_ifds,
            mask_ifds,
            tile_cache: tokio::sync::RwLock::new(HashMap::new()),
        })
    }

    pub fn src_crs(&self) -> &Crs {
        &self.src_crs
    }

    pub fn transform(&self) -> GeoTransform {
        self.transform
    }

    pub fn overview_count(&self) -> usize {
        self.data_ifds.len().saturating_sub(1)
    }

    pub fn level_count(&self) -> usize {
        self.data_ifds.len()
    }

    pub fn level_dimensions(&self, level: usize) -> AgResult<(u32, u32)> {
        let ifd = self.data_ifd(level)?;
        Ok((ifd.image_width(), ifd.image_height()))
    }

    pub fn level_transform(&self, level: usize) -> AgResult<GeoTransform> {
        let (full_w, full_h) = self.level_dimensions(0)?;
        let (level_w, level_h) = self.level_dimensions(level)?;
        let scale_x = full_w as f64 / level_w as f64;
        let scale_y = full_h as f64 / level_h as f64;
        Ok(self.transform.scaled(scale_x, scale_y))
    }

    pub fn level_gsd(&self, level: usize) -> AgResult<f64> {
        let tr = self.level_transform(level)?.0;
        let x_res = (tr.a.powi(2) + tr.d.powi(2)).sqrt();
        let y_res = (tr.b.powi(2) + tr.e.powi(2)).sqrt();
        Ok(x_res.max(y_res))
    }

    pub fn pick_overview(&self, target_gsd: f64) -> usize {
        let mut best = 0usize;
        let mut best_gsd = self.level_gsd(0).unwrap_or(0.0);
        for level in 0..self.level_count() {
            let Ok(gsd) = self.level_gsd(level) else {
                continue;
            };
            if gsd <= target_gsd && gsd >= best_gsd {
                best = level;
                best_gsd = gsd;
            }
        }
        best
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
        let ifd_idx = self.data_ifd_index(level)?;
        let ifd = self.data_ifd(level)?;
        let ((tx0, tx1), (ty0, ty1)) = tile_range_for_window(ifd, col_off, row_off, width, height)
            .ok_or_else(|| AgError::Tiff("ifd lacks tile dims".into()))?;

        let coords: Vec<(usize, usize)> = (tx0..=tx1)
            .flat_map(|x| (ty0..=ty1).map(move |y| (x as usize, y as usize)))
            .collect();
        let arrays = self
            .fetch_decode_tiles_cached(ifd_idx, ifd, &coords)
            .await?;

        let mask = if let Some(mask_ifd) = self.mask_ifd(level)? {
            let mask_tiles = mask_ifd
                .fetch_tiles(&coords, &self.reader)
                .await
                .map_err(|e| AgError::Tiff(format!("fetch mask tiles: {e}")))?;
            let mask_arrays = decode_tiles(mask_tiles).await?;
            Some(stitch_mask(
                &mask_arrays,
                &coords,
                mask_ifd,
                col_off,
                row_off,
                width,
                height,
            )?)
        } else {
            None
        };

        let stitched = stitch(&arrays, &coords, ifd, col_off, row_off, width, height)?;
        let level_transform = self.level_transform(level)?;
        let window_transform = level_transform.translated(col_off as f64, row_off as f64);
        let bbox_world = transform_bbox(level_transform, col_off, row_off, width, height);
        let nodata = ifd.gdal_nodata().and_then(|s| s.parse::<f64>().ok());

        Ok(ImageData {
            pixels: stitched,
            mask,
            bbox: bbox_world,
            crs: self.src_crs.clone(),
            transform: window_transform,
            nodata,
        })
    }

    pub(crate) fn data_ifd(&self, level: usize) -> AgResult<&ImageFileDirectory> {
        let idx = self.data_ifd_index(level)?;
        self.tiff
            .ifds()
            .get(idx)
            .ok_or(AgError::MissingGeo("overview IFD"))
    }

    fn data_ifd_index(&self, level: usize) -> AgResult<usize> {
        self.data_ifds
            .get(level)
            .copied()
            .ok_or(AgError::MissingGeo("overview level"))
    }

    fn mask_ifd(&self, level: usize) -> AgResult<Option<&ImageFileDirectory>> {
        let Some(mask_idx) = self
            .mask_ifds
            .get(level)
            .ok_or(AgError::MissingGeo("overview level"))?
        else {
            return Ok(None);
        };
        Ok(self.tiff.ifds().get(*mask_idx))
    }

    async fn fetch_decode_tiles_cached(
        &self,
        ifd_idx: usize,
        ifd: &ImageFileDirectory,
        coords: &[(usize, usize)],
    ) -> AgResult<Vec<Arc<async_tiff::Array>>> {
        const MAX_TILE_CACHE_ITEMS: usize = 128;

        let mut arrays: Vec<Option<Arc<async_tiff::Array>>> = vec![None; coords.len()];
        let mut missing = Vec::new();
        {
            let cache = self.tile_cache.read().await;
            for (pos, &(x, y)) in coords.iter().enumerate() {
                let key = TileCacheKey { ifd_idx, x, y };
                if let Some(array) = cache.get(&key) {
                    arrays[pos] = Some(array.clone());
                } else {
                    missing.push((pos, x, y));
                }
            }
        }

        if !missing.is_empty() {
            let missing_coords: Vec<(usize, usize)> =
                missing.iter().map(|(_, x, y)| (*x, *y)).collect();
            let tiles = ifd
                .fetch_tiles(&missing_coords, &self.reader)
                .await
                .map_err(|e| AgError::Tiff(format!("fetch_tiles: {e}")))?;
            let decoded = decode_tiles(tiles).await?;

            let mut cache = self.tile_cache.write().await;
            if cache.len() + decoded.len() > MAX_TILE_CACHE_ITEMS {
                cache.clear();
            }
            for ((pos, x, y), array) in missing.into_iter().zip(decoded.into_iter()) {
                let array = Arc::new(array);
                cache.insert(TileCacheKey { ifd_idx, x, y }, array.clone());
                arrays[pos] = Some(array);
            }
        }

        arrays
            .into_iter()
            .map(|array| array.ok_or_else(|| AgError::Tiff("tile cache miss".into())))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TileCacheKey {
    ifd_idx: usize,
    x: usize,
    y: usize,
}

async fn decode_tiles(tiles: Vec<async_tiff::Tile>) -> AgResult<Vec<async_tiff::Array>> {
    tokio::task::spawn_blocking(move || {
        let registry = async_tiff::decoder::DecoderRegistry::default();
        tiles
            .into_iter()
            .map(|t| t.decode(&registry))
            .collect::<Result<Vec<_>, _>>()
    })
    .await
    .map_err(|e| AgError::Tiff(format!("join: {e}")))?
    .map_err(|e| AgError::Tiff(format!("decode: {e}")))
}

fn transform_bbox(transform: GeoTransform, col: i64, row: i64, w: u64, h: u64) -> Bbox {
    let (x0, y0) = transform.forward(col as f64, row as f64);
    let (x1, y1) = transform.forward((col + w as i64) as f64, (row + h as i64) as f64);
    Bbox::new(x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
}

fn split_http_url(url: &str) -> AgResult<(String, String)> {
    let u = url::Url::parse(url).map_err(|e| AgError::Tiff(format!("invalid url: {e}")))?;
    let scheme = u.scheme();
    let host = u
        .host_str()
        .ok_or_else(|| AgError::Tiff("url missing host".into()))?;
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
        .unwrap_or_else(|| Crs::new("UNKNOWN"));

    Ok((gt, crs))
}

fn classify_ifds(tiff: &TIFF) -> AgResult<(Vec<usize>, Vec<Option<usize>>)> {
    let first = tiff.ifds().first().ok_or(AgError::MissingGeo("no IFD"))?;
    let mut mask_by_dims: HashMap<(u32, u32), usize> = HashMap::new();
    let mut overviews: Vec<((u32, u32), usize)> = Vec::new();
    for (idx, ifd) in tiff.ifds().iter().enumerate().skip(1) {
        let dims = (ifd.image_width(), ifd.image_height());
        if is_mask_ifd(ifd) {
            mask_by_dims.insert(dims, idx);
        } else {
            overviews.push((dims, idx));
        }
    }

    overviews.sort_by_key(|((w, h), _)| std::cmp::Reverse(u64::from(*w) * u64::from(*h)));

    let mut data_ifds = vec![0usize];
    let mut mask_ifds = vec![mask_by_dims
        .get(&(first.image_width(), first.image_height()))
        .copied()];
    for (dims, idx) in overviews {
        data_ifds.push(idx);
        mask_ifds.push(mask_by_dims.get(&dims).copied());
    }
    Ok((data_ifds, mask_ifds))
}

fn is_mask_ifd(ifd: &ImageFileDirectory) -> bool {
    ifd.new_subfile_type().is_some_and(|t| t & 4 != 0)
        && ifd.photometric_interpretation() == PhotometricInterpretation::TransparencyMask
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
