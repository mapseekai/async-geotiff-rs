//! Shared statistics on a fixed 1024² EPSG:4326 footprint grid, never tile-local.
use super::{
    raster::{RasterPlan, StatisticsResult},
    stretch::Stretch,
    TileStyle,
};
use crate::{
    blocking::CpuLimiter,
    bounds::Bbox,
    crs::Crs,
    error::{AgError, AgResult},
    io::CogReader,
    request::RequestContext,
    resource::WorkBudget,
};
use std::sync::{Arc, OnceLock};
use tokio::sync::OnceCell;

const GRID_SIZE: usize = 1024;
struct Entry {
    bytes: u32,
    value: OnceCell<Cached>,
}
enum Cached {
    Samples(Arc<Vec<Vec<f64>>>),
    Transfers(Arc<StatisticsResult>),
}
type Cache = moka::future::Cache<String, Arc<Entry>>;

async fn entry(cache: &Cache, key: String, payload_bytes: usize) -> Arc<Entry> {
    let bytes = (payload_bytes + key.len() * 2 + 1024).min(u32::MAX as usize) as u32;
    cache
        .get_with(key, async move {
            Arc::new(Entry {
                bytes,
                value: OnceCell::new(),
            })
        })
        .await
}

pub(crate) async fn resolve(
    style: &TileStyle,
    readers: &[Arc<CogReader>],
    mosaic: bool,
    cpu: &CpuLimiter,
    budget: &WorkBudget,
    context: &RequestContext,
) -> AgResult<Arc<RasterPlan>> {
    style.raster.statistics.check_scope(mosaic)?;
    if !style.raster.stretch.needs_statistics() {
        return Ok(style.raster.clone());
    }
    // Samples and transfers share a 96 MiB retained-memory budget. OnceCell
    // coalesces viewport work without caching errors or cancellation.
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        moka::future::Cache::builder()
            .max_capacity(96 * 1024 * 1024)
            .weigher(|_: &String, entry: &Arc<Entry>| entry.bytes)
            .build()
    });
    let identities: Vec<_> = readers.iter().map(|r| r.statistics_identity()).collect();
    let source_key = serde_json::to_string(&(mosaic, &style.raster.statistics_key, identities))
        .expect("string cache key");
    let k = style
        .raster
        .statistics
        .sample_size
        .unwrap_or(GRID_SIZE * GRID_SIZE)
        .min(GRID_SIZE * GRID_SIZE);
    let channels = style.raster.mosaic_band_count_for_statistics();
    let transfer_key =
        serde_json::to_string(&("transfers", &source_key, &style.raster.stretch_key))
            .expect("string cache key");
    // Include both native RGB LUT sizes even before lazy initialization.
    let transfer_bytes = if matches!(style.raster.stretch, Stretch::Histogram) {
        k * channels * 16
    } else {
        channels * 64
    } + 3 * (256 + 65536);
    let transfer_entry = entry(cache, transfer_key, transfer_bytes).await;
    let resolved = transfer_entry
        .value
        .get_or_try_init(|| async {
            let sample_key =
                serde_json::to_string(&("samples", &source_key)).expect("string cache key");
            let sample_entry = entry(cache, sample_key, k * channels * 8).await;
            let samples = sample_entry
                .value
                .get_or_try_init(|| async {
                    compute(style, readers, mosaic, cpu, budget, context)
                        .await
                        .map(Cached::Samples)
                })
                .await?;
            let Cached::Samples(samples) = samples else {
                unreachable!("sample cache key")
            };
            let samples = samples.clone();
            let work_bytes =
                samples.iter().map(|v| v.len() as u64 * 32).sum::<u64>() + transfer_bytes as u64;
            let ctx = context.with_work(budget.try_reserve(work_bytes)?);
            let raster = style.raster.clone();
            let permit = cpu.acquire().await?;
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                ctx.check()?;
                let result = raster.resolve_samples(&samples)?;
                ctx.check()?;
                Ok::<_, AgError>(Cached::Transfers(Arc::new(result)))
            })
            .await
            .map_err(|e| AgError::Warp(format!("statistics transfer: {e}")))?
        })
        .await?;
    context.check()?;
    let Cached::Transfers(result) = resolved else {
        unreachable!("transfer cache key")
    };
    Ok(Arc::new(style.raster.with_statistics(result)))
}

