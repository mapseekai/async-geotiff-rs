//! Dataset band statistics: embedded `STATISTICS_*` tags (unverified precision) or a
//! one-shot top-overview computation (estimated). See
//! docs/superpowers/specs/2026-05-16-non-uint8-data-stretch-design.md.

use crate::error::AgResult;
use crate::io::CogReader;
use ndarray::{Array3, Axis};

/// How a statistics set was obtained; embedded or sampled values may be approximate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Provenance {
    /// Computed from every valid sample in the supplied domain.
    Exact,
    /// Read from metadata; may contain approximate or inferred fields.
    Embedded,
    /// Computed from a reduced-resolution overview or a subset of samples.
    Estimated,
}

/// Equal-width histogram over `[min, max]`. `counts.len()` is the bin count.
#[derive(Debug, Clone)]
pub struct Histogram {
    /// Lower edge of the first bin, in raw sample units.
    pub min: f64,
    /// Upper edge of the final bin, in raw sample units.
    pub max: f64,
    /// Counts in ascending equal-width bins. Empty or all-zero counts give
    /// [`Self::min`] when querying a quantile; their sum may exceed `u64::MAX`.
    pub counts: Vec<u64>,
}

impl Histogram {
    /// Value at percentile `p` (0..=100) via linear interpolation on the
    /// cumulative count. Robust to the overview-smoothing approximation
    /// (see spec accuracy table). Empty / degenerate → `min`.
    /// Counts may sum beyond `u64::MAX`; accumulation uses `u128` and only
    /// the interpolation converts to f64. The 100th percentile is the upper
    /// edge of the last nonempty bin, excluding any trailing zero-count bins.
    pub fn quantile(&self, p: f64) -> f64 {
        let total: u128 = self.counts.iter().map(|&count| u128::from(count)).sum();
        if total == 0 || self.max <= self.min || self.counts.is_empty() {
            return self.min;
        }
        let target = (p / 100.0) * total as f64;
        let bin_w = (self.max - self.min) / self.counts.len() as f64;
        if p == 100.0 {
            // Distinct integer totals can round to the same f64 near u64::MAX.
            // Locate the endpoint without losing a small final nonempty bin.
            return self
                .counts
                .iter()
                .rposition(|&count| count != 0)
                .map_or(self.min, |i| self.min + (i + 1) as f64 * bin_w);
        }
        let mut cum = 0u128;
        for (i, &c) in self.counts.iter().enumerate() {
            let next = cum + u128::from(c);
            if (next as f64) >= target {
                // Fraction into this bin.
                let into = if c == 0 {
                    0.0
                } else {
                    (target - cum as f64) / c as f64
                };
                return self.min + (i as f64 + into) * bin_w;
            }
            cum = next;
        }
        self.max
    }
}

/// Summary of one band's valid raw samples, or values inferred from its metadata.
#[derive(Debug, Clone)]
pub struct BandStat {
    /// Minimum sample or metadata minimum; an empty computed band uses `0.0`.
    pub min: f64,
    /// Maximum sample or metadata maximum; an empty computed band uses `1.0`.
    pub max: f64,
    /// Arithmetic mean, or a metadata fallback estimate.
    pub mean: f64,
    /// Population standard deviation, or a metadata fallback estimate.
    pub stddev: f64,
    /// Sample distribution used for approximate percentile interpolation.
    pub hist: Histogram,
}

/// Per-band statistics together with their shared provenance.
#[derive(Debug, Clone)]
pub struct DatasetStats {
    /// Band order of the producing operation: embedded metadata follows native
    /// TIFF sample order; sampled statistics follow the decoded image bands,
    /// which may omit Alpha promoted to a validity mask.
    pub bands: Vec<BandStat>,
    /// Whether the values were computed exactly, read from metadata or estimated.
    pub provenance: Provenance,
}

