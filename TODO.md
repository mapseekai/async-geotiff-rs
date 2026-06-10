# TODO

## Recently completed for 0.0.8

- Raised the default decoded block cache budget to 512 MB and documented when
  to use 1024 MB for larger browsing workloads.
- Added foreground block single-flight retry behavior so waiter requests recover
  when the owner request is canceled.
- Optimized `read_window` stitching and fused CPU handoffs in the tile pipeline.
- Added U16/common render LUT fast paths; keep the broader LUT parity checklist
  below for remaining dtype/style coverage.
- Added reproducible tile benchmark notes under `docs/` and `scripts/`.

## Highest-value performance optimization backlog

Context from the latest two tile profiling runs:

- Latest tile runs still showed tile P50 around 100ms because block miss decode,
  `read_window` stitching, and WebP encoding dominated.
- Cache hit path is already very fast: full-hit `blocks_for` is around
  0.02ms P50. The highest leverage work is improving hit rate and reducing
  miss/stitch/encode cost.

### P0 - Increase decoded block cache and improve hit rate

- Raise default or deployment value from 256MB to at least 512MB; use 1024MB
  for interactive browsing tests when memory allows.
- Keep measuring `full_hit`, `any_miss`, `all_miss`, and `pending` separately.
- Code touchpoints:
  - `src/io/cache.rs`: `CacheConfig::default()`.
  - `examples/serve.rs`: `ASYNC_GEOTIFF_BLOCK_CACHE_MB` runtime tuning.
  - `src/io/cog.rs`: `blocks_for` cache probe and single-flight path.
- Success metric:
  - Warm viewport `blocks_for` full-hit ratio increases.
  - `read_window_ms` P50/P95 drops primarily through lower `blocks_ms`.

### P0 - Add low-priority neighbor block prefetch

- Use foreground tile block coordinates to prefetch likely next blocks for pan
  and viewport movement.
- Reuse `BlockCache::claim` so prefetch never duplicates foreground decode.
- Keep prefetch disabled by default until validated.
- Start with Data blocks only, radius 1, max 2-4 extra blocks per foreground
  tile, and one low-priority prefetch worker.
- Code touchpoints:
  - `src/io/cog.rs`: after `read_window` has computed native block coords.
  - `src/io/cog.rs`: reuse `blocks_for`/`BlockCache::claim` behavior.
  - Add separate low-priority semaphore so prefetch cannot starve visible tiles.
- Success metric:
  - `prefetch_later_hit` is high enough to justify extra CPU.
  - Foreground `decode_ms` and `blocks_ms` P95 improve under pan tests.

### P1 - Optimize `read_window` data stitching

- Latest 3857 WebP run still showed `data_stitch_ms` around 29ms P50 and
  above 50ms P95, so stitching is now a first-class bottleneck.
- Add specialized fast paths for common cases:
  - one native block: avoid unnecessary copy where possible.
  - two/four native blocks: use direct contiguous slice copies.
  - common pixel layouts: `u8/u16`, 3-band or 4-band, no mask.
- Code touchpoints:
  - `src/io/cog.rs`: `read_window` data stitch block.
  - `src/io/window.rs`: `stitch_with_info`.
- Success metric:
  - `data_stitch_ms` P50 drops materially on the 3857 viewport benchmark.

### P1 - Reduce WebP/JPEG/PNG encode cost

- WebP encode is consistently around 30-35ms P50 and 55-60ms P95 in the latest
 runs.
- Expose WebP speed/quality tuning, and prefer JPEG for opaque RGB imagery when
  transparency is not required.
- Code touchpoints:
  - `src/render/encode.rs`: `encode`, `encode_webp`, `encode_jpeg`,
    `encode_png`.
- Success metric:
  - WebP encode P95 no longer dominates hot-cache tiles.

### P1 - Add render LUT fast paths for stretch/color ramp

- Hot-cache Samply profiles showed render/style and pixel conversion becoming
  important once decode is removed.
- Precompute LUTs for common integer data paths instead of converting every
  tile pixel through generic `f64` arrays.
- Code touchpoints:
  - `src/render/pipeline.rs`: `render`, `render_single_band`,
    `render_three_band`.
  - `src/render/colormap.rs`: existing LUT construction.
  - `src/render/composite.rs`: composite LUT use.
- Success metric:
  - Hot-cache profile samples in `render::pipeline::render`,
    `PixelData::to_f64`, and `ndarray::Zip::for_each` decrease.

## COG block / tile prefetch optimization

Context: Samply profiling shows CPU hotspots dominated by COG block ZSTD decode. The current decoded block cache and single-flight path already avoid duplicate concurrent decode for the same block, so the next optimization should focus on prewarming likely future cache entries.

### 1. Frontend XYZ tile prefetch

