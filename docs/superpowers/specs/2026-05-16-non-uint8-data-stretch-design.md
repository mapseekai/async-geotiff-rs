# Default Data Stretch for Non-uint8 Rasters

Date: 2026-05-16
Status: Approved (design)

## Problem

When a tile is requested for a non-uint8 COG (UInt16, Int16, UInt32,
Int32, Float32, Float64) **without** an explicit `?rescale=` query
parameter, the renderer applies a hardcoded `0–255` range (the
`auto_rgb` / single-band auto path in `src/style/params.rs`). UInt16
values (0–65535) therefore saturate to 255 almost everywhere and the
client sees a near-uniform white image.

titiler/rio-tiler avoid this by deriving a default stretch range from
**dataset band statistics** stored in the COG. This spec adds the same
behavior.

## Decisions (locked)

- Default range source: **dataset statistics from COG metadata**, with a
  fallback chain when absent.
- Range formula from stats: **raw `STATISTICS_MINIMUM` →
  `STATISTICS_MAXIMUM`** (predictable, matches GDAL/titiler defaults; no
  σ/percentile contrast stretch).
- Float (F32/F64) with no statistics tags: **per-tile min/max**
  (integers without stats use dtype natural range; only the rare
  statless-float case accepts possible tile seams).
- Scope: triggers only when no explicit `rescale` is given and dtype is
  not U8. U8 behavior and the U8 fast path are unchanged.

## Architecture (Approach A: resolve in the tiler, between read and render)

Rationale: `compile()` / `compile_cached()` build `rescale` but have no
access to dataset dtype or statistics, and the moka style cache must not
be keyed on dataset identity. So the default range is resolved after
read+warp and after style compile, then passed into `render()` (whose
signature already accepts a rescale). `params.rs` stays dataset-pure;
`render()` / `pipeline.rs` hot path is untouched.

```
CogReader.read_window()              src/io/cog.rs (~line 412)
  ├─ existing: gdal_nodata() → nodata
  └─ NEW: parse_band_stats(data_ifd.gdal_metadata(),
                           data_ifd.min_sample_value(),
                           data_ifd.max_sample_value())
        → band_stats: Option<Vec<(f64,f64)>>   (indexed by original band)
  ↓
ImageData { …, nodata, band_stats }  src/pixel.rs:113  (one new field)
  ↓
warp::warp()                          copies band_stats through unchanged
  ↓
tiler.tile()  Step 5/6 seam          src/tiler.rs  (one new call site)
  if compiled.rescale.is_empty() && !is_u8(pixels) && !explicit_range_composite:
      effective = resolve_default_rescale(&pixels, band_stats, &compiled.bidx)
  else:
      effective = compiled.rescale
  ↓
render(pixels, &effective, …)         src/render/pipeline.rs (unchanged sig)
```

Note: statistics tags describe the full-resolution band. Reading from an
overview level does not change the value distribution meaningfully and
titiler has the same property, so stats are used as-is regardless of
overview level.

## Component 1 — statistics parsing (`src/io/stats.rs`, new)

```rust
/// Per-band (min, max) parsed from COG metadata, 0-based band index.
pub fn parse_band_stats(
    gdal_metadata: Option<&str>,
    min_sample_value: Option<&[u16]>,
    max_sample_value: Option<&[u16]>,
) -> Option<Vec<(f64, f64)>>;
```

GDAL_METADATA (TIFF tag 42112, exposed by `async_tiff 0.3.0` as
`gdal_metadata()`) is an XML string:

```xml
<GDALMetadata>
  <Item name="STATISTICS_MINIMUM" sample="0">128</Item>
  <Item name="STATISTICS_MAXIMUM" sample="0">4071</Item>
  <Item name="STATISTICS_MINIMUM" sample="1">96</Item>
  ...
</GDALMetadata>
```

Per band, resolution priority:

1. `STATISTICS_MINIMUM` / `STATISTICS_MAXIMUM` from GDAL_METADATA,
   matched by the `sample=` index.