/// Per-band stats from a planar `(bands, H, W)` f64 array, excluding
/// `nodata` and non-finite samples. `hist_bins` is the histogram resolution,
/// clamped to at least one bin. An empty band has min/max `0.0`/`1.0`, zero
/// mean and deviation, and zero bin counts.
pub fn band_stats_from_f64(
    arr: &Array3<f64>,
    nodata: Option<f64>,
    hist_bins: usize,
) -> Vec<BandStat> {
    let bins = hist_bins.max(1);
    // A sample counts toward stats only if finite and not nodata.
    let valid = |v: f64| {
        if !v.is_finite() {
            return false;
        }
        match nodata {
            Some(nd) => nd.is_nan() || (v - nd).abs() >= 1e-9,
            None => true,
        }
    };
    let mut out = Vec::with_capacity(arr.shape()[0]);
    for b in 0..arr.shape()[0] {
        let band = arr.index_axis(Axis(0), b);
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut mean = 0.0_f64;
        let mut m2 = 0.0_f64;
        let mut count = 0_u64;
        for &v in band.iter() {
            if !valid(v) {
                continue;
            }
            min = min.min(v);
            max = max.max(v);
            count += 1;
            let delta = v - mean;
            mean += delta / count as f64;
            m2 += delta * (v - mean);
        }
        if count == 0 {
            out.push(BandStat {
                min: 0.0,
                max: 1.0,
                mean: 0.0,
                stddev: 0.0,
                hist: Histogram {
                    min: 0.0,
                    max: 1.0,
                    counts: vec![0; bins],
                },
            });
            continue;
        }
        let var = m2 / count as f64;
        let stddev = var.max(0.0).sqrt();
        let mut counts = vec![0u64; bins];
        let span = (max - min).max(1e-12);
        for &v in band.iter() {
            if !valid(v) {
                continue;
            }
            let mut idx = (((v - min) / span) * bins as f64) as usize;
            if idx >= bins {
                idx = bins - 1;
            }
            counts[idx] += 1;
        }
        out.push(BandStat {
            min,
            max,
            mean,
            stddev,
            hist: Histogram { min, max, counts },
        });
    }
    out
}

