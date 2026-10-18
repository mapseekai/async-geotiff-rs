---
title: Example HTTP API
description: Routes, tile path format, style parameters, and response behavior.
weight: 10
---

The HTTP API belongs to `examples/serve.rs`; it is not part of the library’s
dependency surface.

## Routes {#routes}

| Method and path | Purpose |
| --- | --- |
| `GET /tiles/{tms}/{z}/{x}/{y}.{ext}` | Render a tile from the selected single COG. |
| `GET /mosaic/tiles/{tms}/{z}/{x}/{y}.{ext}` | Render a tile from the configured mosaic or STAC source. |
| `GET /datasets` | List local TIFF files discovered under `data/`. |
| `GET /cache/stats` | Return decoded-block cache statistics. |
| `GET /formats` | Return output formats compiled into the server. |
| `GET /bounds` | Return bounds used by the example viewers. |
| `GET`, `HEAD /data/{name}` | Serve a discovered local TIFF with range support. |

Supported TMS names are `WebMercatorQuad` and `WorldCRS84Quad`. Supported
style extensions are `png` and feature-gated lossless `webp`.

## Raster Style Q3 {#parameters}

Both tile routes require complete Q3: `version=3.0&type=...`. Unknown fields,
old parameters, and unimplemented capabilities fail explicitly. `dataset`,
`mode`, and `_v` are separate singleton context keys; asset URLs are not styles.

| Parameter | Repeat | Meaning |
| --- | --- | --- |
| `version` / `type` | no | `3.0` and gray/rgb/pseudocolor/categorized/single_color. |
| `bidx` | yes | One-based source bands; RGB requires exactly three. |
| `expression` | no | One expression for gray, three separated by semicolons for RGB; excludes bidx. |
| `method` / `rescale` | ranges only | none, linear, minmax, percentile, stddev, histogram_equalization, curve; see the styling guide for parameters and the shared statistics grid. |
| `colormap_name` / `colormap` | no | Pseudocolor maps; native tables use exact values or half-open intervals. |
| `nodata` | no | Finite number or nan. |
| `color_formula` | no | Supported Gamma operations, applied in order. |
| `opacity` | no | 0–1; default 1. |
| `resampling` / `reproject` | no | nearest, bilinear, cubic, cubic_spline, lanczos, average. |
| `extensions` | no | JSON array of exact versioned Schema URLs; percent-encode the value. |
| `extension_fields` | no | JSON object containing `raster:tile` and/or `raster:mosaic`; percent-encode the value. |

`raster:tile` supports `tilesize` (64, 128, 256, 512, 1024), `format` (`png`,
or `webp` with `lossless: true`); the format must match the path. `raster:mosaic`
requires `pixel_selection` and `stage`, with optional `rank_channel` for highest/lowest.
Declare the [Tile 1.0.0](https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json)
and/or [Mosaic 1.0.0](https://mapseekai.github.io/raster-style/extensions/raster/mosaic/v1.0.0/schema.json)
Schema URLs. The former tile/mosaic short parameters are rejected.

Encode literal `+` as `%2B` in raw Q3. Do not decode before concatenating query
fragments. See the [styling guide](../../guides/styling/) for semantics and limits.

## Responses {#responses}

- `200 OK` returns encoded image bytes.
- `204 No Content` means the coordinate is valid but fully outside the source
  dataset or projection domain.
- Invalid style values and malformed coordinates return a client error with a
  structured library error behind it.

The default output size is 256×256, chosen for broad XYZ-client compatibility.
