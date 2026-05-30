# Closing the maptile integration gaps in `async-geotiff`

**Date:** 2026-05-30
**Status:** Approved (design); ready for implementation planning
**Library version at design time:** 0.0.2

## Context

`async-geotiff` is consumed by the `maptile` service (`services/maptile/maptile-core`),
which maps its own `EffectiveStyle` model onto the library's `TileStyle`. During the
migration to async-geotiff, six capability gaps forced maptile to either return
`InvalidStyle` (HTTP 400) or take on an unwanted direct dependency on `async-tiff`.
This spec closes all six.

The gaps, as reported by the consumer:

1. **Band-math indices** (NDVI / NDWI / NDBI / EVI / SAVI) — `TileStyle` has no
   band-math stage. maptile's `EffectiveStyle.index` + `band_assignments` have no
   target. Workaround: 400.
2. **Inline colormap config** — maptile's `EffectiveStyle.colormap` (`ColorMapConfig`:
   discrete / gradient JSON) has no target; the library only accepts `colormap_name`.
   Workaround: 400.
3. **Non-colorous colormaps (Terrain)** — `BuiltinColormap::Terrain` is not in the
   colorous set the library exposes. Workaround: 400.
4. **Band scale/offset (`unscale`)** — the library never reads GDAL scale/offset
   metadata; maptile's `EffectiveStyle.unscale` has no target. Workaround: 400.
5. **`mode` resampling** — `ResamplingMethod::Mode` has no target in the library's
   `Resampling` enum. Workaround: 400.
6. **`ObjectReader` not re-exported, no object_store convenience constructor** — to
   open a COG from an existing `Arc<dyn ObjectStore>` (e.g. MinIO with credentials),
   the caller must name and construct `async_tiff::reader::ObjectReader` directly,
   forcing a direct, version-locked `async-tiff` dependency.

## Goal