/// Parse embedded stats from a COG's GDAL_METADATA XML, falling back to
/// TIFF `SMin/SMaxSampleValue`. Returns `None` when neither yields a
/// usable min/max for every band. Histogram bounds come from
/// `STATISTICS_HISTOMIN/HISTOMAX`; `STATISTICS_HISTOBINS` supplies comma- or
/// whitespace-separated counts. Absent or unusable counts produce a single-bin
/// placeholder. `STATISTICS_HISTONUMBINS` is not used; bin count follows the
/// successfully parsed values.
pub fn parse_embedded_stats(
    gdal_metadata: Option<&str>,
    smin: Option<&[u16]>,
    smax: Option<&[u16]>,
    bands: usize,
) -> Option<DatasetStats> {
    fn items(xml: &str) -> Vec<(String, usize, String)> {
        use quick_xml::events::Event;

        let mut reader = quick_xml::Reader::from_str(xml);
        reader.config_mut().trim_text(true);
        let mut out = Vec::new();
        let mut current: Option<(String, usize)> = None;

        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) if e.name().as_ref() == b"Item" => {
                    let mut name = String::new();
                    let mut sample = 0usize;
                    for attr in e.attributes().flatten() {
                        let Ok(value) = attr.decode_and_unescape_value(reader.decoder()) else {
                            continue;
                        };
                        match attr.key.as_ref() {
                            b"name" => name = value.into_owned(),
                            b"sample" => sample = value.parse().unwrap_or(0),
                            _ => {}
                        }
                    }
                    current = (!name.is_empty()).then_some((name, sample));
                }
                Ok(Event::Text(e)) => {
                    if let Some((name, sample)) = current.take() {
                        if let Ok(value) = e.decode() {
                            out.push((name, sample, value.trim().to_string()));
                        }
                    }
                }
                Ok(Event::End(e)) if e.name().as_ref() == b"Item" => {
                    current = None;
                }
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
        }
        out
    }

    let parsed = gdal_metadata.map(items).unwrap_or_default();
    let get = |name: &str, b: usize| -> Option<f64> {
        parsed
            .iter()
            .find(|(n, s, _)| n == name && *s == b)
            .and_then(|(_, _, v)| v.parse::<f64>().ok())
    };

    let mut out = Vec::with_capacity(bands);
    for b in 0..bands {
        let min = get("STATISTICS_MINIMUM", b)
            .or_else(|| smin.and_then(|s| s.get(b)).map(|&x| x as f64))?;
        let max = get("STATISTICS_MAXIMUM", b)
            .or_else(|| smax.and_then(|s| s.get(b)).map(|&x| x as f64))?;
        let mean = get("STATISTICS_MEAN", b).unwrap_or((min + max) / 2.0);
        let stddev = get("STATISTICS_STDDEV", b).unwrap_or((max - min) / 4.0);
        let hist = {
            let hmin = get("STATISTICS_HISTOMIN", b).unwrap_or(min);
            let hmax = get("STATISTICS_HISTOMAX", b).unwrap_or(max);
            let counts: Vec<u64> = parsed
                .iter()
                .find(|(n, s, _)| n == "STATISTICS_HISTOBINS" && *s == b)
                .map(|(_, _, v)| {
                    v.split(|c: char| c == ',' || c.is_whitespace())
                        .filter(|t| !t.is_empty())
                        .filter_map(|t| t.parse::<u64>().ok())
                        .collect()
                })
                .filter(|c: &Vec<u64>| !c.is_empty())
                .unwrap_or_else(|| vec![1]);
            Histogram {
                min: hmin,
                max: hmax,
                counts,
            }
        };
        out.push(BandStat {
            min,
            max,
            mean,
            stddev,
            hist,
        });
    }
    Some(DatasetStats {
        bands: out,
        provenance: Provenance::Embedded,
    })
}

const HIST_BINS: usize = 256;

/// Statistics over the same valid samples carried by an image.
pub(crate) fn band_stats_from_image(
    image: &crate::pixel::ImageData,
    hist_bins: usize,
) -> Vec<BandStat> {
    let mut arr = image.pixels.as_f64();
    if let Some(mask) = &image.mask {
        for mut band in arr.to_mut().axis_iter_mut(Axis(0)) {
            ndarray::Zip::from(&mut band)
                .and(mask)
                .for_each(|value, &valid| {
                    if !valid {
                        *value = f64::NAN;
                    }
                });
        }
    }
    band_stats_from_f64(&arr, image.nodata, hist_bins)
}

