# Tile Performance Benchmark - 2026-06-07

This note summarizes the local profiling and benchmark pass for the example
tile server. The goal was to separate decoded-block cache behavior, output
format cost, decode concurrency, and CPU profile hotspots.

## Environment

- Machine: local macOS development machine
- Date: 2026-06-07
- Binary: `target/profiling/examples/serve`
- Build profile: `profiling` profile, optimized with debuginfo
- Features: `webp,jpeg`
- Tokio console: disabled for benchmark and Samply CPU profiles
- Logging: `RUST_LOG=async_geotiff=warn`
- Dataset:
  `SVN3-01_20250814_L2D0000203064_01_6112500050210001-FUS_3857_cog.tif`
- Tile set:
  - TMS: `WebMercatorQuad`
  - z: `14`
  - x: `13652..13667`
  - y: `6182..6198`
  - total requests per run: `272`
- Client request concurrency: `8`

Base server command shape:

```bash
ASYNC_GEOTIFF_BIND=127.0.0.1:<PORT> \
ASYNC_GEOTIFF_BLOCK_CACHE_MB=<MB> \
ASYNC_GEOTIFF_MAX_DECODE_TASKS=<N> \
RUST_LOG=async_geotiff=warn \
target/profiling/examples/serve
```

`ASYNC_GEOTIFF_MAX_TILE_TASKS` is not set here: it defaults to
`available_parallelism()` (10 on this machine) which was found to be
near-optimal. Pinning it to 4 increases P95 at high concurrency due to
scheduling queuing.

## Cache Capacity Results

These runs used PNG output, `ASYNC_GEOTIFF_MAX_DECODE_TASKS=4`, and
`ASYNC_GEOTIFF_MAX_TILE_TASKS=4`.

| Scenario | Cache | P50 | P95 | P99 | Max | Decoded blocks | Result |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| cold PNG | 1024 MB | 7.04 ms | 11.99 ms | 99.73 ms | 100.35 ms | 211 | Cold path has a small long tail |
| warm PNG | 1024 MB | 2.63 ms | 5.19 ms | 14.09 ms | 14.75 ms | 211 | Hot cache is very fast |
| cold PNG | 512 MB | 7.19 ms | 10.81 ms | 100.98 ms | 101.61 ms | 211 | Similar to 1024 MB for this viewport |
| warm PNG | 512 MB | 2.84 ms | 4.80 ms | 5.81 ms | 7.11 ms | 211 | 512 MB holds this viewport working set |
| cold PNG | 256 MB | 7.00 ms | 16.44 ms | 102.10 ms | 102.75 ms | 211 | Eviction starts to matter |
| warm PNG | 256 MB | 7.56 ms | 13.71 ms | 15.32 ms | 16.80 ms | 455 | Second run still re-decodes blocks |

Observed resident decoded block size for this viewport:

```text
resident_bytes=331874304
```

Interpretation:

- `256 MB` is not enough for this viewport. The warm run increased
  `decoded` from `211` to `455`, proving that blocks were evicted and decoded
  again.
- `512 MB` is enough for this exact working set. Warm requests did not increase
  `decoded` or `fetched`.
- A conservative default for interactive browsing should be at least `512 MB`.
  `1024 MB` gives more headroom for larger viewports, multiple datasets, or
  pan/zoom sequences.

## Output Format Results

These runs used hot decoded-block cache with `512 MB+` effective capacity.

| Format | P50 | P95 | P99 | Average bytes | P50 bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| PNG | 3.37 ms | 5.19 ms | 6.84 ms | 186 KB | 246 KB |
| JPEG | 3.22 ms | 5.25 ms | 23.22 ms | 13 KB | 16 KB |
| WebP | 2.81 ms | 4.49 ms | 5.01 ms | 31 KB | 47 KB |

Interpretation:

- Hot-cache server-side latency is low for all three formats.
- PNG transfer size is much larger for this imagery.
- JPEG has the smallest payload, but showed a higher P99 in this run.
- WebP had the best combined latency and size profile in this run.
- For imagery browsing, prefer WebP or JPEG by default. Keep PNG for
  transparency-sensitive or categorical/classified tiles.

## Decode Concurrency Matrix

These runs used `ASYNC_GEOTIFF_BLOCK_CACHE_MB=512`,
`ASYNC_GEOTIFF_MAX_TILE_TASKS=4`, PNG output, and request concurrency `8`.

| Decode tasks | Cold P50 | Cold P95 | Cold P99 | Warm P50 | Warm P95 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 2 | 9.62 ms | 16.35 ms | 115.30 ms | 3.36 ms | 6.43 ms |
| 4 | 11.06 ms | 18.10 ms | 172.24 ms | 3.90 ms | 6.35 ms |
| 6 | 8.91 ms | 13.09 ms | 140.76 ms | 3.44 ms | 6.42 ms |
| 8 | 9.38 ms | 12.98 ms | 132.13 ms | 4.22 ms | 8.99 ms |