async fn compute(
    style: &TileStyle,
    readers: &[Arc<CogReader>],
    mosaic: bool,
    cpu: &CpuLimiter,
    budget: &WorkBudget,
    context: &RequestContext,
) -> AgResult<Arc<Vec<Vec<f64>>>> {
    if readers.is_empty() {
        return Err(AgError::query("statistics", "no bound sources"));
    }
    let permit = cpu.acquire().await?;
    let sources = readers.to_vec();
    let ctx = context.clone();
    let bbox = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        ctx.check()?;
        let mut bbox = Bbox::new(
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for reader in sources {
            crate::tiler::Tiler::new(reader.clone()).check_raster_source()?;
            let b = reader.wgs84_bounds()?;
            bbox.xmin = bbox.xmin.min(b[0]);
            bbox.ymin = bbox.ymin.min(b[1]);
            bbox.xmax = bbox.xmax.max(b[2]);
            bbox.ymax = bbox.ymax.max(b[3]);
        }
        if ![bbox.xmin, bbox.ymin, bbox.xmax, bbox.ymax]
            .iter()
            .all(|v| v.is_finite())
            || bbox.width() <= 0.
            || bbox.height() <= 0.
        {
            return Err(AgError::query("statistics", "invalid footprint grid"));
        }
        Ok(bbox)
    })
    .await
    .map_err(|e| AgError::Warp(format!("statistics bounds: {e}")))??;
    let channels = style.raster.mosaic_band_count_for_statistics() as u64;
    let grid_bytes = (GRID_SIZE as u64)
        .pow(2)
        .checked_mul(channels * 64 + 64)
        .ok_or_else(|| AgError::ResourceLimit("statistics grid estimate overflow".into()))?;
    // Keep the grid/reducer charged between sources, but not its temporary
    // warp/statistics buffers while read_window reserves decoder/stitch buffers.
    // The retained allowance covers F64 bands, masks and mean-reducer counts.
    let retained_bytes = (GRID_SIZE as u64).pow(2) * (channels * 16 + 16);
    let scratch_bytes = grid_bytes - retained_bytes;
    #[allow(unused_mut)]
    let mut context = context.with_work(budget.try_reserve(retained_bytes)?);
    #[cfg(feature = "mosaic")]
    let mut reducer: Option<
        crate::mosaic::selection::Reducer<crate::mosaic::selection::NonEmptyReducer>,
    > = None;
    let mut single = None;
    for reader in readers {
        context.check()?;
        let (w, h) = reader.level_dimensions(0)?;
        let tr = reader.transform();
        let native = Bbox::from_corners(&[
            tr.forward(0., 0.),
            tr.forward(w as f64, 0.),
            tr.forward(w as f64, h as f64),
            tr.forward(0., h as f64),
        ]);
        let level = reader
            .pick_overview(native.width().abs().max(native.height().abs()) / GRID_SIZE as f64);
        let (w, h) = reader.level_dimensions(level)?;
        let bands = u64::from(reader.data_ifd(level)?.samples_per_pixel());
        let bytes = u64::from(w)
            .checked_mul(u64::from(h))
            .and_then(|n| n.checked_mul(bands * 24 + 2))
            .ok_or_else(|| AgError::ResourceLimit("statistics source estimate overflow".into()))?;
        let mut image = reader
            .read_window(level, 0, 0, u64::from(w), u64::from(h))
            .await?;
        // read_window already accounts for the returned source while decoding.
        // After it returns, charge the source/warp stage instead of overlapping
        // that reservation with the decoder's much larger temporary buffers.
        let work = budget.try_reserve(bytes)?;
        let scratch = budget.try_reserve(scratch_bytes)?;
        let stage_context = context.with_work(scratch.clone());
        if let Some(nodata) = style.nodata {
            Arc::make_mut(&mut image).nodata = Some(nodata);
        }
        let kernel = if image.crs == Crs::epsg(4326) {
            style.resampling
        } else {
            style.reproject
        };
        let raster = style.raster.clone();
        let ctx = stage_context.clone();
        let permit = cpu.acquire().await?;
        let warped = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _work = work;
            ctx.check()?;
            let image =
                raster.warp_dependencies(&image, &Crs::epsg(4326), bbox, GRID_SIZE, kernel)?;
            ctx.check()?;
            #[cfg(feature = "mosaic")]
            let image = if mosaic {
                raster.prepare_mosaic(image)
            } else {
                image
            };
            Ok::<_, AgError>(image)
        })
        .await
        .map_err(|e| AgError::Warp(format!("statistics warp: {e}")))??;
        #[cfg(feature = "mosaic")]
        if mosaic {
            let config = style
                .raster
                .mosaic
                .as_ref()
                .expect("validated mosaic request");
            let (selection, rank) = (config.selection(), config.rank);
            // Median retains each input grid, so reserve that memory explicitly.
            if selection == crate::mosaic::PixelSelection::Median {
                context = context
                    .with_work(budget.try_reserve(channels * (GRID_SIZE as u64).pow(2) * 9)?);
            }
            let ctx = context.with_work(scratch.clone());
            let permit = cpu.acquire().await?;
            reducer = Some(
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    ctx.check()?;
                    if let Some(mut reducer) = reducer {
                        reducer.feed(&warped)?;
                        Ok(reducer)
                    } else {
                        crate::mosaic::selection::Reducer::new_raster(selection, rank).feed(&warped)
                    }
                })
                .await
                .map_err(|e| AgError::Mosaic(format!("statistics reduce: {e}")))??,
            );
            continue;
        }
        single = Some(warped);
    }
    let context = context.with_work(budget.try_reserve(scratch_bytes)?);
    let raster = style.raster.clone();
    let permit = cpu.acquire().await?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        context.check()?;
        #[cfg(feature = "mosaic")]
        let single = if let Some(reducer) = reducer {
            Some(reducer.finish()?)
        } else {
            single
        };
        let image = single.ok_or_else(|| AgError::query("statistics", "no statistics image"))?;
        let channels_ready = mosaic && raster.mosaic.as_ref().is_some_and(|m| m.after_channels);
        let resolved = raster.statistics_samples(&image, channels_ready);
        context.check()?;
        Ok(Arc::new(resolved))
    })
    .await
    .map_err(|e| AgError::Warp(format!("statistics resolve: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn display_changes_reuse_transfers_and_methods_reuse_samples() {
        use crate::request::{RequestLimiter, RequestOptions};
        use object_store::{memory::InMemory, path::Path, ObjectStoreExt};
        use serde_json::json;
        use std::time::Duration;
        let store = Arc::new(InMemory::new());
        store
            .put(
                &Path::from("cache-fixture"),
                bytes::Bytes::from_static(include_bytes!("../../tests/fixtures/rgb_nodata.tif"))
                    .into(),
            )
            .await
            .unwrap();
        let reader = Arc::new(
            CogReader::open_object_store(store, "cache-fixture")
                .await
                .unwrap(),
        );
        let cpu = CpuLimiter::new(1);
        let mut input = json!({"version":"2.0","renderer":{"type":"gray","bidx":[2]},
            "stretch":{"method":"minmax"},"statistics":{"scope":"dataset","accuracy":"sample","sample_size":1024}});
        let mut original_key = String::new();
        let mut original_transfer_key = String::new();
        for step in 0..5 {
            match step {
                1 => input["opacity"] = json!(0.5),
                2 => input["stretch"] = json!({"method":"stddev","stddev":2}),
                3 => input["stretch"]["range_policy"] = json!("transparent"),
                4 => {
                    input["renderer"]["type"] = json!("pseudocolor");
                    input["renderer"]["colormap_name"] = json!("viridis");
                }
                _ => {}
            }
            let style = TileStyle::from_json(&input.to_string())
                .unwrap_or_else(|e| panic!("step={step}: {e}; {input}"));
            if step == 0 {
                original_key = style.raster.statistics_key.clone();
            } else {
                assert_eq!(style.raster.statistics_key, original_key);
            }
            if step == 2 {
                original_transfer_key = style.raster.stretch_key.clone();
            }
            if step >= 3 {
                assert_eq!(style.raster.stretch_key, original_transfer_key);
            }
            // Transfer hits cannot wait for CPU. A method miss may use CPU, but
            // 1 MiB cannot accommodate reading/warping a new 1024² sample grid.
            let held = if matches!(step, 1 | 3 | 4) {
                Some(cpu.acquire().await.unwrap())
            } else {
                None
            };
            let budget = WorkBudget::new(if step == 0 {
                512 * 1024 * 1024
            } else {
                1024 * 1024
            });
            let resolved = RequestLimiter::new(1)
                .run(
                    Duration::from_secs(10),
                    &RequestOptions::default(),
                    |context| {
                        let (style, cpu, budget, reader) = (&style, &cpu, &budget, reader.clone());
                        async move { resolve(style, &[reader], false, cpu, budget, &context).await }
                    },
                )
                .await
                .unwrap();
            drop(held);
            assert_eq!(budget.used_bytes(), 0);
            let image = reader.read_window(0, 0, 0, 2, 2).await.unwrap();
            let actual = resolved
                .render(&image, None, super::super::TileFormat::Png)
                .unwrap();
            // Rebind the same ranges through a fresh explicit style, so all
            // opacity, range policy and color properties are checked pixelwise.
            let mut explicit = input.clone();
            explicit["stretch"]
                .as_object_mut()
                .unwrap()
                .remove("stddev");
            explicit["stretch"]["method"] = json!("linear");
            explicit["stretch"]["rescale"] = json!(resolved
                .stretch
                .transfers()
                .unwrap()
                .iter()
                .map(|t| {
                    match t {
                        super::super::stretch::Transfer::Range(lo, hi) => [*lo, *hi],
                        _ => panic!("range expected"),
                    }
                })
                .collect::<Vec<_>>());
            let explicit = TileStyle::from_json(&explicit.to_string()).unwrap();
            let expected = explicit
                .raster
                .render(&image, None, super::super::TileFormat::Png)
                .unwrap();
            let decode = |bytes: bytes::Bytes| image::load_from_memory(&bytes).unwrap().to_rgba8();
            let rgba = decode(actual);
            assert_eq!(rgba, decode(expected), "display step {step}");
            assert!(rgba
                .pixels()
                .filter(|p| p[3] != 0)
                .all(|p| p[3] == if step == 0 { 255 } else { 128 }));
            if step == 4 {
                assert!(rgba.pixels().any(|p| p[0] != p[1] || p[1] != p[2]));
            }
        }
        input["nodata"] = json!(99);
        assert_ne!(
            TileStyle::from_json(&input.to_string())
                .unwrap()
                .raster
                .statistics_key,
            original_key
        );
    }

    #[tokio::test]
    async fn rgb_statistics_fit_default_budget_without_overlapping_decode_scratch() {
        use crate::request::{RequestLimiter, RequestOptions};
        use object_store::{memory::InMemory, path::Path, ObjectStoreExt};
        use std::time::Duration;
        let store = Arc::new(InMemory::new());
        store
            .put(
                &Path::from("fixture"),
                bytes::Bytes::from_static(include_bytes!(
                    "../../tests/fixtures/rgb_uint16_statistics.tif"
                ))
                .into(),
            )
            .await
            .unwrap();
        let budget = WorkBudget::new(512 * 1024 * 1024);
        let reader = Arc::new(
            CogReader::builder()
                .reader_config(crate::io::ReaderConfig {
                    work_budget: budget.clone(),
                    ..Default::default()
                })
                .open_object_store(store, "fixture")
                .await
                .unwrap(),
        );
        let baseline = budget.used_bytes();
        let cpu = CpuLimiter::new(1);
        let mut queries = vec!["version=2.0&type=rgb&bidx=1&bidx=2&bidx=3&method=minmax"];
        if cfg!(feature = "mosaic") {
            queries.push("version=2.0&type=rgb&bidx=1&bidx=2&bidx=3&method=minmax&pixel_selection=mean&stage=before_channels");
        }
        for query in queries {
            let style = TileStyle::from_raster_query(query).unwrap();
            let mosaic = style.raster.mosaic.is_some();
            let sources = vec![reader.clone(); if mosaic { 2 } else { 1 }];
            RequestLimiter::new(1)
                .run(
                    Duration::from_secs(30),
                    &RequestOptions::default(),
                    |context| {
                        let (style, sources, cpu, budget) = (&style, &sources, &cpu, &budget);
                        async move { compute(style, sources, mosaic, cpu, budget, &context).await }
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                budget.used_bytes(),
                baseline,
                "statistics leases must release"
            );
            assert!(budget.peak_bytes() <= budget.limit_bytes());
        }
    }

    #[tokio::test]
    async fn statistics_waits_for_callers_cpu_limiter() {
        use crate::request::{RequestLimiter, RequestOptions};
        use object_store::{memory::InMemory, path::Path, ObjectStoreExt};
        use std::time::Duration;
        let store = Arc::new(InMemory::new());
        store
            .put(
                &Path::from("fixture"),
                bytes::Bytes::from_static(include_bytes!("../../tests/fixtures/rgb_nodata.tif"))
                    .into(),
            )
            .await
            .unwrap();
        let reader = Arc::new(
            CogReader::builder()
                .reader_config(crate::io::ReaderConfig {
                    work_budget: WorkBudget::new(512 * 1024 * 1024),
                    ..Default::default()
                })
                .open_object_store(store, "fixture")
                .await
                .unwrap(),
        );
        let reader_budget = reader.work_budget();
        let baseline = reader_budget.peak_bytes();
        let cpu = CpuLimiter::new(1);
        let _held = cpu.acquire().await.unwrap();
        let style =
            TileStyle::from_raster_query("version=2.0&type=gray&bidx=2&method=minmax").unwrap();
        let result = RequestLimiter::new(1)
            .run(
                Duration::from_millis(30),
                &RequestOptions::default(),
                |context| async move {
                    compute(
                        &style,
                        &[reader],
                        false,
                        &cpu,
                        &WorkBudget::new(512 * 1024 * 1024),
                        &context,
                    )
                    .await
                },
            )
            .await;
        assert!(matches!(result, Err(AgError::DeadlineExceeded)));
        assert_eq!(
            reader_budget.peak_bytes(),
            baseline,
            "statistics must not run using the reader's free CPU pool"
        );
    }
}
