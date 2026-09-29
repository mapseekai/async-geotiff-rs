//! Shared statistics on a fixed 1024² EPSG:4326 footprint grid, never tile-local.
use super::{raster::RasterPlan, TileStyle};
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
type Entry = Arc<OnceCell<Arc<RasterPlan>>>;

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
    // Two entries bound retained empirical CDFs to at most about 96 MiB.
    // OnceCell coalesces viewport requests without caching errors/cancellation.
    static CACHE: OnceLock<moka::future::Cache<String, Entry>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| moka::future::Cache::new(2));
    let identities: Vec<_> = readers.iter().map(|r| r.statistics_identity()).collect();
    let key =
        serde_json::to_string(&(mosaic, &style.raster.key, identities)).expect("string cache key");
    let entry = cache
        .get_with(key, async { Arc::new(OnceCell::new()) })
        .await;
    let result = entry
        .get_or_try_init(|| compute(style, readers, mosaic, cpu, budget, context))
        .await?;
    context.check()?;
    Ok(result.clone())
}

async fn compute(
    style: &TileStyle,
    readers: &[Arc<CogReader>],
    mosaic: bool,
    cpu: &CpuLimiter,
    budget: &WorkBudget,
    context: &RequestContext,
) -> AgResult<Arc<RasterPlan>> {
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
    let bytes = (GRID_SIZE as u64)
        .pow(2)
        .checked_mul(channels * 64 + 64)
        .ok_or_else(|| AgError::ResourceLimit("statistics grid estimate overflow".into()))?;
    #[allow(unused_mut)]
    let mut context = context.with_work(budget.try_reserve(bytes)?);
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
        let work = budget.try_reserve(bytes)?;
        let mut image = reader
            .read_window(level, 0, 0, u64::from(w), u64::from(h))
            .await?;
        if let Some(nodata) = style.nodata {
            Arc::make_mut(&mut image).nodata = Some(nodata);
        }
        let kernel = if image.crs == Crs::epsg(4326) {
            style.resampling
        } else {
            style.reproject
        };
        let raster = style.raster.clone();
        let ctx = context.clone();
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
            let ctx = context.clone();
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
        let resolved = raster.resolve_statistics(&image, channels_ready)?;
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