Interpretation:

- `2` decode tasks under-utilizes the cold path.
- `6` and `8` gave the best cold P95 in this workload.
- `8` slightly hurt warm P95, likely due to extra scheduling/CPU contention.
- Recommended starting point:

```bash
ASYNC_GEOTIFF_BLOCK_CACHE_MB=512
ASYNC_GEOTIFF_MAX_DECODE_TASKS=6
ASYNC_GEOTIFF_MAX_TILE_TASKS=4
```

Use `1024 MB` cache when the deployment has enough memory or when multiple
active viewports/datasets are expected.

## Samply CPU Profiles

Two Samply profiles were captured without `tokio-console`.

```text
/tmp/async-geotiff-clean.json.gz
/tmp/async-geotiff-decode-pressure.json.gz
```

### Clean Mixed Profile

Workload: one cold run followed by nineteen hot runs, with enough cache to hold
the decoded block working set.

Top 250 serve hotspots:

| Category | Share |
| --- | ---: |
| render/style/pixel conversion | 45.4% |
| PNG encode/compress | 20.0% |
| warp/resample/resize | 10.2% |
| COG block decode / ZSTD | 6.8% |

Interpretation:

- Once decoded blocks are hot, ZSTD decode is no longer the primary bottleneck.
- Render fast paths, LUT-based stretch/color mapping, and output format changes
  become more important in hot-cache browsing.

### Decode Pressure Profile

Workload: `64 MB` cache, repeated viewport requests, forcing eviction and
re-decode.

Cache stats:

```json
{"decoded":3708,"entries":42,"fetched":3712,"hits":1746,"misses":7245,"resident_bytes":66060288}
```

Top 250 serve hotspots:

| Category | Share |
| --- | ---: |
| COG block decode / ZSTD | 69.6% |
| render/style/pixel conversion | 12.9% |
| PNG encode/compress | 4.9% |
| warp/resample/resize | 2.4% |

Interpretation:

- With too little cache or cold/miss-heavy access, ZSTD block decode dominates.
- Cache capacity and prefetch strategy are the highest-leverage optimizations
  for miss-heavy browsing.

## Detailed Cold vs Warm Stage Timing

This section uses a DEBUG-instrumented run to break down each tile stage. The
absolute values include tracing/logging overhead and should be used for stage
attribution, not as the final production latency number.

Run settings:

```text
requests: 272 PNG tiles
cache: 512 MB
decode tasks: 6
tile tasks: 4
request concurrency: 8
log level: async_geotiff=debug
summary JSON: /tmp/async-geotiff-stage-detail-summary-by-order.json
raw log: /tmp/async-geotiff-stage-detail.log
```

Because this binary log did not include `tile_id` in spans, phases were split by
log order: the first 272 tile-level timing rows are cold, and the next 272 are
warm. One extra `read_window`/`blocks_for` row was ignored by this split.

### Client And Cache State

| Phase | Wall | Client P50 | Client Avg | Client P95 | Client P99 | Decoded | Hits | Misses | Resident bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| cold | 364.51 ms | 8.137 ms | 10.582 ms | 13.195 ms | 106.584 ms | 211 | 487 | 413 | 331874304 |
| warm | 123.93 ms | 3.359 ms | 3.568 ms | 6.046 ms | 7.097 ms | 211 | 1386 | 413 | 331874304 |

The warm run did not increase `decoded` or `misses`, which means the 512 MB
cache held the full decoded-block working set for this viewport.

### Tile Internal Stages

| Stage | Phase | n | P50 | Avg | P90 | P95 | P99 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| tile total | cold | 272 | 7.054 ms | 6.651 ms | 10.040 ms | 10.628 ms | 13.537 ms |
| tile total | warm | 272 | 2.063 ms | 2.225 ms | 3.179 ms | 4.257 ms | 5.325 ms |
| warp_tile | cold | 272 | 6.150 ms | 5.813 ms | 8.739 ms | 9.437 ms | 12.701 ms |
| warp_tile | warm | 272 | 1.366 ms | 1.505 ms | 2.335 ms | 2.759 ms | 3.751 ms |
| style | cold | 272 | 0.005 ms | 0.121 ms | 0.014 ms | 0.025 ms | 4.267 ms |
| style | warm | 272 | 0.004 ms | 0.006 ms | 0.012 ms | 0.013 ms | 0.026 ms |
| render_encode | cold | 272 | 0.680 ms | 0.717 ms | 1.139 ms | 1.272 ms | 1.452 ms |
| render_encode | warm | 272 | 0.646 ms | 0.714 ms | 1.187 ms | 1.341 ms | 2.069 ms |

