---
title: Production operation
description: Size caches, bound CPU work, secure remote assets, and export observability.
weight: 30
---

## Share one cache {#cache}

The default decoded-block cache budget is 512 MiB. A server with many readers
should inject the same `Arc<BlockCache>` into every reader and mosaic. Give
each distinct COG a stable source ID so cache entries cannot collide.

This budget covers resident decoded blocks in the cache, not total process
RSS. In-flight compressed bytes, decode and warp buffers, encoded responses,
reader metadata, and mosaic reducer state are additional memory. In particular,
`Median` retains every contributing image until reduction completes.

Default tile entrypoints share a `RequestLimiter` admitting 64 requests and a
512 MiB `WorkBudget` for active work, separate from decoded-cache residency.
Custom limiters and budgets can be injected. These accounting limits are not
a hard RSS ceiling: keep per-tile asset fan-out and retained output buffers
bounded, and size containers using representative load tests. See
[runtime limits and source consistency](/docs/guides/runtime-limits/) for defaults,
bounded memory-pressure retries, cancellation and pinned source snapshots.

Use `BlockCache::stats()` to observe:

- `hits` and `misses`
- `decoded` and `fetched`
- `entries`
- `resident_bytes`

Useful derived signals include hit ratio, fetched-to-decoded amplification,
and resident bytes divided by the configured budget.

## Bound CPU work {#cpu}

Decode, stitch, warp, mosaic reduction, render, and encode are CPU-heavy.
They run in `spawn_blocking` behind `CpuLimiter`. Non-mosaic `CogReader` and
`Tiler` instances accept a caller-owned limiter. `MosaicTiler` shares one
internal limiter across its assets and final render, but does not accept the
non-mosaic limiter. Defaults follow the host's available parallelism and clamp
zero to one permit.

`Tiler::new` reuses the limiter carried by its reader:

```rust
use std::sync::Arc;

use async_geotiff::CpuLimiter;
use async_geotiff::io::cog::CogReader;
use async_geotiff::tiler::Tiler;

let cpu = CpuLimiter::new(8);
let reader = Arc::new(
    CogReader::builder()
        .source_id("scene-a")
        .cpu_limiter(cpu.clone())
        .open_http("https://example.com/a.tif")
        .await?,
);
let tiler = Tiler::new(reader);
```

The example server's CPU accounting depends on the path. A command-line default
COG uses separate reader-decode and tiler-render limiters, so those phases can
overlap. Lazily selected local datasets reuse one `AppState` limiter sized to
the larger configured value. `MosaicTiler` owns another shared limiter. Treat
the two environment values as component tuning, not as a universal
process-wide CPU cap. For non-mosaic readers and tilers, wire one
library-level `CpuLimiter` through every component that supports injection.

Current `MosaicTiler` owns an internal shared limiter and does not expose a
builder hook for injecting the non-mosaic limiter. A strict numeric CPU-job cap
across mixed mosaic and single-COG traffic is therefore unsupported today.
External request concurrency provides useful coarse backpressure, but is not
equivalent to one shared permit budget.

Do not lower tile concurrency preemptively. Measure P95 latency and CPU
contention first; an unnecessarily small limit adds queueing to every tile.

## Tune access patterns {#access-patterns}

- Increase cache capacity for multi-dataset servers or wide pan/zoom sessions.
- Prefer ZSTD-compressed COGs over DEFLATE when read latency matters.
- The example server prefetches a one-block ring by default. Set
  `ASYNC_GEOTIFF_PREFETCH_RING=0` when extra object-store GETs are undesirable.
- Keep `MosaicConfig::max_assets_per_tile` bounded for untrusted indexes.

## Export traces and metrics {#observability}

`Tiler::tile` and `CogReader::read_window` emit `tracing` spans. Connect a
`tracing-opentelemetry` layer in the application when OTLP traces are needed.
The library intentionally takes no OpenTelemetry dependency.

Record request count, latency, error count, and the `Ok(None)` rate at the HTTP
or application boundary. Poll cache statistics separately for memory and
fetch behavior.

## Protect remote access {#security}

Use a strict `HttpAccessPolicy` when mosaic or STAC documents are not trusted.
Configure credentials on an `object_store` instance rather than embedding them
in asset identifiers. Avoid logging raw presigned URLs at application
boundaries outside the library.
