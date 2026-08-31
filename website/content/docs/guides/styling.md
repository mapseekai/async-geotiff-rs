---
title: Styling and band math
description: Select bands, stretch values, apply colormaps, and derive bands with expressions.
weight: 10
---

The same `TileStyle` model serves the Rust API and the example server’s query
string. Styles are validated before expensive COG reads and rendering work.

## Common recipes {#recipes}

Natural-color RGB from three bands:

```text
bidx=1&bidx=2&bidx=3&rescale=0,3000&rescale=0,3000&rescale=0,3000
```

Single-band color ramp:

```text
bidx=1&colormap_name=viridis&rescale=0,3000
```

Percentile stretch:

```text
stretch=percent&pc=2,98
```

Average downsampling and bilinear reprojection:

```text
resampling=average&reproject=bilinear
```

## Stretch behavior {#stretch}

For non-U8 data, the effective value range is selected in this order:

1. Explicit `rescale=min,max` values.
2. The requested `stretch` mode.
3. `stretch=stddev&sigma=2.0` when neither is present.

U8 RGB data keeps its 0–255 fast path. Dataset statistics come from embedded
`STATISTICS_*` tags when present; otherwise the highest overview is sampled
once and the estimate is memoized.

## Arithmetic expressions {#expressions}

`expression` computes one derived output band. Band references are 1-indexed:

```text
expression=(b5-b4)/(b5+b4)&rescale=-1,1&colormap_name=viridis
```

This NDVI formula is only correct when band 5 is near-infrared and band 4 is
red in the selected COG. The library does not infer spectral roles from band
numbers; verify the asset metadata and adjust the references before using the
formula. A pre-rendered STAC `visual` asset is not automatically suitable for
this example.

When used in a URL, percent-encode `/` and `+`:

```bash
curl --output ndvi.png \
  'http://127.0.0.1:8080/tiles/WebMercatorQuad/0/0/0.png?expression=(b5-b4)%2F(b5%2Bb4)&rescale=-1,1&colormap_name=viridis'
```

The grammar supports finite decimal numbers, parentheses, binary `+`, `-`,
`*`, `/`, and unary `-`. Multiplication and division bind more tightly than
addition and subtraction. Scientific notation is not supported.

`expression` is mutually exclusive with `bidx` and `color_formula`, and it
accepts at most one `rescale` range. Nodata inputs, division by zero, and other
non-finite results become transparent pixels. Input is bounded to 4096 bytes,
256 tokens, and 128 nesting levels.

## Color formulas {#color-formulas}

The supported rio-color subset applies operations from left to right:

- `gamma`
- `sigmoidal`
- `saturation`

For example:

```text
color_formula=gamma RGB 1.5
```

Percent-encode spaces when the formula is placed in a URL query string.