Interpretation:

- Decoded-block cache primarily reduces `warp_tile`, because `warp_tile`
  contains `read_window`.
- `style` and `render_encode` do not materially improve with decoded-block
  cache. They are separate optimization targets, for example LUT fast paths and
  WebP/JPEG output.

### Warp And Read Window

| Stage | Phase | n | P50 | Avg | P90 | P95 | P99 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| warp total | cold | 272 | 6.140 ms | 5.783 ms | 8.720 ms | 9.399 ms | 12.697 ms |
| warp total | warm | 272 | 1.335 ms | 1.475 ms | 2.288 ms | 2.687 ms | 3.746 ms |
| read_window inside warp | cold | 272 | 5.482 ms | 4.819 ms | 8.083 ms | 8.710 ms | 11.687 ms |
| read_window inside warp | warm | 272 | 0.550 ms | 0.597 ms | 0.956 ms | 1.053 ms | 1.316 ms |
| warp_cpu | cold | 272 | 0.765 ms | 0.956 ms | 1.763 ms | 2.264 ms | 3.438 ms |
| warp_cpu | warm | 272 | 0.659 ms | 0.873 ms | 1.647 ms | 2.290 ms | 3.026 ms |

Interpretation:

- The main cache win is `read_window`: P50 drops from `5.482 ms` to
  `0.550 ms`.
- `warp_cpu` remains in the same range because cache does not eliminate
  resampling/reprojection work.

### Read Window Internals

| Stage | Phase | n | P50 | Avg | P90 | P95 | P99 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| read_window total | cold | 272 | 5.441 ms | 4.793 ms | 8.056 ms | 8.680 ms | 11.502 ms |
| read_window total | warm | 272 | 0.526 ms | 0.570 ms | 0.917 ms | 1.016 ms | 1.266 ms |
| blocks_ms | cold | 272 | 4.750 ms | 4.189 ms | 7.463 ms | 7.707 ms | 10.483 ms |
| blocks_ms | warm | 272 | 0.020 ms | 0.032 ms | 0.067 ms | 0.093 ms | 0.176 ms |
| data_stitch | cold | 272 | 0.564 ms | 0.603 ms | 0.974 ms | 1.104 ms | 1.281 ms |
| data_stitch | warm | 272 | 0.482 ms | 0.537 ms | 0.876 ms | 0.988 ms | 1.258 ms |
| assemble | cold | 272 | 0.001 ms | 0.001 ms | 0.002 ms | 0.002 ms | 0.006 ms |
| assemble | warm | 272 | 0.000 ms | 0.001 ms | 0.001 ms | 0.001 ms | 0.002 ms |

Interpretation:

- `blocks_ms` is the direct cache-sensitive part. It drops from `4.750 ms` P50
  to `0.020 ms` P50.
- `data_stitch` still runs in the warm path because cached blocks still need to
  be copied/stiched into the requested window.

### Block Fetch, Decode, And Cache

| Stage | Phase | n | P50 | Avg | P90 | P95 | P99 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| blocks_for total | cold | 272 | 4.745 ms | 4.163 ms | 7.361 ms | 7.703 ms | 10.479 ms |
| blocks_for total | warm | 272 | 0.003 ms | 0.004 ms | 0.006 ms | 0.009 ms | 0.023 ms |
| cache_probe | cold | 272 | 0.002 ms | 0.004 ms | 0.008 ms | 0.014 ms | 0.028 ms |
| cache_probe | warm | 272 | 0.003 ms | 0.004 ms | 0.006 ms | 0.009 ms | 0.023 ms |
| fetch | cold | 272 | 0.881 ms | 0.924 ms | 1.997 ms | 2.296 ms | 2.557 ms |
| fetch | warm | 272 | 0.000 ms | 0.000 ms | 0.000 ms | 0.000 ms | 0.000 ms |
| decode aggregate | cold | 272 | 1.735 ms | 2.197 ms | 4.972 ms | 5.796 ms | 6.161 ms |
| decode aggregate | warm | 272 | 0.000 ms | 0.000 ms | 0.000 ms | 0.000 ms | 0.000 ms |
| pending_wait | cold | 272 | 0.002 ms | 1.033 ms | 4.613 ms | 5.680 ms | 8.555 ms |
| pending_wait | warm | 272 | 0.000 ms | 0.000 ms | 0.000 ms | 0.000 ms | 0.000 ms |

Block request shape:

| Metric | Phase | n | P50 | Avg | P90 | P95 | P99 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| requested blocks | cold | 272 | 4.000 | 3.309 | 4.000 | 4.000 | 4.000 |
| requested blocks | warm | 272 | 4.000 | 3.305 | 4.000 | 4.000 | 4.000 |
| cache_hits | cold | 272 | 2.000 | 1.918 | 4.000 | 4.000 | 4.000 |
| cache_hits | warm | 272 | 4.000 | 3.305 | 4.000 | 4.000 | 4.000 |
| cache_misses | cold | 272 | 2.000 | 1.518 | 2.000 | 2.000 | 2.000 |
| cache_misses | warm | 272 | 0.000 | 0.000 | 0.000 | 0.000 | 0.000 |

Full cache hit vs any miss:

| Group | Phase | n | P50 | Avg | P90 | P95 | P99 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| full hit | cold | 37 | 0.002 ms | 0.003 ms | 0.004 ms | 0.008 ms | 0.031 ms |
| any miss | cold | 235 | 5.208 ms | 4.818 ms | 7.466 ms | 8.079 ms | 10.579 ms |
| full hit | warm | 272 | 0.003 ms | 0.004 ms | 0.006 ms | 0.009 ms | 0.023 ms |
| any miss | warm | 0 | - | - | - | - | - |

Interpretation:

- Full cache hit makes `blocks_for` essentially free: P50 around `0.002-0.003 ms`.
- Any block miss makes `blocks_for` jump to `5.208 ms` P50 in this run.
- Cold fetch time is smaller than decode/wait time. The miss penalty is mainly
  decode plus pending wait for single-flight work.

### Decode Block Jobs

| Stage | Phase | n | P50 | Avg | P90 | P95 | P99 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| decode_job_ms | cold | 211 | 3.146 ms | 3.057 ms | 5.260 ms | 5.890 ms | 6.514 ms |
| cpu_wait_ms | cold | 211 | 0.000 ms | 0.021 ms | 0.001 ms | 0.162 ms | 0.485 ms |
| decode total | cold | 211 | 3.146 ms | 3.078 ms | 5.290 ms | 5.940 ms | 6.515 ms |
| decode_job_ms | warm | 0 | - | - | - | - | - |

Interpretation:

- `decode_block` only appears in the cold/miss path. In the warm 272 measured
  tile rows, no decode jobs were needed.
- CPU permit wait was low in this run, so decode latency was mostly actual
  block decode work, not queueing.

## Conclusions

1. Decoded block cache capacity is the strongest immediate lever.
   - `256 MB` is too small for the tested viewport.
   - `512 MB` is sufficient for this viewport.
   - `1024 MB` is safer for real browsing patterns.

2. Hot-cache performance is already strong.
   - Warm PNG P95 was around `5 ms` with enough cache.
   - This shifts the next bottleneck to render/style and encoding.

3. Cold/miss-heavy performance is still dominated by ZSTD decode.
   - The decode-pressure profile showed ZSTD at `69.6%` of top hotspots.
   - This supports block cache tuning and frontend/backend prefetch work.

4. PNG is not a good default for continuous imagery browsing.
   - WebP or JPEG should be preferred for RGB imagery tiles.
   - PNG should remain available for transparency/categorical use cases.

5. When creating COGs, prefer ZSTD over DEFLATE.
   - A second dataset (`cog.qiw8xj3u9nvgmzxzrfzt04mr.tif`, DEFLATE-compressed)
     showed block decode at `4.55 ms` P50 vs `1.54 ms` P50 for the ZSTD dataset
     — roughly 3× faster.
   - In miss-heavy workloads, compression codec is the fastest server-side lever
     after cache capacity.
   - Recommended gdal_translate: `-co COMPRESS=ZSTD -co ZSTD_LEVEL=1`.

6. `ASYNC_GEOTIFF_MAX_TILE_TASKS` should be left at its default.
   - The default is `available_parallelism()` (10 on this machine).
   - Pinning it to 4 at `c=64` inflated warp/encode P95 due to scheduling queuing,
     not algorithm slowness. Do not override unless profiling shows CPU contention.

7. A practical starting configuration is:

```bash
ASYNC_GEOTIFF_BLOCK_CACHE_MB=512
ASYNC_GEOTIFF_MAX_DECODE_TASKS=6
RUST_LOG=async_geotiff=warn
```

For production-like browsing tests, prefer:

```bash
ASYNC_GEOTIFF_BLOCK_CACHE_MB=1024
```

## Next Tests

- Re-run the same matrix after frontend XYZ tile preloading is added.
- Re-run the same matrix after backend COG block prefetch is added.
- Add a hot-cache render benchmark after LUT fast paths are implemented.
- Add a format benchmark with representative visual-quality settings for JPEG
  and WebP.
- Repeat against a larger pan/zoom path instead of a single fixed viewport.
