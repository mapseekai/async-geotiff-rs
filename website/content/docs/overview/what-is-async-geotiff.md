---
title: What is async-geotiff?
description: The project’s purpose, supported data, and deliberate boundaries.
weight: 10
---

`async-geotiff` is a Rust library for turning Cloud-Optimized GeoTIFFs (COGs)
into styled XYZ map tiles. It reads only the byte ranges needed for a tile,
decodes native TIFF blocks, optionally reprojects them, applies a rendering
style, and encodes the result.

## Core capabilities {#capabilities}

- Async COG access over HTTPS or any caller-configured `object_store` backend.
- Shared, byte-bounded caching of decoded native TIFF blocks.
- `WebMercatorQuad` (EPSG:3857) and `WorldCRS84Quad` (EPSG:4326) tile grids.
- U8, U16, I16, U32, I32, F32, and F64 source pixels.
- Nearest, bilinear, cubic, cubic-spline, Lanczos, and average resampling.
- Rescaling, colormaps, RGB composition, color formulas, and arithmetic band math.
- PNG output and feature-gated lossless WebP output.
- Multi-asset mosaics backed by MosaicJSON, STAC, or a custom source.

## What the library deliberately does not own {#boundaries}

**No GDAL runtime.** TIFF I/O uses `async-tiff` and `object_store`. The default
`proj` feature uses the system `libproj` library only for coordinate
transformations that do not have a built-in fast path.

**No web framework in the library.** The Axum HTTP layer and MapLibre,
OpenLayers, and Leaflet viewers live under `examples/`. Applications can use
`CogReader`, `Tiler`, or `MosaicTiler` without inheriting Axum.

**No credentials manager.** When opening S3, GCS, MinIO, or another object
store, the application configures authentication on the store and passes the
ready object to the library.

**No telemetry backend.** The crate emits `tracing` spans and exposes cache
statistics. The application decides whether to export them to OpenTelemetry,
Prometheus, logs, or another system.

> [!NOTE]
> A tile fully outside the dataset or projection domain returns `Ok(None)`.
> The example HTTP server maps that result to `204 No Content`.
