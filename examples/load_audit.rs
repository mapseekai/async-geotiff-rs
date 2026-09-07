//! Bounded loopback workload. Run through scripts/run_load_audit.py to record RSS.
//! Synthetic fixtures and local timings do not represent production capacity.

#[cfg(not(feature = "mosaic"))]
fn main() {
    eprintln!("load_audit requires --features mosaic");
}

#[cfg(feature = "mosaic")]
#[tokio::main(worker_threads = 4)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    workload::run().await
}

#[cfg(feature = "mosaic")]
mod workload {
    use async_geotiff::{
        bounds::Bbox,
        error::AgResult,
        io::{BlockCache, CacheConfig, CogReader},
        mosaic::{AssetRef, HttpAccessPolicy, MosaicSource, MosaicTiler, PixelSelection},
        request::{RequestCancellation, RequestLimiter, RequestOptions},
        resource::WorkBudget,
        style::TileStyle,
        tiler::Tiler,
        tms::{TileCoord, TileMatrixSet},
    };
    use axum::{
        body::Body,
        extract::State,
        http::{header, HeaderMap, Method, StatusCode},
        response::Response,
        routing::get,
        Router,
    };
    use serde_json::{json, Value};
    use std::{
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };
    use tokio::task::JoinSet;

    const TIFF: &[u8] = include_bytes!("../tests/fixtures/rgb_deflate.tif");

    #[derive(Default)]
    struct HttpMetrics {
        started: AtomicU64,
        completed: AtomicU64,
        active: AtomicU64,
        peak_active: AtomicU64,
        bytes: AtomicU64,
        delay_ms: AtomicU64,
    }

    impl HttpMetrics {
        fn snapshot(&self) -> Value {
            json!({"get_started": self.started.load(Ordering::Relaxed),
                "get_completed": self.completed.load(Ordering::Relaxed),
                "active": self.active.load(Ordering::Relaxed),
                "peak_active": self.peak_active.load(Ordering::Relaxed),
                "response_bytes": self.bytes.load(Ordering::Relaxed)})
        }
    }