/// Read embedded `STATISTICS_*` tags or estimate statistics from the smallest
/// overview. Embedded values retain native TIFF band order; estimates follow
/// [`CogReader::read_window`] output band order and its mask semantics.
///
/// Returns `Ok(None)` when no complete embedded statistics are available and
/// the COG has no overview, or the smallest overview exceeds 16,777,216 pixels.
/// Sampled statistics use the reader's CPU limiter and working-memory budget.
///
/// # Errors
/// Propagates metadata, I/O, decoding, window-limit and resource-limit errors.
/// When called inside a request, its cancellation and deadline also apply.
pub async fn load_dataset_stats(reader: &CogReader) -> AgResult<Option<DatasetStats>> {
    // 1) Embedded metadata path: tags on the full-res IFD. Scope the IFD
    //    borrow so it ends before the later `read_window` call.
    {
        let ifd = reader.data_ifd(0)?;
        let bands = ifd.samples_per_pixel() as usize;
        if let Some(exact) = parse_embedded_stats(
            ifd.gdal_metadata(),
            ifd.min_sample_value(),
            ifd.max_sample_value(),
            bands,
        ) {
            return Ok(Some(exact));
        }
    }

    // 2) Estimated path: read the highest overview in full.
    let top = reader.level_count().saturating_sub(1);
    if top == 0 {
        return Ok(None);
    }
    let (w, h) = reader.level_dimensions(top)?;
    // Estimated path must stay a small read (spec: ≈ top overview only).
    // If the only/highest overview is unexpectedly large, skip rather than
    // spike memory; caller falls back to stretch::fallback_rescale.
    const MAX_OVERVIEW_PIXELS: u64 = 16 * 1024 * 1024;
    if w as u64 * h as u64 > MAX_OVERVIEW_PIXELS {
        return Ok(None);
    }
    let bands = u64::from(reader.data_ifd(top)?.samples_per_pixel());
    let estimate = u64::from(w)
        .checked_mul(u64::from(h))
        .and_then(|n| n.checked_mul(bands.saturating_mul(16).saturating_add(2)))
        .ok_or_else(|| {
            crate::error::AgError::ResourceLimit("statistics work estimate overflow".into())
        })?;
    let work = reader.work_budget().try_reserve(estimate)?;
    let img = reader.read_window(top, 0, 0, w as u64, h as u64).await?;
    let permit = reader.cpu_limiter().acquire().await?;
    let request = crate::request::RequestContext::current();
    let stats = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _work = work;
        crate::request::RequestContext::run_blocking(request, || {
            Ok(band_stats_from_image(&img, HIST_BINS))
        })
    })
    .await
    .map_err(|e| crate::error::AgError::Warp(format!("statistics task: {e}")))??;
    Ok(Some(DatasetStats {
        bands: stats,
        provenance: Provenance::Estimated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantile_linear_interpolates() {
        // 4 bins over [0,4], 10 counts each. p=50 → midpoint = 2.0.
        let h = Histogram {
            min: 0.0,
            max: 4.0,
            counts: vec![10, 10, 10, 10],
        };
        assert!((h.quantile(50.0) - 2.0).abs() < 1e-9);
        assert!((h.quantile(0.0) - 0.0).abs() < 1e-9);
        assert!((h.quantile(100.0) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn quantile_accumulates_public_counts_beyond_u64() {
        let h = Histogram {
            min: 0.0,
            max: 4.0,
            counts: vec![u64::MAX; 4],
        };
        for (percentile, expected) in [
            (0.0, 0.0),
            (25.0, 1.0),
            (50.0, 2.0),
            (75.0, 3.0),
            (100.0, 4.0),
        ] {
            assert!((h.quantile(percentile) - expected).abs() < 1e-9);
        }
    }

    #[test]
    fn quantile_handles_embedded_counts_beyond_u64() {
        let xml = r#"<GDALMetadata>
<Item name="STATISTICS_MINIMUM" sample="0">0</Item>
<Item name="STATISTICS_MAXIMUM" sample="0">1</Item>
<Item name="STATISTICS_HISTOMIN" sample="0">0</Item>
<Item name="STATISTICS_HISTOMAX" sample="0">1</Item>
<Item name="STATISTICS_HISTONUMBINS" sample="0">2</Item>
<Item name="STATISTICS_HISTOBINS" sample="0">18446744073709551615,1</Item>
</GDALMetadata>"#;
        let stats = parse_embedded_stats(Some(xml), None, None, 1).unwrap();
        assert_eq!(stats.bands[0].hist.counts, vec![u64::MAX, 1]);
        assert!((stats.bands[0].hist.quantile(50.0) - 0.25).abs() < 1e-9);
        assert_eq!(stats.bands[0].hist.quantile(100.0), 1.0);
    }

    #[test]
    fn quantile_upper_endpoint_keeps_small_counts_and_excludes_empty_tail() {
        for counts in [
            vec![u64::MAX, 1],
            vec![1, u64::MAX],
            vec![u64::MAX, 1, 0],
            vec![1, u64::MAX, 0, 0],
            vec![1, 1, 0],
        ] {
            let histogram = Histogram {
                min: 0.0,
                max: counts.len() as f64,
                counts,
            };
            assert_eq!(histogram.quantile(100.0), 2.0);
        }
    }

    #[test]
    fn quantile_skewed() {
        // Mass concentrated in first bin: p=2 stays near min.
        let h = Histogram {
            min: 0.0,
            max: 100.0,
            counts: vec![98, 1, 1, 0],
        };
        assert!(h.quantile(2.0) < 25.0);
        assert!(h.quantile(98.0) >= 25.0);
    }

    #[test]
    fn quantile_degenerate_returns_min() {
        let h = Histogram {
            min: 5.0,
            max: 5.0,
            counts: vec![0, 0],
        };
        assert_eq!(h.quantile(50.0), 5.0);
    }

    #[test]
    fn kernel_basic_stats_and_nodata_exclusion() {
        use ndarray::Array3;
        // 1 band, 1x4: values 0(nodata), 2, 4, 6 → over {2,4,6}.
        let arr = Array3::from_shape_vec((1, 1, 4), vec![0.0, 2.0, 4.0, 6.0]).unwrap();
        let s = band_stats_from_f64(&arr, Some(0.0), 8);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].min, 2.0);
        assert_eq!(s[0].max, 6.0);
        assert!((s[0].mean - 4.0).abs() < 1e-9);
        // population stddev of {2,4,6} = sqrt(8/3) ≈ 1.6329931619
        assert!((s[0].stddev - (8.0_f64 / 3.0).sqrt()).abs() < 1e-9);
    }

    #[test]
    fn kernel_all_nodata_is_safe() {
        use ndarray::Array3;
        let arr = Array3::from_shape_vec((1, 1, 2), vec![0.0, 0.0]).unwrap();
        let s = band_stats_from_f64(&arr, Some(0.0), 4);
        assert_eq!(s[0].min, 0.0);
        assert_eq!(s[0].max, 1.0);
    }

    #[test]
    fn nan_nodata_retains_finite_samples() {
        let arr = Array3::from_shape_vec((1, 1, 4), vec![10.0, 20.0, 30.0, f64::NAN]).unwrap();
        let s = band_stats_from_f64(&arr, Some(f64::NAN), 4);
        assert_eq!((s[0].min, s[0].max, s[0].mean), (10.0, 30.0, 20.0));
        assert_eq!(s[0].hist.counts.iter().sum::<u64>(), 3);
    }

    #[test]
    fn image_stats_exclude_masked_outliers() {
        let image = crate::pixel::ImageData {
            pixels: crate::pixel::PixelData::F64(
                Array3::from_shape_vec((1, 1, 3), vec![10.0, 20.0, 9999.0]).unwrap(),
            ),
            mask: Some(ndarray::array![[true, true, false]]),
            bbox: crate::bounds::Bbox::new(0.0, 0.0, 3.0, 1.0),
            crs: crate::crs::Crs::epsg(4326),
            transform: crate::bounds::GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 1.0, 0.0, -1.0]),
            nodata: None,
        };
        let stats = band_stats_from_image(&image, 4);
        assert_eq!(
            (stats[0].min, stats[0].max, stats[0].mean),
            (10.0, 20.0, 15.0)
        );
        assert_eq!(stats[0].hist.counts.iter().sum::<u64>(), 2);
    }

    #[test]
    fn masked_stats_do_not_mutate_borrowed_strided_f64_pixels() {
        let pixels = ndarray::array![[[1.0, 9999.0], [3.0, 5.0]]].permuted_axes([0, 2, 1]);
        let image = crate::pixel::ImageData {
            pixels: crate::pixel::PixelData::F64(pixels),
            mask: Some(ndarray::array![[true, true], [false, true]]),
            bbox: crate::bounds::Bbox::new(0.0, 0.0, 2.0, 2.0),
            crs: crate::crs::Crs::epsg(4326),
            transform: crate::bounds::GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 2.0, 0.0, -1.0]),
            nodata: None,
        };
        let stats = band_stats_from_image(&image, 4);
        assert_eq!((stats[0].min, stats[0].max, stats[0].mean), (1.0, 5.0, 3.0));
        assert_eq!(stats[0].hist.counts.iter().sum::<u64>(), 3);
        assert_eq!(image.pixels.as_f64()[[0, 1, 0]], 9999.0);
    }

    #[test]
    fn variance_retains_small_spread_at_large_offset() {
        let arr = Array3::from_shape_vec((1, 1, 3), vec![1e12, 1e12 + 2.0, 1e12 + 4.0]).unwrap();
        let s = band_stats_from_f64(&arr, None, 4);
        assert!((s[0].stddev - (8.0_f64 / 3.0).sqrt()).abs() < 1e-9);
    }

    #[test]
    fn parses_gdal_metadata_per_band() {
        let xml = r#"<GDALMetadata>
<Item name="STATISTICS_MINIMUM" sample="0">79</Item>
<Item name="STATISTICS_MAXIMUM" sample="0">1896</Item>
<Item name="STATISTICS_MEAN" sample="0">565</Item>
<Item name="STATISTICS_STDDEV" sample="0">45</Item>
<Item name="STATISTICS_MINIMUM" sample="1">74</Item>
<Item name="STATISTICS_MAXIMUM" sample="1">2213</Item>
</GDALMetadata>"#;
        let s = parse_embedded_stats(Some(xml), None, None, 2).unwrap();
        assert_eq!(s.provenance, Provenance::Embedded);
        assert_eq!(s.bands[0].min, 79.0);
        assert_eq!(s.bands[0].max, 1896.0);
        assert_eq!(s.bands[0].mean, 565.0);
        assert_eq!(s.bands[1].min, 74.0);
        // band 1 has no MEAN tag → midpoint fallback
        assert!((s.bands[1].mean - (74.0 + 2213.0) / 2.0).abs() < 1e-9);
    }

    #[test]
    fn falls_back_to_sminmax_then_none() {
        // No XML, but SMin/SMax tags present.
        let s = parse_embedded_stats(None, Some(&[10, 20]), Some(&[100, 200]), 2).unwrap();
        assert_eq!(s.bands[0].min, 10.0);
        assert_eq!(s.bands[1].max, 200.0);
        // Nothing at all → None.
        assert!(parse_embedded_stats(None, None, None, 1).is_none());
        // Malformed XML → None (no usable min/max).
        assert!(parse_embedded_stats(Some("<not xml"), None, None, 1).is_none());
    }

    #[test]
    fn inferred_embedded_fields_are_not_claimed_exact() {
        let stats = parse_embedded_stats(None, Some(&[10]), Some(&[100]), 1).unwrap();
        assert_ne!(stats.provenance, Provenance::Exact);
    }

    // Exercises the estimated (overview) path against a local fixture.
    // Ignored by default: requires a multi-overview UInt16 COG on disk.
    #[tokio::test]
    #[ignore = "requires local data/SVN3-..._cog.tif"]
    async fn estimated_stats_from_overview() {
        use object_store::local::LocalFileSystem;
        use std::sync::Arc;

        let fs = Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        let reader = crate::io::CogReader::open_object_store(
            fs,
            "data/SVN3-01_20250814_L2D0000203064_01_6112500050210001-FUS_3857_cog.tif",
        )
        .await
        .unwrap();
        let s = load_dataset_stats(&reader).await.unwrap().unwrap();
        assert_eq!(s.provenance, Provenance::Estimated);
        assert_eq!(s.bands.len(), 3);
        assert!(s.bands[0].min >= 0.0 && s.bands[0].max < 65535.0);
        assert!(s.bands[0].max > s.bands[0].min);
    }
}