- Evaluate a MapLibre-side tile preloader similar to `AbelVM/maplibre-preload`.
- Use it to preload tiles for targeted map movement such as `flyTo`, `panTo`, `easeTo`, and `zoomTo`.
- Start conservatively for the dynamic COG service:
  - `async: false`
  - `burstLimit: 30-60`
  - `useTile: true` when possible
- Goal: indirectly warm the backend decoded block cache through normal tile requests.

### 2. Backend COG block prefetch

- Add optional low-priority block prefetch after a foreground tile finishes resolving its required blocks.
- Reuse the existing `BlockCache::claim` / single-flight path. Do not create a separate path that can decode the same block twice.
- First MVP strategy:
  - prefetch only Data blocks initially;
  - radius `1`;
  - max `2-4` extra blocks per foreground tile;
  - prefer right, down, and right-down neighbors first;
  - skip prefetch if the block is already cached or in flight;
  - skip prefetch if no prefetch permit is immediately available.
- Add a separate low-priority semaphore so prefetch never starves foreground tile work.

Suggested environment switches:

```bash
ASYNC_GEOTIFF_BLOCK_PREFETCH=1
ASYNC_GEOTIFF_BLOCK_PREFETCH_RADIUS=1
ASYNC_GEOTIFF_BLOCK_PREFETCH_MAX_PER_TILE=4
ASYNC_GEOTIFF_BLOCK_PREFETCH_CONCURRENCY=1
```

### 3. Metrics and validation

- Track:
  - `prefetch_scheduled`
  - `prefetch_skipped_cache_hit`
  - `prefetch_skipped_inflight`
  - `prefetch_skipped_no_permit`
  - `prefetch_decoded`
  - `prefetch_later_hit`
- Compare before/after on the same pan/zoom workload:
  - decoded block cache hit ratio
  - foreground `decode_ms`
  - foreground `read_window_ms`
  - tile P50/P95/P99
  - CPU utilization
- Treat `prefetch_later_hit` as the key success metric. If prefetch work rarely becomes later foreground hits, disable or tighten the strategy.

### 4. Guardrails

- Keep prefetch disabled by default until validated.
- Do not let prefetch wait behind foreground decode work for a long time; stale prefetch should be droppable.
- Avoid large default burst values from frontend preloaders; dynamic COG tiles can saturate ZSTD decode CPU quickly.
- Re-test with `tokio-console` disabled for clean CPU numbers, because console/tracing overhead is visible in Samply profiles.

## Render LUT fast paths

Context: After block decode/cache improvements, render CPU may become more visible. Current render paths can convert selected bands to `f64` and perform per-pixel stretch/color operations. The project already has a 256-entry colormap LUT (`0..255 -> RGBA`); the next step is to precompute `raw pixel value -> display value` or `raw pixel value -> RGBA` for common integer data types.

### 1. Target fast paths

- Add LUT paths only for high-frequency, low-risk cases first:
  - `U8`, `U16`, and `I16` pixel data;
  - no expression;
  - no complex color formula;
  - fixed `rescale`;
  - single-band colormap;
  - three-band RGB composite.
- Keep `U32`, `I32`, `F32`, and `F64` on the existing generic path initially.

### 2. LUT shapes

- Single-band colormap:
  - `U8`: `[u8; 256] -> [[u8; 4]; 256]`
  - `U16`: `Vec<[u8; 4]>` with `65536` entries
  - `I16`: `Vec<[u8; 4]>` with `65536` entries, indexed by `(raw as i32 - i16::MIN as i32)`
- RGB stretch:
  - per-band `raw -> u8` LUT;
  - `U16` RGB uses roughly `3 * 64KB` per compiled style;
  - write RGBA directly from lookup results.

### 3. Implementation sketch

- Add `src/render/lut.rs`.
- Add a compiled LUT field to `CompiledTileStyle`, for example:

```rust
pub fast_lut: Option<CompiledRenderLut>
```

- Add a render fast path after the existing `try_u8_rgb_fastpath`:

```rust
if let Some(rgba) = try_lut_fastpath(image, style, auto_rgb, h, w)? {
    return encode(&rgba, style.format);
}
```

- Preserve the current generic render path as fallback for unsupported styles or data types.

### 4. Cache key and invalidation

- Build/cache LUTs by:
  - dtype;
  - `rescale` low/high values;
  - colormap name or user stops;
  - nodata handling;
  - composite mode;
  - `bidx`.
- Avoid rebuilding LUTs per tile when style parameters are identical.

### 5. Validation

- Compare before/after on hot-cache workloads where decode is not dominant:
  - `render::pipeline::render` samples in Samply;
  - `PixelData::to_f64` samples;
  - `ndarray::Zip::for_each` samples;
  - `render_encode_ms`;
  - tile P50/P95/P99.
- Add correctness tests for:
  - clamping below/above `rescale`;
  - `U16` single-band colormap parity with generic path;
  - `I16` indexing offset;
  - RGB stretch parity with generic path;
  - nodata/mask alpha behavior.
