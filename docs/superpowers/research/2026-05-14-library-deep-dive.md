# Library Deep-Dive — async-geotiff-rs

Date: 2026-05-14
Inputs: 9 parallel research agents (5 Rust crates, 4 Python references)

## TL;DR — what the research changes about the design

1. **async-tiff is lower-level than rio-tiler's `Reader`.** It gives us
   `ImageFileDirectory` + tile-indexed fetch + sync decode. Everything geo
   (GeoTransform, CRS, overview pick, bbox→tile window, nodata parse, stitch)
   is **our job**. The spec already factors this into `io/`.
2. **Use `object_store` + `ObjectReader`**, not `ReqwestReader`. ObjectReader
   coalesces ranges; ReqwestReader does not.
3. **Decode is sync** — wrap in `tokio::task::spawn_blocking`.
4. **`proj::Proj` is `!Send + !Sync`.** Per-worker `thread_local!` instances,
   or own the `Proj` inside a `spawn_blocking` task. Never `Arc<Mutex<Proj>>`
   in a hot loop.
5. **Warp via coarse-grid approximation** (gdalwarp-style): sample a 32×32 grid
   through PROJ exactly, bilinear-interpolate intermediate pixel mappings.
   ~50–500× faster than per-pixel proj calls; sub-pixel error.
6. **No standalone `affine` crate.** Use `geo::AffineTransform<f64>` for the
   2×3 GeoTransform.
7. **`tile-grid` is sync, no slippy-Y helper.** Apply `matrix_height - 1 - y`
   manually for SW-origin TMSs used as XYZ.
8. **fast_image_resize wants interleaved (H,W,C); ndarray default is planar
   (C,H,W).** Pay one permute-copy on ingest, keep interleaved through the
   render pipeline.
9. **Pre-baked colormaps: use `colorous`** (viridis/magma/plasma/inferno/
   cividis/turbo — exact matplotlib LUTs). Reserve `enterpolation` for
   user-supplied control points.
10. **Sigmoidal contrast is not in `palette`.** Hand-code per rio-color's
    exact normalized formula in linear sRGB.
11. **Titiler query semantics:** `bidx` and `rescale` are *repeated* params,
    not CSV. Default tile size is **512**. Format is a path suffix, not a
    query param.
12. **rio-tiler's mask priority:** alpha band > GDAL_NODATA > none. Replicate.

---

## 1. async-tiff (0.3.0)

**Open a remote COG**:
```rust
let store = Arc::new(HttpBuilder::new().with_url(base).build()?); // object_store
let reader = ObjectReader::new(store, path.into());
let cache  = ReadaheadMetadataCache::new(reader.clone());
let mut md = TiffMetadataReader::try_open(&cache).await?;
let ifds   = md.read_all_ifds(&cache).await?;
let tiff   = TIFF::new(ifds, md.endianness());
```

**What async-tiff gives us**:
- `ImageFileDirectory` per overview level (one IFD = one level).
- Geo tags raw: `model_tiepoint()`, `model_pixel_scale()`,
  `model_transformation()`, `geo_key_directory()`.
- Per-tile fetch: `ifd.fetch_tiles(coords, &reader) -> Vec<Tile>`
  (one batched IO via `get_byte_ranges`).
- `Tile::decode(&DecoderRegistry::default()) -> Array` (sync, CPU-bound).
- Compressions: None, Deflate, LZW, JPEG, WebP, ZSTD, LZMA, LERC, JPEG2000.
- Predictor 2 + float-predictor 3 supported.

**What async-tiff does NOT give us — we must implement**:
- GeoTransform construction from tiepoint+scale (or 4×4 transformation).
- CRS resolution from `GeoKeyDirectory` (parse GeoKeys → EPSG → `Proj`).
- Overview level picker (compare desired GSD to `image_width` per IFD).
- pixel-window → tile-coord math + cropping/stitching of returned tiles.
- `spawn_blocking` decode pool.
- `gdal_nodata() -> Option<&str>` parsing per dtype.

