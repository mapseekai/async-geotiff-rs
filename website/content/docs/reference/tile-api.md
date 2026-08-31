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
extensions are `png`, feature-gated `webp`, and feature-gated `jpg`/`jpeg`.

## Style parameters {#parameters}

| Parameter | Repeat | Default or example | Meaning |
| --- | ---: | --- | --- |
| `bidx` | yes | `bidx=1&bidx=2&bidx=3` | 1-indexed source-band selection. |
| `expression` | no | `(b5-b4)/(b5+b4)` | Arithmetic expression producing one derived band. |
| `rescale` | yes | `0,3000` | Explicit per-band `min,max` range; highest stretch priority. |
| `stretch` | no | `minmax`, `percent`, `stddev` | Automatic stretch mode. |
| `pc` | no | `2,98` | Percentile cut used by `stretch=percent`. |
| `sigma` | no | `2.0` | Standard-deviation multiplier. |
| `nodata` | no | `nan`, `inf`, `-inf`, or float | Override the dataset nodata value. |
| `colormap_name` | no | `viridis` | Built-in single-band color ramp. |
| `color_formula` | no | `gamma RGB 1.5` | Supported rio-color operation sequence. |
| `resampling` | no | `nearest` | Source read or resize kernel. |
| `reproject` | no | `nearest` | Warp-time resampling kernel. |
| `tilesize` | no | `256` | One of 64, 128, 256, 512, or 1024. |
| `format` | no | `png` | Optional format assertion; must agree with the path extension. |

Resampling kernels are `nearest`, `bilinear`, `cubic`, `cubic_spline`,
`lanczos`, and `average`. Average computes a box mean for downsampling and
uses nearest behavior when upsampling.

The mosaic tile route additionally accepts `pixel_selection=first`,
`highest`, `lowest`, `mean`, or `median`. The default is `first`.

## Responses {#responses}

- `200 OK` returns encoded image bytes.
- `204 No Content` means the coordinate is valid but fully outside the source
  dataset or projection domain.
- Invalid style values and malformed coordinates return a client error with a
  structured library error behind it.

The default output size is 256×256, chosen for broad XYZ-client compatibility.