    struct Active(Arc<HttpMetrics>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Ordering::Relaxed);
        }
    }

    async fn serve(
        State(metrics): State<Arc<HttpMetrics>>,
        method: Method,
        headers: HeaderMap,
    ) -> Response {
        let etag = "\"synthetic-deflate-v1\"";
        if headers
            .get(header::IF_MATCH)
            .is_some_and(|value| value != etag)
        {
            return Response::builder()
                .status(StatusCode::PRECONDITION_FAILED)
                .body(Body::empty())
                .unwrap();
        }
        if method == Method::HEAD {
            return Response::builder()
                .header(header::CONTENT_LENGTH, TIFF.len())
                .header(header::ETAG, etag)
                .body(Body::empty())
                .unwrap();
        }
        metrics.started.fetch_add(1, Ordering::Relaxed);
        let active = metrics.active.fetch_add(1, Ordering::Relaxed) + 1;
        metrics.peak_active.fetch_max(active, Ordering::Relaxed);
        let _active = Active(metrics.clone());
        tokio::time::sleep(Duration::from_millis(
            metrics.delay_ms.load(Ordering::Relaxed),
        ))
        .await;
        let range = headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("bytes="))
            .and_then(|v| v.split_once('-'))
            .and_then(|(a, b)| {
                Some((
                    a.parse::<usize>().ok()?,
                    if b.is_empty() {
                        TIFF.len() - 1
                    } else {
                        b.parse::<usize>().ok()?
                    },
                ))
            });
        let (start, end) = range.unwrap_or((0, TIFF.len() - 1));
        if start >= TIFF.len() || end < start {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .body(Body::empty())
                .unwrap();
        }
        let end = end.min(TIFF.len() - 1);
        let bytes = &TIFF[start..=end];
        metrics.completed.fetch_add(1, Ordering::Relaxed);
        metrics
            .bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let mut response = Response::builder()
            .header(header::ETAG, etag)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, bytes.len());
        if range.is_some() {
            response = response.status(StatusCode::PARTIAL_CONTENT).header(
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{}", TIFF.len()),
            );
        }
        response.body(Body::from(bytes)).unwrap()
    }

    struct Fixed(Vec<AssetRef>);
    #[async_trait::async_trait]
    impl MosaicSource for Fixed {
        async fn assets_for_tile(&self, _: TileCoord) -> AgResult<Vec<AssetRef>> {
            Ok(self.0.clone())
        }
    }

    enum Renderer {
        Single(Tiler),
        Mosaic(MosaicTiler<Fixed>),
    }
    impl Renderer {
        async fn create(base: &str, mosaic: bool, cache: Arc<BlockCache>) -> AgResult<Self> {
            if mosaic {
                let assets = (0..3)
                    .map(|i| AssetRef {
                        url: format!("{base}/{i}.tif"),
                        source_id: format!("asset-{i}"),
                        bbox: Bbox::new(10.0, 20.0, 10.064, 20.064),
                    })
                    .collect();
                Ok(Self::Mosaic(
                    MosaicTiler::builder(Fixed(assets))
                        .block_cache(cache)
                        .http_policy(HttpAccessPolicy::permissive())
                        .build(),
                ))
            } else {
                let reader = CogReader::builder()
                    .block_cache(cache)
                    .open_http(&format!("{base}/0.tif"))
                    .await?;
                Ok(Self::Single(Tiler::new(Arc::new(reader))))
            }
        }

        async fn tile(&self, coord: TileCoord, style: &TileStyle) -> AgResult<usize> {
            self.tile_with_options(coord, style, &RequestOptions::default())
                .await
        }

        async fn tile_with_options(
            &self,
            coord: TileCoord,
            style: &TileStyle,
            options: &RequestOptions,
        ) -> AgResult<usize> {
            let output = match self {
                Self::Single(renderer) => renderer.tile_with_options(coord, style, options).await?,
                Self::Mosaic(renderer) => {
                    renderer
                        .tile_with_options(coord, PixelSelection::Mean, style, options)
                        .await?
                }
            };
            Ok(output.expect("fixture center must produce a tile").len())
        }
    }

    fn coord() -> TileCoord {
        let latitude = 20.032_f64.to_radians();
        TileCoord {
            tms: TileMatrixSet::WebMercatorQuad,
            z: 12,
            x: ((10.032 + 180.0) / 360.0 * 4096.0) as u32,
            y: ((1.0 - (latitude.tan() + 1.0 / latitude.cos()).ln() / std::f64::consts::PI) / 2.0
                * 4096.0) as u32,
        }
    }

    fn cache_json(cache: &BlockCache) -> Value {
        let s = cache.stats();
        json!({"hits":s.hits,"misses":s.misses,"decoded":s.decoded,"fetched":s.fetched,"entries":s.entries,"resident_bytes":s.resident_bytes})
    }

    fn work_json() -> Value {
        let budget = WorkBudget::global();
        json!({"used_bytes":budget.used_bytes(),"peak_bytes":budget.peak_bytes(),"limit_bytes":budget.limit_bytes()})
    }

    fn quantile(sorted: &[f64], percentile: usize) -> f64 {
        sorted[(sorted.len() * percentile).div_ceil(100).saturating_sub(1)]
    }

    async fn scenario(
        base: &str,
        http: Arc<HttpMetrics>,
        name: &str,
        mosaic: bool,
        cold: bool,
        count: usize,
        concurrency: usize,
    ) -> Value {
        let cache = Arc::new(BlockCache::new(CacheConfig::default()));
        let warm = if cold {
            None
        } else {
            Some(Arc::new(
                Renderer::create(base, mosaic, cache.clone()).await.unwrap(),
            ))
        };
        let style = TileStyle::from_query(
            "tilesize=64&rescale=0,255&resampling=bilinear&reproject=bilinear",
        )
        .unwrap();
        if let Some(renderer) = &warm {
            renderer.tile(coord(), &style).await.unwrap();
        }
        cache.sync_count().await;
        let cache_before = cache_json(&cache);
        let before = http.snapshot();
        http.peak_active.store(0, Ordering::Relaxed);
        let started = Instant::now();
        let mut latencies = Vec::new();
        let mut cold_caches = Vec::new();
        let mut output_bytes = 0;
        for batch in (0..count).step_by(concurrency) {
            let mut jobs = JoinSet::new();
            for _ in batch..(batch + concurrency).min(count) {
                let (base, renderer, style) = (base.to_owned(), warm.clone(), style.clone());
                let shared_cache = cache.clone();
                jobs.spawn(async move {
                    let started = Instant::now();
                    let cache = if renderer.is_some() {
                        shared_cache
                    } else {
                        Arc::new(BlockCache::new(CacheConfig::default()))
                    };
                    let renderer = match renderer {
                        Some(r) => r,
                        None => Arc::new(
                            Renderer::create(&base, mosaic, cache.clone())
                                .await
                                .unwrap(),
                        ),
                    };
                    let bytes = renderer.tile(coord(), &style).await.unwrap();
                    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                    cache.sync_count().await;
                    (elapsed, bytes, cache_json(&cache))
                });
            }
            while let Some(result) = jobs.join_next().await {
                let (elapsed, bytes, stats) = result.unwrap();
                latencies.push(elapsed);
                output_bytes += bytes;
                if cold {
                    cold_caches.push(stats);
                }
            }
        }
        let duration = started.elapsed().as_secs_f64();
        cache.sync_count().await;
        latencies.sort_by(f64::total_cmp);
        let after = http.snapshot();
        let get_count =
            after["get_started"].as_u64().unwrap() - before["get_started"].as_u64().unwrap();
        json!({"scenario":name,"samples":count,"concurrency":concurrency,"assets":if mosaic {3}else{1},"cold_includes_open":cold,
            "latency_ms":{"p50":quantile(&latencies,50),"p95":quantile(&latencies,95),"p99":quantile(&latencies,99)},
            "elapsed_seconds":duration,"completed_per_second":count as f64/duration,"encoded_bytes":output_bytes,
            "get_count":get_count,"http_before":before,"http_after":after,"cache_before":cache_before,"cache_after":cache_json(&cache),"cold_request_caches":cold_caches,"work_budget":work_json()})
    }

    async fn cancellation(base: &str, http: Arc<HttpMetrics>, concurrency: usize) -> Value {
        let cache = Arc::new(BlockCache::new(CacheConfig::default()));
        let renderer = Arc::new(Renderer::create(base, false, cache.clone()).await.unwrap());
        http.delay_ms.store(100, Ordering::Relaxed);
        let before = http.snapshot();
        let mut jobs = JoinSet::new();
        for _ in 0..concurrency {
            let renderer = renderer.clone();
            jobs.spawn(async move { renderer.tile(coord(), &TileStyle::default()).await });
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        jobs.abort_all();
        while jobs.join_next().await.is_some() {}
        let at_abort = http.snapshot();
        let cache_at_abort = cache_json(&cache);
        tokio::time::sleep(Duration::from_millis(250)).await;
        cache.sync_count().await;
        http.delay_ms.store(2, Ordering::Relaxed);
        json!({"scenario":"abort_single_read","concurrency":concurrency,"server_delay_ms":100,"abort_after_ms":20,"settle_ms":250,
            "http_before":before,"http_at_abort":at_abort,"http_after_settle":http.snapshot(),"cache_at_abort":cache_at_abort,"cache_after_settle":cache_json(&cache)})
    }

    async fn controlled_stop(
        base: &str,
        http: Arc<HttpMetrics>,
        deadline: bool,
        mosaic: bool,
    ) -> Value {
        let limit = RequestLimiter::new(2);
        let cache = Arc::new(BlockCache::new(CacheConfig::default()));
        let source = Fixed(
            (0..3)
                .map(|i| AssetRef {
                    url: format!("{base}/{i}.tif"),
                    source_id: format!("controlled-{i}"),
                    bbox: Bbox::new(10.0, 20.0, 10.064, 20.064),
                })
                .collect(),
        );
        let renderer = Arc::new(if mosaic {
            Renderer::Mosaic(
                MosaicTiler::builder(source)
                    .block_cache(cache.clone())
                    .request_limiter(limit.clone())
                    .http_policy(HttpAccessPolicy::permissive())
                    .build(),
            )
        } else {
            let reader = CogReader::builder()
                .block_cache(cache.clone())
                .open_http(&format!("{base}/0.tif"))
                .await
                .unwrap();
            Renderer::Single(Tiler::new(Arc::new(reader)).with_request_limiter(limit.clone()))
        });
        http.delay_ms.store(100, Ordering::Relaxed);
        let before = http.snapshot();
        let cancellation = RequestCancellation::default();
        let mut jobs = JoinSet::new();
        for _ in 0..8 {
            let renderer = renderer.clone();
            let options = RequestOptions {
                timeout: Some(Duration::from_millis(if deadline { 10 } else { 1000 })),
                cancellation: cancellation.clone(),
            };
            jobs.spawn(async move {
                let started = Instant::now();
                let result = renderer
                    .tile_with_options(coord(), &TileStyle::default(), &options)
                    .await;
                let outcome = match result {
                    Err(async_geotiff::error::AgError::Overloaded) => "overloaded",
                    Err(async_geotiff::error::AgError::Cancelled) => "cancelled",
                    Err(async_geotiff::error::AgError::DeadlineExceeded) => "deadline",
                    other => panic!("unexpected controlled-stop result: {other:?}"),
                };
                json!({"outcome":outcome,"elapsed_ms":started.elapsed().as_secs_f64()*1000.0})
            });
        }
        if !deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancellation.cancel();
        }
        let mut outcomes = Vec::new();
        while let Some(result) = jobs.join_next().await {
            outcomes.push(result.unwrap());
        }
        let at_return = http.snapshot();
        let permits_at_return = limit.available_permits();
        let work_at_return = work_json();
        let cache_at_return = cache_json(&cache);
        tokio::time::sleep(Duration::from_millis(250)).await;
        cache.sync_count().await;
        http.delay_ms.store(2, Ordering::Relaxed);
        assert_eq!(
            limit.available_permits(),
            2,
            "admission permits must return after cancellation settles"
        );
        json!({"scenario":format!("{}_{}",if deadline {"deadline"}else{"cancel_handle"},if mosaic {"cold_mosaic"}else{"single_read"}),"concurrency":8,"admission_limit":2,
            "server_delay_ms":100,"deadline_ms":if deadline {10}else{1000},"cancel_after_ms":if deadline {None}else{Some(20)},"settle_ms":250,
            "outcomes":outcomes,"http_before":before,"http_at_return":at_return,"http_after_settle":http.snapshot(),
            "permits_at_return":permits_at_return,"permits_after_settle":limit.available_permits(),"work_at_return":work_at_return,"work_after_settle":work_json(),
            "cache_at_return":cache_at_return,"cache_after_settle":cache_json(&cache)})
    }

    async fn memory_rejection(base: &str, http: Arc<HttpMetrics>) -> Value {
        let budget = WorkBudget::new(64 * 1024);
        let cache = Arc::new(BlockCache::new(CacheConfig::default()));
        let reader = CogReader::builder()
            .block_cache(cache.clone())
            .open_http(&format!("{base}/0.tif"))
            .await
            .unwrap();
        let renderer = Arc::new(Tiler::new(Arc::new(reader)).with_work_budget(budget.clone()));
        let before = http.snapshot();
        let mut jobs = JoinSet::new();
        for _ in 0..8 {
            let renderer = renderer.clone();
            jobs.spawn(async move {
                let style = TileStyle::from_query("tilesize=1024&rescale=0,255").unwrap();
                assert!(matches!(
                    renderer.tile(coord(), &style).await,
                    Err(async_geotiff::error::AgError::ResourceLimit(_))
                ));
            });
        }
        while let Some(result) = jobs.join_next().await {
            result.unwrap();
        }
        assert_eq!(budget.used_bytes(), 0);
        assert_eq!(
            http.started.load(Ordering::Relaxed),
            before["get_started"].as_u64().unwrap(),
            "memory rejection must precede pixel GETs"
        );
        json!({"scenario":"memory_budget_rejection","concurrency":8,"tile_size":1024,"resource_limit_errors":8,
            "budget_limit_bytes":budget.limit_bytes(),"budget_peak_bytes":budget.peak_bytes(),"budget_used_after":budget.used_bytes(),
            "http_before":before,"http_after":http.snapshot(),"cache_after":cache_json(&cache)})
    }

    pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
        let samples = std::env::args()
            .nth(1)
            .map(|s| s.parse::<usize>())
            .transpose()?
            .unwrap_or(32)
            .clamp(4, 128);
        let concurrency = std::env::args()
            .nth(2)
            .map(|s| s.parse::<usize>())
            .transpose()?
            .unwrap_or(4)
            .clamp(1, 16);
        let http = Arc::new(HttpMetrics::default());
        http.delay_ms.store(2, Ordering::Relaxed);
        let app = Router::new()
            .route("/{asset}", get(serve))
            .with_state(http.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut results = Vec::new();
        for (name, mosaic, cold) in [
            ("single_cold", false, true),
            ("single_warm", false, false),
            ("mosaic_cold", true, true),
            ("mosaic_warm", true, false),
        ] {
            results.push(
                scenario(
                    &base,
                    http.clone(),
                    name,
                    mosaic,
                    cold,
                    samples,
                    concurrency,
                )
                .await,
            );
        }
        results.push(cancellation(&base, http.clone(), concurrency).await);
        for mosaic in [false, true] {
            results.push(controlled_stop(&base, http.clone(), false, mosaic).await);
            results.push(controlled_stop(&base, http.clone(), true, mosaic).await);
        }
        results.push(memory_rejection(&base, http).await);
        server.abort();
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"fixture":"rgb_deflate.tif","fixture_bytes":TIFF.len(),"runtime_worker_threads":4,"server_delay_ms":2,"results":results})
            )?
        );
        Ok(())
    }
}
