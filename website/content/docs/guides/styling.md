---
title: Styling and band math
description: Configure raster styles with Raster Style 2.0 JSON and Q2 inputs.
weight: 10
---

Inputs use SDK `raster-style = "=0.3.0"`, protocol `2.0`, and Rust 1.92.
The SDK validates structure and semantics; the adapter rejects unsupported
capabilities. Unknown fields and duplicate singleton parameters are errors.
The subset below targets `Tiler::tile` and `MosaicTiler::tile`, not full backend certification.

## Rust inputs {#recipes}

```rust
use async_geotiff::style::TileStyle;

let style = TileStyle::from_json(r#"{
  "version": "2.0",
  "renderer": {"type": "pseudocolor", "bidx": [1], "colormap_name": "viridis"},
  "stretch": {"method": "linear", "rescale": [[0, 3000]]},
  "image": {"format": "png", "tilesize": 256}
}"#)?;
```

Use `TileStyle::from_raster_query` for an original Q2 string, or
`TileStyle::from_raster_style(&RasterStyle)` for an SDK-validated document.
`RasterStyle` is re-exported from `async_geotiff::style`.
`style.raster_style_json()` returns normalized JSON. Reparse the document to
change a style. The parsed style is immutable; format() and tilesize() expose read-only output settings.

## HTTP Q2

Single-band pseudocolor:

```text
/tiles/WebMercatorQuad/0/0/0.png?version=2.0&type=pseudocolor&bidx=1&method=linear&rescale=0,3000&colormap_name=viridis
```

RGB with one range broadcast to all three outputs:

```text
version=2.0&type=rgb&bidx=1&bidx=2&bidx=3&method=linear&rescale=0,3000
```

`dataset`, `mode`, and `_v` belong to the example server, not the style document.
TMS and tile coordinates remain in the path. The SDK decodes raw query fragments
once: encode `+` as `%2B` and spaces as `%20`. Never pre-decode Q2.
JSON is a Rust API entrypoint; the example HTTP service uses Q2 GET requests.
PNG is the default. WebP requires the `webp` feature, `format=webp&lossless=true`,
and a `.webp` path. The path extension must match the style format.

## Stretch and color {#stretch}

- Renderers: `gray`, `rgb`, `pseudocolor`, `categorized`, `single_color`.
- `method=none`, including omission, uses `[0,255]` on display paths and never
  triggers automatic statistics.
- `method=linear` requires `rescale`; RGB accepts one broadcast or three channel ranges.
- Display paths support `range_policy=clamp|transparent`, checked before color operations.
- Named ramps use the project's pinned `colorous` 256-entry tables: viridis,
  magma, inferno, plasma, cividis, turbo, greys.
- Native `colormap` objects use exact data values; interval arrays use half-open
  `[lo,hi)` bounds, including the final interval. With explicit stretch or Gamma,
  pseudocolor uses 0–255 lookup indices; categorized always uses exact data values.
- Gray supports `renderer_invert`; valid zero and negative inputs work with `single_color`.
- `opacity` multiplies output Alpha. Quantized zero Alpha produces transparent black.

Categorized supports explicit exact-value tables and nearest only. It reads the
base level because source overviews lack category-preservation evidence.
Numeric paths support the existing nearest, bilinear, cubic, cubic_spline,
lanczos, and average kernels. Dependencies are resampled independently, preserving
validity. Source Alpha bands are currently rejected before pixel reads.

## Statistical stretch and curves

All seven methods are supported: `none`, `linear`, `minmax`, `percentile`,
`stddev`, `histogram_equalization`, and `curve`. The examples expose their parameters:

```text
method=percentile&percentiles=2,98
method=stddev&stddev=2
method=histogram_equalization
method=curve&curves=[[[0,0],[128,0.5],[255,1]]]
```

Percentiles use linear interpolation of sorted samples; standard deviation is
population deviation. Histogram equalization uses the empirical CDF, without bins.
Curve points are `[data value, normalized output]`; provide one broadcast curve
or three RGB curves. Percent-encode the JSON in an actual Q2 URL.

`Tiler` and `MosaicTiler` bind a fixed **1024 × 1024 EPSG:4326 grid** over the
complete source footprint or union of the fixed mosaic collection. The grid has
north-up rows, equally spaced longitude/latitude centers, and coverage/validity
masks inherited from each source. Overview choice is fixed from each source's full
extent at this grid size, independent of tile zoom. The same upstream resampling,
NoData, expressions, tuple validity and mosaic stages run on that grid. Statistics
never come from the requested tile or unverified embedded GDAL summaries.