2. TIFF `SMinSampleValue` / `SMaxSampleValue` (`min_sample_value()` /
   `max_sample_value()`, tags 340/341) if present.
3. Band absent in both → `None` for that band (handled downstream).

Parsing uses a small hand-rolled scan for `<Item name=… sample=…>value
</Item>` — no new dependency. Malformed or missing XML yields `None`;
never an error. Consistent with the existing
`gdal_nodata().and_then(|s| s.parse().ok())` pattern at `cog.rs:412`.

## Component 2 — `ImageData` plumbing

`src/pixel.rs:113` gains one field:

```rust
pub struct ImageData {
    pub pixels: PixelData,
    pub mask: Option<Array2<bool>>,
    pub bbox: Bbox,
    pub crs: Crs,
    pub transform: GeoTransform,
    pub nodata: Option<f64>,
    pub band_stats: Option<Vec<(f64, f64)>>, // NEW: full-band (min,max)
}
```

`band_stats` is indexed by **original** band order. `warp` copies the
field through unchanged (one-line struct field; values not touched).
Any other `ImageData` constructors set `band_stats: None`.

## Component 3 — `resolve_default_rescale` (`src/style/rescale.rs`, new)

```rust
pub fn resolve_default_rescale(
    pixels: &PixelData,
    band_stats: Option<&[(f64, f64)]>,
    bidx: &[usize],            // 0-based selected/compiled bands
) -> Vec<(f64, f64)>;
```

Pure function (no IO). For each selected band, in priority order:

1. **Stats present** for that band → raw `(min, max)` from `band_stats`.
2. **No stats, integer dtype** → dtype natural range:
   - U16 → `(0.0, 65535.0)`
   - I16 → `(-32768.0, 32767.0)`
   - U32 → `(0.0, 4294967295.0)`
   - I32 → `(-2147483648.0, 2147483647.0)`
   - U8 → not reached (caller skips u8)
3. **No stats, float dtype (F32/F64)** → per-tile min/max: one pass over
   that band's pixels, skipping nodata and mask-invalid samples.

Degenerate guards:

- `max <= min` (flat band / all-nodata tile) → widen to
  `(min, min + 1.0)`.
- Float per-tile pass finds no valid sample → `(0.0, 1.0)`.

This is the single home of stretch policy and is fully unit-testable
without IO. The `tiler.rs` call site is the only place the float
fallback needs pixel access, and it holds the warped `PixelData` there.

## Trigger conditions

Default applies only when **all** hold:

- compiled style has empty `rescale` (no `?rescale=` given), AND
- dtype is not U8, AND
- the composite is not a discrete / explicit-vmin-vmax colormap (those
  already carry their own range).

Explicit `?rescale=` always wins. U8 keeps the existing fast path
unchanged (zero behavior and zero perf change for 8-bit RGB). Both
single-band non-u8 (auto "greys" colormap) and 3-band non-u8 auto-RGB
flow through the resolver.

## Error handling

The feature never introduces a hard error. Malformed GDAL XML, missing
tags, and all-nodata tiles silently fall back down the priority chain.

## Testing

- Unit (`stats.rs`): multi-band `sample=` items; missing items;
  malformed XML → `None`; SMin/SMax fallback path.
- Unit (`rescale.rs`): each priority branch; degenerate `max<=min`;
  float per-tile pass skipping nodata; `bidx` reordering selects the
  correct stats entry.
- Integration: synthetic UInt16 COG **with** stats → output spans full
  tonal range (assert not near-uniform-white); UInt16 **without** stats
  → dtype-range path (documents the darker tradeoff); F32 no-stats →
  per-tile path produces visible contrast.
- Regression: existing U8 RGB tile byte-identical to pre-change (fast
  path guard).

## Out of scope (YAGNI)

Percentile / mean±σ contrast stretch; GDAL SCALE/OFFSET application;
per-tile stretch for integers; a `/statistics` endpoint; auto-contrast
for u8.