One comprehensive, phased design that closes all six gaps, sequenced so the trivial
tier ships first (clearing three of maptile's five HTTP-400 paths) while the heavier
pipeline work follows.

## Key findings from the codebase

- **Issue 2 is ~80% already built.** `ColourDefinition` (`src/render/composite.rs:17-23`)
  already has `Colours` / `ColoursAndBreaks` / `Discrete` variants, and
  `Composite::from_definition` already handles them. `TileStyle` simply only exposes
  `colormap_name → ColourDefinition::Named`. This is mostly a parse-and-wire job.
- **`object_store` is already in the public API.** `CogReader::open_http_with_options`
  takes `object_store::ClientOptions` (`src/io/cog.rs:84`). So consumers already need a
  version-matched `object_store`; re-exporting it is the clean fix, not a new exposure.
- **Render path today** is `bidx-select → valid_mask → composite/formula`
  (`src/render/pipeline.rs:55-104`). The new stages insert into this chain.
- GDAL per-band scale/offset live in TIFF tag **42112** (`GDAL_METADATA`, XML).

## Resolved design decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Session scope | All 6 in one phased spec | Coordinated, but ships incrementally |
| Band-math model | Generic expression engine + typed `Index` enum sugar that lowers to expression strings | Future-proof: common 5 are ergonomic, anything else is a raw expression |
| Expression grammar v1 | Arithmetic only (`+ − × ÷`, unary minus, parens, float literals, `b<N>`) | Covers all 5 indices exactly; smallest, exhaustively testable; functions can be added later without breaking the grammar |
| New param naming | Follow titiler conventions (`expression`, `colormap`, `unscale`, `resampling=mode`) | Library already mirrors titiler (`color_formula`, `rescale`, `nodata`, …) |

## A. Cross-cutting: pipeline stage order

This is the spine. The new stages have a **forced order**; violating it fails silently
(NDVI computed on raw DN instead of unscaled physical values is garbage but does not
error):

```
decode → [unscale] → [expression] → bidx/rescale → [colormap] → encode
         (issue 4)   (issue 1)                      (issues 2,3)
```

Invariants the spec pins:

- `unscale` runs **before** `expression`.
- `expression` operates on **unscaled** values.
- `expression` produces a single synthetic band that then flows through the normal
  rescale + colormap path unchanged.

## B. Phasing

| Phase | Issue | Target version | Notes |
|-------|-------|----------------|-------|
| 1 | #6 ObjectReader / object_store re-export + `open_object_store` | 0.0.3 | Zero pipeline risk; unblocks the forced `async-tiff` dep |
| 2 | #3 Terrain colormap | 0.0.3 | One builtin LUT, isolated |
| 3 | #2 Inline colormap wiring | 0.0.3 | ~80% already built |
| 4 | #4 unscale (GDAL scale/offset) | 0.0.4 | Adds metadata read; must precede expression for correct ordering |
| 5 | #5 mode resampling | 0.0.4 | Isolated kernel addition |
| 6 | #1 band-math expression + `Index` sugar | 0.0.5 | Largest; depends on unscale ordering |

Phases 1–3 alone clear three of maptile's five HTTP-400 paths.

## C. Phase details

### Phase 1 — #6 API ergonomics (0.0.3)

In `src/io/mod.rs`:

```rust
pub use async_tiff::reader::ObjectReader;
pub use object_store;            // whole crate, so consumers version-match through us
```

Add a convenience constructor on `CogReader`:

```rust
/// Open a COG from an existing object store + key, building the ObjectReader internally.
pub async fn open_object_store(store: Arc<dyn ObjectStore>, key: &str) -> AgResult<Self>
```

(plus `_with_cache` / `_with_config` variants mirroring the existing `open_with_reader*`
family for parity.)

Result: maptile needs neither `async-tiff` nor a separately-versioned `object_store`;
it constructs its `Arc<dyn ObjectStore>` against `async_geotiff::io::object_store` and
calls `open_object_store`.

**Tests:** constructor opens a known fixture COG from an in-memory/local object store and
matches `open_http` output; doc-test of the re-export path compiles.

### Phase 2 — #3 Terrain colormap (0.0.3)

Add a `"terrain"` arm to `builtin_lut` (`src/render/colormap.rs`) using the known
GDAL/matplotlib terrain stops, built via the existing `lut_from_stops`. maptile maps
`BuiltinColormap::Terrain → "terrain"`. (Existing `Gray → "greys"` mapping stays.)

**Tests:** `builtin_lut("terrain")` returns Ok; endpoint colors match reference stops
(low = blue/green water-edge, high = white peaks); unknown name still errors.

### Phase 3 — #2 inline colormap (0.0.3)

Add `colormap: Option<String>` to `TileStyle` (titiler's `colormap=` JSON param) and a
`"colormap"` arm in `apply_param`. Parse the JSON into the existing `ColourDefinition`
(`Discrete` / `Colours` / `ColoursAndBreaks`) and feed the existing
`Composite::from_definition`.

**JSON contract (pinned).** Accept titiler's `colormap=` format, since the library
already mirrors titiler param names:

- Discrete: `{"<int>": [r,g,b,a], ...}` → `ColourDefinition::Discrete`
- Gradient (intervals): `[[v, [r,g,b,a]], ...]` → `ColourDefinition::ColoursAndBreaks`

`ColourDefinition` is `#[serde(untagged)]` (first-variant-that-deserializes wins, which
is fragile). The spec requires an explicit parse step that routes by JSON shape (object
vs array) rather than relying on untagged ordering, and a round-trip test against
maptile's `ColorMapConfig` serialization for both discrete and gradient.

`colormap` and `colormap_name` are mutually exclusive (error if both given).

**Tests:** discrete JSON → expected RGBA + transparent-on-unmapped; gradient JSON →
expected endpoint colors; malformed JSON → `AgError::query`; both `colormap` and
`colormap_name` → error; round-trip from maptile's `ColorMapConfig` JSON.

### Phase 4 — #4 unscale (0.0.4)

At open time, read TIFF tag **42112** (`GDAL_METADATA`) and parse per-band `SCALE` /
`OFFSET` items from its XML; store `Vec<(scale, offset)>` (default `(1.0, 0.0)`) on
`CogReader`. Add `unscale: bool` to `TileStyle`. When set, apply `v*scale + offset`
immediately after decode (before expression). Bands without metadata use identity.

Optional explicit-override path (caller-supplied scale/offset) is **deferred**, not v1.

**Tests:** fixture COG with GDAL_METADATA scale/offset → unscaled values match expected;
missing-metadata band → identity; `unscale=false` → raw values unchanged; ordering test
asserting unscale precedes expression.

### Phase 5 — #5 mode resampling (0.0.4)

Add `Resampling::Mode` and `"mode"` to `Resampling::parse` (`src/warp/resampler.rs`).
Majority vote over the source footprint a destination pixel covers; **downsample-only**,
degrading to nearest-neighbour on upsampling (mirrors `Average`'s documented contract).
Intended for categorical/classification rasters.

**Tests:** `parse("mode")` → `Mode`; downsampling a known categorical block returns the
majority class; upsampling matches nearest; ties resolve deterministically (document the
rule, e.g. lowest value wins).

### Phase 6 — #1 band-math expression + `Index` sugar (0.0.5)

**Engine.** Arithmetic-only evaluator: tokenize → shunting-yard → per-pixel eval.

```
expr   := term (('+'|'-') term)*
term   := factor (('*'|'/') factor)*
factor := number | band | '(' expr ')' | '-' factor
band   := 'b' DIGIT+          // 1-indexed, matches bidx convention
number := FLOAT
```

Add `expression: Option<String>` to `TileStyle`, **mutually exclusive with `bidx`**
(expression names bands directly via `b<N>` and produces one synthetic band, which then
runs through the normal rescale + colormap path).

**Semantics (pinned):**
- division-by-zero / NaN result → transparent (treated as nodata)
- nodata in **any** referenced input band → nodata out
- v1 = single-band expression output (titiler's comma-separated multi-band is deferred)

**Typed sugar** that lowers to expression strings:

```rust
pub enum Index {
    Ndvi { nir: u32, red: u32 },
    Ndwi { green: u32, nir: u32 },
    Ndbi { swir: u32, nir: u32 },
    Evi  { nir: u32, red: u32, blue: u32 },
    Savi { nir: u32, red: u32, l: f64 },
}
impl Index { pub fn to_expression(&self) -> String }
```

Examples produced:
- NDVI: `(b{nir}-b{red})/(b{nir}+b{red})`
- EVI: `2.5*(b{nir}-b{red})/(b{nir}+6*b{red}-7.5*b{blue}+1)`
- SAVI: `(1+{l})*(b{nir}-b{red})/(b{nir}+b{red}+{l})`

maptile maps `EffectiveStyle.index` + `band_assignments` → an `Index` value (or a raw
`expression` for anything outside the five).

**Tests (reference values):**
- NDVI(nir=0.5, red=0.1) = 0.667 (±1e-3)
- EVI / SAVI at known reference points
- division-by-zero (water/shadow, denom=0) → transparent
- nodata in an input band → nodata out
- unknown band `b9` on a 6-band image → `AgError`
- malformed expression (`(b1+`, `b1**b2`) → `AgError::query`
- each `Index` variant lowers to the expected expression string

## D. Compatibility & testing

- `TileStyle` gains `#[non_exhaustive]`; docs show `..Default::default()` construction.
  The consumer version-tracks the library, so additive field changes must be explicit.
- Per project CLAUDE.md (TDD, 80% coverage): every phase ships tests first; each phase
  detail above lists its test plan. The evaluator is tested against reference index
  values.

## Out of scope (deferred, not v1)

- Functions in the expression grammar (`sqrt`/`abs`/`min`/`max`/`clamp`).
- Multi-band (comma-separated) expression output.
- Caller-supplied explicit scale/offset override for unscale.
- `mode` resampling on upsampling (degrades to nearest by design).