**Known gaps in async-tiff**:
- Tile-interleaved COGs not supported (issue #247).
- Band-interleaved selective read not supported — we fetch all samples.
- No mask band / sparse tile / BigTIFF (unverified) / mosaic.

## 2. tile-grid (0.6)

**Registry**: `tile_grid::tms().lookup("WebMercatorQuad")` /
`"WorldCRS84Quad"` plus 9 more bundled OGC TMSs.

**Key calls**:
- `tms.xy_bounds(&Xyz::new(x, y, z)) -> BoundingBox` (in TMS CRS).
- `tms.resolution_z(z) -> Option<f64>` (CRS units/pixel).
- `tms.crs()` → `Crs` with `.as_srid()` for EPSG.
- `Xyz::new` arg order is `(x, y, z)`. **Note: arg order trap.**

**Slippy-XYZ ↔ TMS-Y**: NW-origin TMSs (WebMercatorQuad) match XYZ natively
— no flip needed. SW-origin TMSs need manual
`tms.matrix(z).matrix_height - 1 - y`. Wrap in a helper.

**Gap**: no JSON loader convenience; deserialize via `ogcapi-types` if we
ever need custom TMS. Variable-matrix-width TMSs degrade.

## 3. proj (0.27) + Affine

**System dep**: PROJ 9.6 + SQLite3 + libtiff dev headers.
`bundled_proj` feature builds libproj from source; still needs sqlite+tiff.
**No pure-Rust replacement at parity** (`proj4rs` covers a subset).

**Axis order trap**: `Proj::new_known_crs("EPSG:4326","EPSG:3857", None)`
normalizes EPSG:4326 to **(lon, lat)**, opposite of pyproj 3+ default.
Document this loudly.

**Threading**: `Proj` is `!Send + !Sync`. Options:
- `thread_local!` per worker (recommended for rayon).
- own `Proj` inside `spawn_blocking` (recommended for tokio path).
- never `Arc<Mutex<Proj>>` in hot loops.

**Batch**: `convert_array(&mut [(f64,f64)])` is 3–10× faster than scalar.
Always batch.

**Warp approximation (gdalwarp pattern)**:
1. Sample a coarse 32×32 grid in destination tile through PROJ exactly.
2. Bilinear-interpolate intermediate dst→src pixel coords.
3. Validate corners + center; subdivide if max residual > 0.1 px.
Gives ~50–500× speedup with sub-pixel error vs per-pixel `convert`.

**GeoTransform**: use `geo::AffineTransform<f64>::new(a,b,c,d,e,f)` —
`.inverse()` for world→pixel; matches GDAL exactly.
- NO standalone `affine` crate on crates.io.
- Internal newtype `GeoTransform([f64;6])` with `From`/`Into` for ergonomics.

## 4. ndarray (0.16) + fast_image_resize (5.x)

**Layout trap**: ndarray C-order `(C,H,W)` is **planar (RRRGGGBBB)**,
fast_image_resize wants **interleaved (RGBRGBRGB)**.

```rust
// Pay the permute copy once on ingest:
let interleaved = src.view().permuted_axes([1,2,0]).to_owned();
let buf = interleaved.into_raw_vec_and_offset().0;
let img = Image::from_vec_u8(w, h, buf, PixelType::U8x3)?;
```

**Rule**: keep render pipeline working on `(H,W,C)` interleaved; only
permute back to `(C,H,W)` if a downstream consumer demands it. We don't —
PNG encode wants interleaved anyway.

**fast_image_resize 5.x**:
- Pixel types include `F32` (single-band f32 — perfect for DEMs).
- Filters: Nearest, Bilinear, CatmullRom, Mitchell, Lanczos3.
- Runtime-detected SSE4/AVX2/NEON.
- `rayon` feature parallelizes rows. **Enable.**

**ndarray performance ladder**:
- `Zip::from(a).and(b).par_for_each(...)` — best, SIMD + rayon.
- `azip!` macro — same.
- `mapv` allocates; prefer `mapv_inplace`.
- `axis_iter` for per-band kernels.

**Colormap LUT**: build output as `Array3<u8>` of shape `(H, W, 4)`
interleaved RGBA — matches `PixelType::U8x4` for the subsequent resize, skips
a permute.

**ndarray 0.16 note**: `into_raw_vec` renamed to
`into_raw_vec_and_offset` returning `(Vec, usize)`.

## 5. palette (0.7) + enterpolation (0.3) + colorous

**Recommendation**: use **`colorous`** for matplotlib-faithful colormaps:
```rust
let c = colorous::VIRIDIS.eval_continuous(t); // t in [0,1]
```
Exact LUTs as static `&[[u8;3]]`. Use `enterpolation::Linear` + `Clamp`
only for **user-supplied** stop colors.

**rio-color formula in Rust** — applied left-to-right in **sRGB** (NOT linear),
per rio-color semantics:

```rust
// gamma: out = in^(1/g) — APPLIED IN sRGB SPACE per rio-color
// sigmoidal: normalized so s(0)=0, s(1)=1
// saturation: rio-color goes RGB->XYZ->LAB->LCH, scales C, back
//             (NOT simple HSV) — copy the formula exactly
```

**Sigmoidal** is not in `palette`; hand-code:
```rust
#[inline] fn sig(x: f32, beta: f32, alpha: f32) -> f32 {
    let s = |v: f32| 1.0 / (1.0 + (beta*(alpha - v)).exp());
    ((s(x) - s(0.0)) / (s(1.0) - s(0.0))).clamp(0.0, 1.0)
}
```
Short-circuit when `contrast == 0.0` (rio-color does).

**Saturation accuracy**: rio-color uses RGB→XYZ→LAB→LCH with D65, scales
C, inverse. Simple HSV scaling will visibly drift in hue. We must match.
`palette` provides Lab/Lch types — use those, not Hsv.

## 6. rio-tiler — what we copy

Source: `rio_tiler/io/rasterio.py:252-301`, `reader.py:335-484`,
`models.py:339-888`, `colormap.py:91-177`, `utils.py:549-658`.

**Pipeline (port to Rust)**:
```
tile(z,x,y) ──> bbox in TMS CRS
              ──> part(bbox, dst_crs, w, h)
                     ├ src_crs == dst_crs: read window directly
                     └ else: WarpedVRT (our equivalent = warp module
                                        with proj + bilinear)
              ──> ImageData { array, mask, alpha_mask, bounds, crs, nodata }
              ──> post_process: rescale → color_formula → colormap
              ──> render: pack to (H,W,C) + alpha → encode
```

**Mask priority** (replicate):
1. Alpha band (if `ColorInterp::Alpha` present) → alpha_mask.
2. Else GDAL_NODATA value → mask = (data == nodata).
3. Else: fully valid.

**Encoding**: rio-tiler uses GDAL drivers via `MemoryFile`. We use the
Rust `image` crate (PNG by default; WebP/JPEG behind features).

## 7. titiler — URL contract we replicate

**Path**: `/tiles/{tileMatrixSetId}/{z}/{x}/{y}[.{format}]`
- `format` ∈ `png|jpeg|webp|tif|npy` (path suffix, not query).
- Default `tilesize=512` — confirm we want this default (rio-tiler `tile()`
  defaults to 256; titiler defaults to 512). **Pick: 256 for closer parity
  with most XYZ clients; expose `tilesize` query to override.**

**Query parameters (full mapping → `RenderSpec`)**:

| Query | Type | Repeat? | Notes |
|---|---|---|---|
| `bidx` | int | yes | `?bidx=1&bidx=2&bidx=3` |
| `expression` | str | no | rio-tiler band math, `;`-separated. **Post-MVP.** |
| `nodata` | float / "nan" / "inf" / "-inf" | no | overrides dataset nodata |
| `unscale` | bool | no | apply scale/offset tags |
| `resampling` | enum | no | nearest/bilinear/cubic/cubic_spline/lanczos/average |
| `reproject` | enum | no | same enum, used only for warp |
| `buffer` | float (0.5 multiples) | no | tile edge buffer in pixels |
| `padding` | int | no | edge padding |
| `rescale` | "min,max" | yes | one per band |
| `color_formula` | str | no | rio-color subset |
| `colormap_name` | str | no | colorous + extras |
| `colormap` | JSON | no | custom — **post-MVP** |
| `return_mask` | bool | no | default true |
| `tilesize` | int | no | default 256 (our choice) |
| `algorithm` | str | no | hillshade/terrarium/normalizedIndex — **post-MVP** |
| `algorithm_params` | JSON | no | post-MVP |

**Errors**: 422 for invalid enum; 400 for malformed value / bad formula /
bad JSON. Our `AgError` → axum response mapping mirrors this.

## 8. rio-color — formula DSL

**Operators** (left-to-right, applied in sRGB):

| Op | Syntax | Math |
|---|---|---|
| gamma | `gamma BANDS G` | `out = in^(1/G)` |
| sigmoidal | `sigmoidal BANDS β α` | normalized sigmoid (formula above) |
| saturation | `saturation P` | RGB→Lab→LCh, `C' = C·P`, back to RGB |

**BANDS**: any combination of `r`/`g`/`b`/`R`/`G`/`B`/`1`/`2`/`3`.
`rgb`==`RGB`==`123`.

**Regression test data** (from `test_operations.py`):
- `gamma rgb 0.95` on `[10,50,100]/255` → channel 0 ≈ `0.033069782`
- `sigmoidal rgb 5 0.53` on same input → channel 0 ≈ `0.080560341`
- `saturation 1.25` on `[0.392,0.776,0.164]` → channel 0 ≈ `0.158606224`

These become our Rust unit tests for byte-exact parity.

**Parser**: simple whitespace split (rio-color does this). nom skeleton in
the rio-color report — use it.

## 9. map-engine — author's preferred design language

The user's own Rust project; this informs API ergonomics:

- **Strong-typed gradients** with multiple shapes:
  `Named` | `Colours` | `ColoursAndBreaks` | `Discrete` | `RGB`.
- **Startup preload**: parse styles once, hold `Arc<State>`. Per-request
  computation is bounded.
- **`thiserror`-based unified error**: `MapEngineError`.
- **Chained builder**: `Raster::read_tile() → RawPixels::style() →
  StyledPixels::into_png()`.
- **Module split**: `cmap` / `colour` / `raster` / `tiles`.

These hints are compatible with the titiler URL contract — they just shape
how we structure our internal types and error handling. Spec already aligns.

---

## 10. Decisions to fold back into the spec

1. **Default tile size = 256** (not titiler's 512) — wider XYZ client compatibility.
   Override via `?tilesize=`.
2. **`bidx` and `rescale` are repeated query params**, not CSV. Update
   `RenderSpec` parsing accordingly.
3. **Color formula applied in sRGB** with rio-color's exact Lab/Lch saturation
   path; sigmoidal short-circuit on `contrast=0`.
4. **Colormap source**: `colorous` for built-ins; `enterpolation` only for
   user stops.
5. **Output buffer layout**: `(H, W, C)` interleaved throughout the render
   pipeline. One permute-copy on ingest from `(C, H, W)` async-tiff output.
6. **Warp via coarse-grid approximation**, not per-pixel proj. 32×32 grid,
   bilinear interpolated, validated and subdivided on residual > 0.1 px.
7. **PROJ threading**: own `Proj` inside `spawn_blocking` task that performs
   warp+decode. No `Mutex<Proj>` shared state.
8. **Affine type**: `geo::AffineTransform<f64>` wrapped by an internal
   `GeoTransform` newtype.
9. **Mask priority**: alpha band > nodata > none.
10. **Decode pool**: `spawn_blocking` for `Tile::decode`. Bound concurrency
    with `tokio::sync::Semaphore` (post-MVP if needed).
11. **Format in path**: `/tiles/{tms}/{z}/{x}/{y}.{ext}` — keep `ext` as
    `png|jpeg|webp`. `tif|npy` post-MVP.

## 11. Confidence and open items

High confidence on: async-tiff API surface, tile-grid, fast_image_resize 5.x,
palette/colorous, titiler query schema, rio-color math.

To verify before coding:
- [ ] Exact `proj` crate version pin and PROJ 9.6 availability on dev box.
- [ ] ndarray 0.16 `into_raw_vec_and_offset` return shape on permuted views.
- [ ] `async-tiff` Cargo feature flags actually pin (default vs `ndarray`).
- [ ] Whether `colorous` returns enough colormap variety for our needs, or
      we need to bundle a few extras (rio-tiler ships ~70 colormaps).
- [ ] HTTP store: use `object_store::http::HttpBuilder` (preferred) vs reqwest.

## Sources

- async-tiff 0.3.0 docs.rs, github developmentseed/async-tiff
- tile-grid 0.6 docs.rs, github pka/tile-grid
- proj 0.27 docs.rs, github georust/proj
- geo crate AffineTransform docs.rs
- ndarray 0.16 docs.rs
- fast_image_resize 5.x docs.rs
- palette 0.7, enterpolation 0.3, colorous docs.rs
- rio-tiler v7+ source (cogeotiff/rio-tiler)
- titiler latest source (developmentseed/titiler)
- rio-color (mapbox/rio-color)
- map-engine (zwishing/map-engine)