Omitted statistics means dataset/mosaic scope, sample accuracy and a maximum of
1,000,000 row-major evenly spaced grid positions. `statistics={"scope":"dataset",
"accuracy":"exact"}` visits all cells **of this bound grid**, not every native
resolution source pixel. NoData samples are discarded without replacement. Empty
statistics give transparent output; a constant range maps its value to 0.5.

Statistics are shared across tiles and cached with reader instance/order and the
full normalized style. CPU work, source reads and retained median inputs use the
existing resource limits. Complete reads of the selected overview can be rejected
when they exceed the work budget. Two cached plans bound retained CDF storage.

MosaicJSON with `quadkey_zoom=0` provides a complete fixed collection for statistics.
Partitioned MosaicJSON statistical stretches fail explicitly: flattening its index
would lose coverage masks and regional paint order. Tile rendering always uses the
index-selected assets. Custom `MosaicSource` implementations must provide
`statistics_assets()` whose full-footprint composite preserves tile selection semantics. Unbound dynamic STAC collections, viewport scope and external
`statistics.ref` snapshots fail explicitly. Low-level `render(ImageData, ...)`
requires a resolved plan for statistical methods; use the tilers to bind it.

## Expressions {#expressions}

Decimal literals, parentheses, unary minus, and arithmetic `+ - * /` are supported.
Band numbers are one-based; RGB expressions separate three outputs with `;`.

```text
version=2.0&type=gray&expression=(b5-b4)%2F(b5%2Bb4)&method=linear&rescale=-1,1
```

This NDVI example requires band 5 to be near-infrared and band 4 red; check the
asset metadata first. Expressions cannot coexist with `bidx`, but may combine
with supported Gamma operations. Invalid inputs, division by zero, and non-finite
results become transparent. Functions, conditionals, and scientific notation
are unsupported. Both the SDK and existing arithmetic parser bound input size.

## Color formulas {#color-formulas}

`effects.color_formula` supports Gamma, evaluated in array order. Q2 example:

```text
version=2.0&type=gray&bidx=1&color_formula=gamma%20rgb%202
```

Unsupported requests fail explicitly: external statistics snapshots, calibration
other than none, index, color_mapping, source palettes, mode resampling, terrain,
extensions, other color operations, JPEG, and lossy WebP.

## Mosaic

With the default `mosaic` feature enabled, declare this JSON member:

```json
"mosaic": {"pixel_selection": "highest", "stage": "after_channels", "rank_channel": 1}
```

Q2 uses `pixel_selection=highest&stage=after_channels&rank_channel=1`.
All five methods (first, highest, lowest, mean, median) support before_channels
and after_channels. Call `mosaic.tile(coord, &style).await?` to take the
selection from the document. `tile_with_options(coord, &style, &options)` adds request cancellation and deadlines.

Before channels, ranking positions refer to deduplicated source dependencies
sorted by source band number; after channels, positions refer to output order.
`rank_channel` is one-based and only applies to highest/lowest. Only complete,
valid tuples from one source participate; channels are never stitched across
sources. Gamma, colormaps, and opacity apply once after merging. The source
Alpha limitation still applies.

Assets remain outside the style, supplied by MosaicJSON, STAC, or a custom
MosaicSource. See the [mosaic guide](../mosaics/) for configuration and limits.

## Migration

Removed: `from_query` / `from_pairs`, `ValidatedTileStyle`, `compile_validated`,
mutable style fields, `Stretch` / `ColourDefinition` / `Colour` / `lut_from_stops`, and the separate mosaic
selection argument. Use `from_json` / `from_raster_query` / `from_raster_style`.
`TileStyle::default()` is a protocol gray-band-1 style with fixed 0–255 mapping.
HTTP requires `version=2.0&type=...`; old parameters and unknown fields fail.
All viewers emit Q2 only. Opacity is applied once by the server; MapLibre offers
mosaic stage and rank-channel controls.

| Legacy input | Raster Style input |
| --- | --- |
| `bidx=1&colormap_name=viridis&rescale=0,3000` | Add `version=2.0&type=pseudocolor&method=linear` |
| Three identical RGB ranges | One broadcast range; explicitly set `type=rgb` and three `bidx` values |
| Omitted stretch auto-selects stddev for non-U8 | Fixed `[0,255]`; explicitly supply a range when needed |
| Continuous stops in `colormap` | Do not reuse: native arrays are interval tables; continuous color_mapping is not yet implemented |
| `stretch=percent&pc=2,98` | `method=percentile&percentiles=2,98` |
