//! `CogReader` — async-tiff wrapper with GeoTransform/CRS derivation.

use std::collections::HashMap;
use std::sync::Arc;

use std::time::Duration;

use async_tiff::metadata::{cache::ReadaheadMetadataCache, TiffMetadataReader};
use async_tiff::reader::ObjectReader;
use async_tiff::tags::PhotometricInterpretation;
use async_tiff::{ImageFileDirectory, TIFF};
use object_store::http::HttpBuilder;
use object_store::path::Path;
use object_store::{ClientOptions, ObjectStore};

use crate::bounds::{Bbox, GeoTransform};
use crate::crs::Crs;
use crate::error::{AgError, AgResult};
use crate::io::window::{
    stitch_mask_with_info, stitch_with_info, tile_range_for_window, IfdInfo,
};
use crate::pixel::ImageData;

pub struct CogReader {
    pub(crate) tiff: TIFF,
    pub(crate) reader: ObjectReader,
    pub(crate) src_crs: Crs,
    pub(crate) transform: GeoTransform,
    data_ifds: Vec<usize>,
    mask_ifds: Vec<Option<usize>>,
    /// Per data-level, the band index (0-based) whose `ExtraSamples` is
    /// Associated/UnassociatedAlpha. Mirrors rio-tiler's
    /// "alpha band > nodata > none" mask priority.
    alpha_bands: Vec<Option<usize>>,
}

impl CogReader {
    /// Open a remote COG over HTTPS (or any URL `object_store::http` supports).
    ///
    /// Uses tuned `ClientOptions` for tile-server workloads: large per-host
    /// idle pool so concurrent tile requests reuse connections instead of
    /// paying TLS handshake per fetch, and a 30s timeout.
    pub async fn open_http(url: &str) -> AgResult<Self> {
        Self::open_http_with_options(url, default_client_options()).await
    }

