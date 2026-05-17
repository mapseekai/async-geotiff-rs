//! Dataset band statistics: embedded `STATISTICS_*` tags (exact) or a
//! one-shot top-overview computation (estimated). See
//! docs/superpowers/specs/2026-05-16-non-uint8-data-stretch-design.md.

use crate::error::AgResult;
use crate::io::CogReader;
use ndarray::{Array3, Axis};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Provenance {
    Exact,
    Estimated,
}

/// Equal-width histogram over `[min, max]`. `counts.len()` is the bin count.
#[derive(Debug, Clone)]
pub struct Histogram {
    pub min: f64,
    pub max: f64,
    pub counts: Vec<u64>,
}

impl Histogram {
    /// Value at percentile `p` (0..=100) via linear interpolation on the
    /// cumulative count. Robust to the overview-smoothing approximation
    /// (see spec accuracy table). Empty / degenerate → `min`.
    pub fn quantile(&self, p: f64) -> f64 {
        let total: u64 = self.counts.iter().sum();
        if total == 0 || self.max <= self.min || self.counts.is_empty() {
            return self.min;
        }
        let target = (p / 100.0) * total as f64;
        let bin_w = (self.max - self.min) / self.counts.len() as f64;
        let mut cum = 0u64;
        for (i, &c) in self.counts.iter().enumerate() {
            let next = cum + c;
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

#[derive(Debug, Clone)]
pub struct BandStat {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub stddev: f64,
    pub hist: Histogram,
}

#[derive(Debug, Clone)]
pub struct DatasetStats {
    /// Indexed by original (full) band order.
    pub bands: Vec<BandStat>,
    pub provenance: Provenance,
}

/// Per-band stats from a planar `(bands, H, W)` f64 array, excluding
/// `nodata` and non-finite samples. `hist_bins` is the histogram
/// resolution (256 in production).
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
            Some(nd) => (v - nd).abs() >= 1e-9,
            None => true,
        }
    };
    let mut out = Vec::with_capacity(arr.shape()[0]);
    for b in 0..arr.shape()[0] {
        let band = arr.index_axis(Axis(0), b);
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut sum = 0.0_f64;
        let mut sumsq = 0.0_f64;
        let mut count = 0_u64;
        for &v in band.iter() {
            if !valid(v) {
                continue;
            }
            min = min.min(v);
            max = max.max(v);
            sum += v;
            sumsq += v * v;
            count += 1;
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
        let mean = sum / count as f64;
        let var = (sumsq / count as f64) - mean * mean;
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

/// Parse exact stats from a COG's GDAL_METADATA XML, falling back to
/// TIFF `SMin/SMaxSampleValue`. Returns `None` when neither yields a
/// usable min/max for every band. Histogram comes from
/// `STATISTICS_HISTOMIN/HISTOMAX/HISTONUMBINS/HISTOBINS` when present,
/// otherwise a single-bin placeholder (percentile then ≈ min/max).
pub fn parse_embedded_stats(
    gdal_metadata: Option<&str>,
    smin: Option<&[u16]>,
    smax: Option<&[u16]>,
    bands: usize,
) -> Option<DatasetStats> {
    // Pull `<Item name="X" sample="N">V</Item>` into (name, sample, value).
    fn items(xml: &str) -> Vec<(String, usize, String)> {
        let mut v = Vec::new();
        for chunk in xml.split("<Item").skip(1) {
            let Some(open_end) = chunk.find('>') else {
                continue;
            };
            let attrs = &chunk[..open_end];
            let body = &chunk[open_end + 1..];
            let Some(close) = body.find("</Item>") else {
                continue;
            };
            let value = body[..close].trim().to_string();
            let name = attr(attrs, "name");
            let sample = attr(attrs, "sample").parse::<usize>().unwrap_or(0);
            if !name.is_empty() {
                v.push((name, sample, value));
            }
        }
        v
    }
    fn attr(attrs: &str, key: &str) -> String {
        let pat = format!("{key}=\"");
        let Some(start) = attrs.find(&pat) else {
            return String::new();
        };
        let rest = &attrs[start + pat.len()..];
        rest.find('"')
            .map(|e| rest[..e].to_string())
            .unwrap_or_default()
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
        provenance: Provenance::Exact,
    })
}

const HIST_BINS: usize = 256;

/// Embedded `STATISTICS_*` tags (exact) when present; otherwise compute
/// from the highest (smallest) overview (estimated). `Ok(None)` when the
/// COG has no overview to fall back to — caller then uses
/// `stretch::fallback_rescale`. Reads ≈ the top overview only (a few
/// hundred KB), independent of full-resolution size.
pub async fn load_dataset_stats(reader: &CogReader) -> AgResult<Option<DatasetStats>> {
    // 1) Exact path: embedded tags on the full-res IFD. Scope the IFD
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
    let img = reader.read_window(top, 0, 0, w as u64, h as u64).await?;
    let arr = img.pixels.to_f64();
    let stats = band_stats_from_f64(&arr, img.nodata, HIST_BINS);
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
        assert_eq!(s.provenance, Provenance::Exact);
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

    // Exercises the estimated (overview) path against a local fixture.
    // Ignored by default: requires a multi-overview UInt16 COG on disk.
    #[tokio::test]
    #[ignore = "requires local data/SVN3-..._cog.tif"]
    async fn estimated_stats_from_overview() {
        use async_tiff::reader::ObjectReader;
        use object_store::local::LocalFileSystem;
        use object_store::path::Path;
        use std::sync::Arc;

        let fs = Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        let p =
            Path::from("data/SVN3-01_20250814_L2D0000203064_01_6112500050210001-FUS_3857_cog.tif");
        let reader = crate::io::CogReader::open_with_reader(ObjectReader::new(fs, p))
            .await
            .unwrap();
        let s = load_dataset_stats(&reader).await.unwrap().unwrap();
        assert_eq!(s.provenance, Provenance::Estimated);
        assert_eq!(s.bands.len(), 3);
        assert!(s.bands[0].min >= 0.0 && s.bands[0].max < 65535.0);
        assert!(s.bands[0].max > s.bands[0].min);
    }
}