    /// Like [`open_http`] but lets the caller override [`ClientOptions`]
    /// (e.g. timeouts, proxy, custom headers).
    pub async fn open_http_with_options(url: &str, options: ClientOptions) -> AgResult<Self> {
        let (base, path) = split_http_url(url)?;
        let store: Arc<dyn ObjectStore> = Arc::new(
            HttpBuilder::new()
                .with_url(base)
                .with_client_options(options)
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
        let alpha_bands: Vec<Option<usize>> = data_ifds
            .iter()
            .map(|&idx| {
                tiff.ifds()
                    .get(idx)
                    .and_then(|ifd| alpha_band_index(ifd))
            })
            .collect();
        Ok(Self {
            tiff,
            reader,
            src_crs,
            transform,
            data_ifds,
            mask_ifds,
            alpha_bands,
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

    /// Pick the finest overview whose ground-sample distance is still ≥ the
    /// target (or the coarsest available if every overview is finer than
    /// target). Matches rio-tiler / gdalwarp behaviour — previously we
    /// returned the coarsest level ≤ target, which under-resolved every
    /// tile by one overview level.
    pub fn pick_overview(&self, target_gsd: f64) -> usize {
        let levels: Vec<(usize, f64)> = (0..self.level_count())
            .filter_map(|l| self.level_gsd(l).ok().map(|g| (l, g)))
            .collect();
        if levels.is_empty() {
            return 0;
        }
        // Find finest (smallest gsd) level whose gsd >= target.
        let mut best: Option<(usize, f64)> = None;
        for &(l, gsd) in &levels {
            if gsd >= target_gsd && best.map_or(true, |(_, b)| gsd < b) {
                best = Some((l, gsd));
            }
        }
        if let Some((l, _)) = best {
            return l;
        }
        // All overviews are finer than target → coarsest available.
        levels
            .iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|&(l, _)| l)
            .unwrap_or(0)
    }

    /// Read a pixel window from the chosen overview level.
    /// Returns ImageData (planar (C,H,W) layout from async-tiff).
    ///
    /// The whole CPU chain `decode → stitch (+ optional mask decode/stitch)`
    /// runs inside one `spawn_blocking` job. Previously `decode_tiles` was
    /// blocking but `stitch` (which after the dtype refactor does the bulk
    /// memcpy + planar-axis view assembly) ran back on the tokio worker —
    /// blocking the reactor for any window large enough to matter.
    pub async fn read_window(
        &self,
        level: usize,
        col_off: i64,
        row_off: i64,
        width: u64,
        height: u64,
    ) -> AgResult<ImageData> {
        let ifd = self.data_ifd(level)?;
        let ((tx0, tx1), (ty0, ty1)) = tile_range_for_window(ifd, col_off, row_off, width, height)
            .ok_or_else(|| AgError::Tiff("ifd lacks tile dims".into()))?;
        let data_info = IfdInfo::from(ifd);

        let coords: Vec<(usize, usize)> = (tx0..=tx1)
            .flat_map(|x| (ty0..=ty1).map(move |y| (x as usize, y as usize)))
            .collect();

        // I/O: ranged HTTP fetch (object_store handles coalescing). Stays async.
        let data_tiles = ifd
            .fetch_tiles(&coords, &self.reader)
            .await
            .map_err(|e| AgError::Tiff(format!("fetch_tiles: {e}")))?;
        let (mask_tiles_opt, mask_info_opt) = if let Some(mask_ifd) = self.mask_ifd(level)? {
            let tiles = mask_ifd
                .fetch_tiles(&coords, &self.reader)
                .await
                .map_err(|e| AgError::Tiff(format!("fetch mask tiles: {e}")))?;
            (Some(tiles), Some(IfdInfo::from(mask_ifd)))
        } else {
            (None, None)
        };

        // CPU: decode + stitch in one blocking job — never touches reactor.
        let coords_for_blocking = coords.clone();
        let (stitched, mask) = tokio::task::spawn_blocking(
            move || -> AgResult<(crate::pixel::PixelData, Option<ndarray::Array2<bool>>)> {
                let registry = async_tiff::decoder::DecoderRegistry::default();
                let data_arrays: Vec<async_tiff::Array> = data_tiles
                    .into_iter()
                    .map(|t| t.decode(&registry))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| AgError::Tiff(format!("decode: {e}")))?;
                let stitched = stitch_with_info(
                    &data_arrays,
                    &coords_for_blocking,
                    &data_info,
                    col_off,
                    row_off,
                    width,
                    height,
                )?;
                let mask = match (mask_tiles_opt, mask_info_opt) {
                    (Some(tiles), Some(info)) => {
                        let mask_arrays: Vec<async_tiff::Array> = tiles
                            .into_iter()
                            .map(|t| t.decode(&registry))
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(|e| AgError::Tiff(format!("decode mask: {e}")))?;
                        Some(stitch_mask_with_info(
                            &mask_arrays,
                            &coords_for_blocking,
                            &info,
                            col_off,
                            row_off,
                            width,
                            height,
                        )?)
                    }
                    _ => None,
                };
                Ok((stitched, mask))
            },
        )
        .await
        .map_err(|e| AgError::Tiff(format!("join: {e}")))??;

        // Alpha-band → mask promotion (rio-tiler priority: alpha > nodata).
        // Only applies when no separate mask IFD was present and the data
        // IFD's ExtraSamples tags one band as alpha. `split_off_band` only
        // runs when the index is in range, so the consume-on-call is safe.
        let alpha_idx = self.alpha_bands.get(level).copied().flatten();
        let (stitched, mask) = match (mask, alpha_idx) {
            (None, Some(b)) if b < stitched.bands() => {
                if let Some((rest, alpha)) = stitched.split_off_band(b) {
                    (rest, Some(alpha))
                } else {
                    // Unreachable given the guard above, but keep the path total.
                    return Err(AgError::Tiff("alpha-band split failed".into()));
                }
            }
            (m, _) => (stitched, m),
        };

        let level_transform = self.level_transform(level)?;
        let window_transform = level_transform.translated(col_off as f64, row_off as f64);
        let bbox_world = transform_bbox(level_transform, col_off, row_off, width, height);
        let nodata = self
            .data_ifd(level)?
            .gdal_nodata()
            .and_then(|s| s.parse::<f64>().ok());

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

}

/// World-space AABB of a pixel rectangle under any (possibly rotated/sheared)
/// GeoTransform. Previously only sampled two opposite corners, which produced
/// the wrong bbox for rotated COGs (where the world-space pixel grid is not
/// axis-aligned).
fn transform_bbox(transform: GeoTransform, col: i64, row: i64, w: u64, h: u64) -> Bbox {
    let c0 = col as f64;
    let r0 = row as f64;
    let c1 = (col + w as i64) as f64;
    let r1 = (row + h as i64) as f64;
    let corners = [
        transform.forward(c0, r0),
        transform.forward(c1, r0),
        transform.forward(c1, r1),
        transform.forward(c0, r1),
    ];
    Bbox::from_corners(&corners)
}

/// Defaults tuned for COG tile servers: keep many idle keep-alive sockets per
/// origin so back-to-back tile requests skip the TLS handshake. Mirrors the
/// `obstore` connection-pool tuning recommended for async-geotiff.
fn default_client_options() -> ClientOptions {
    ClientOptions::new()
        .with_pool_max_idle_per_host(64)
        .with_pool_idle_timeout(Duration::from_secs(90))
        .with_timeout(Duration::from_secs(30))
        .with_connect_timeout(Duration::from_secs(10))
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

/// 0-based index of the band tagged as Associated/UnassociatedAlpha via
/// `ExtraSamples`. The alpha band is positioned after the
/// PhotometricInterpretation-defined bands (3 for RGB, 1 for greyscale),
/// so the alpha index is `(bands - extra_samples.len()) + offset_within_extra`.
/// Returns None when there are no extra samples or none is tagged alpha.
fn alpha_band_index(ifd: &ImageFileDirectory) -> Option<usize> {
    use async_tiff::tags::ExtraSamples;
    let extras = ifd.extra_samples()?;
    let total_bands = ifd.samples_per_pixel() as usize;
    let base = total_bands.checked_sub(extras.len())?;
    extras.iter().enumerate().find_map(|(i, e)| {
        matches!(e, ExtraSamples::AssociatedAlpha | ExtraSamples::UnassociatedAlpha)
            .then_some(base + i)
    })
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
