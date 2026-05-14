# async-geotiff-rs Design Spec

Date: 2026-05-14
Status: Draft v2 (MVP) — revised after library deep-dive (see
`docs/superpowers/research/2026-05-14-library-deep-dive.md`)

## 1. Goal

A high-performance Rust crate that reads cloud-hosted GeoTIFFs (COGs) and serves
them as XYZ map tiles, comparable to Python's `rio-tiler` / `titiler`. Tiles are
rendered on the fly with styling controlled by URL query parameters.

Non-goals (v0): mosaic of multiple COGs, persistent tile cache, vector overlays,
QML/SLD/JSON style files, MBTiles output.

## 2. Scope (MVP)

In scope:
- Read COG via `async-tiff` (HTTP range reads through `object_store` reader).
- Two tile matrix sets: `WebMercatorQuad` (EPSG:3857) and `WorldCRS84Quad` (EPSG:4326),
  provided by the `tile-grid` crate.
- Source CRS = target CRS → fast path (no warp).
- Arbitrary source CRS → reprojected per-pixel via `proj`.
- Intermediate buffer: `ndarray::Array3<T>` (bands, height, width) + boolean mask.
- Single-band continuous data → colormap → RGBA PNG.
- Multi-band data → RGB band selection (`bidx=1,2,3`) → RGBA PNG.
- Titiler-style URL query parameter styling:
  - `bidx` band selection (1-indexed)
  - `rescale` per-band min/max stretch
  - `nodata` override
  - `colormap_name` (built-in palettes via `enterpolation`)
  - `resampling` (nearest/bilinear/lanczos)
  - `color_formula` (subset of rio-color: gamma, sigmoidal, saturation)
- Resize to tile size (default 256) via `fast_image_resize`.
- Output: PNG (v0). WebP/JPEG behind a feature flag, post-MVP.
- Demo HTTP server (axum) at `/tiles/{tms}/{z}/{x}/{y}.png?...`.

Reserved (trait stubs, no impl):
- `trait TileCache` — cache layer hook.
- `trait MosaicSource` — multi-COG mosaic hook.

## 3. Architecture

Single crate `async-geotiff` with modules (small files, high cohesion):

```
src/
  lib.rs              # re-exports, prelude
  error.rs            # AgError (thiserror), AgResult
  pixel.rs            # PixelData enum (U8/U16/I16/U32/I32/F32/F64) + dtype traits
  bounds.rs           # Bbox, Affine helpers wrapping `affine` crate
  crs.rs              # Crs wrapper around `proj`; equality + transform builder

  io/
    mod.rs
    cog.rs            # CogReader (async-tiff wrapper): metadata, overview pick, read_window
    overview.rs       # overview level selection by target resolution
    window.rs         # bbox -> pixel window, clamping, oversampling rules

  warp/
    mod.rs
    reproject.rs      # per-pixel inverse warp using proj + selected resampler
    resampler.rs      # Nearest, Bilinear, Lanczos (delegates to fast_image_resize for non-warp resize)

  render/
    mod.rs
    pipeline.rs       # RenderPipeline: rescale -> mask -> colormap -> color_formula -> resize -> encode
    rescale.rs        # min/max stretch, percentile-clip (post-MVP)
    colormap.rs       # built-in palettes, enterpolation-based 1D LUT
    color_formula.rs  # rio-color subset parser + apply (gamma, sigmoidal, saturation)
    encode.rs         # PNG (image crate) ; WebP/JPEG behind feature flag

  style/
    mod.rs
    query.rs          # parse titiler-style query string -> RenderSpec
    spec.rs           # RenderSpec struct (the parsed style)

  tms/
    mod.rs            # TileMatrixSet enum wrapping tile-grid; xyz -> bbox in TMS CRS

  tiler/
    mod.rs            # Tiler::tile(z,x,y, RenderSpec) -> Bytes ; orchestrates the pipeline

  cache.rs            # trait TileCache (no impl in v0)
  mosaic.rs           # trait MosaicSource (no impl in v0)

  server/             # behind feature flag `server`
    mod.rs
    routes.rs         # axum handlers: /tiles/{tms}/{z}/{x}/{y}.{fmt}
    state.rs          # AppState: Arc<Tiler>

examples/
  serve.rs            # `cargo run --example serve --features server -- <cog-url>`
benches/
  tile_bench.rs       # criterion: cold tile, warm tile, warp vs no-warp
tests/
  integration_*.rs    # local fixture COGs (small, checked-in or generated)
```

Justification: starts as one crate per Q1 confirmation. Module split mirrors a
future workspace boundary (`io`, `warp`, `render`, `style`, `tiler`, `server`),
so the eventual split is mechanical.

## 4. Core Types

```rust
pub enum PixelData {
    U8(Array3<u8>),
    U16(Array3<u16>),
    I16(Array3<i16>),
    U32(Array3<u32>),
    I32(Array3<i32>),
    F32(Array3<f32>),
    F64(Array3<f64>),
}

pub struct ImageData {
    pub pixels: PixelData,
    pub mask: Option<Array2<bool>>,   // true = valid
    pub bbox: Bbox,                   // in TMS CRS after warp
    pub crs: Crs,
}

pub struct RenderSpec {
    pub bidx: Vec<u32>,               // 1-indexed band selection
    pub rescale: Option<Vec<(f64, f64)>>, // per-band or single-band
    pub nodata: Option<f64>,
    pub colormap_name: Option<String>,
    pub resampling: Resampling,
    pub color_formula: Option<String>, // raw string; parsed in render
    pub tile_size: u32,                // default 256
}

pub struct Tiler {
    reader: Arc<CogReader>,
    src_crs: Crs,
    cache: Option<Arc<dyn TileCache>>,
}
```

Dispatch on `PixelData` uses a small macro to monomorphize hot paths (rescale,
warp, mask) per dtype.

## 5. Data Flow

```
Request (tms, z, x, y, RenderSpec)
  -> tms.tile_bbox(z, x, y)                       # bbox in TMS CRS
  -> if src_crs == dst_crs:
         pixel_window = affine.inverse(bbox)
         overview = pick_overview(window.res)
         pixels = cog.read_window(overview, window)
     else:
         bbox_src = proj(dst -> src).transform_bbox(bbox_dst)
         overview = pick_overview(bbox_src.res / tile_size)
         pixels_src = cog.read_window(overview, bbox_src)
         pixels = warp::reproject(pixels_src, src_affine, dst_grid, resampling)
  -> render_pipeline(pixels, mask, spec)
      |- rescale (per band)
      |- apply nodata -> mask -> alpha
      |- if single band: colormap (LUT via enterpolation)
         else: RGB compose from bidx
      |- color_formula (palette: gamma/sigmoidal/saturation)
      |- fast_image_resize to tile_size
      |- encode PNG
  -> Bytes
```

## 6. Key Design Decisions (revised after research)

1. **Overview selection**: pick the IFD whose `image_width / src_width_at_full_res`
   gives a GSD just smaller than the target tile GSD. Each overview is its own
   `ImageFileDirectory` in async-tiff (no built-in level abstraction).
2. **Warp strategy**: inverse-mapping at destination pixels via a **coarse 32×32
   sample grid through PROJ + bilinear interpolation** (gdalwarp pattern).
   Validate corners + center; subdivide on residual > 0.1 px. Avoids per-pixel
   PROJ calls (~50–500× speedup, sub-pixel error).
3. **PROJ threading**: `Proj` is `!Send + !Sync`. The warp + decode runs inside
   `tokio::task::spawn_blocking` and owns its own `Proj`. No shared
   `Mutex<Proj>` in hot loops.
4. **Decode is sync**: `Tile::decode` from async-tiff is CPU-bound and sync.
   Always call it from inside `spawn_blocking`.
5. **Reader**: `object_store::HttpBuilder` (or S3/GCS) + async-tiff
   `ObjectReader`. Never `ReqwestReader` (no range coalescing).
6. **Buffer layout**: `(H, W, C)` interleaved through the entire render
   pipeline. One permute-copy on ingest from async-tiff's planar `(C, H, W)`
   output. Matches `fast_image_resize::PixelType::U8x{3,4}` / `F32` and
   PNG encoder.
7. **Color formula DSL**: subset of rio-color — `gamma`, `sigmoidal`,
   `saturation`. Applied **left-to-right in sRGB**. Saturation goes
   `RGB→Lab→Lch`, scale C, back (via `palette::Lch`), not naive HSV — matches
   rio-color exactly. Sigmoidal short-circuits when `contrast == 0.0`.
   Regression-tested against the three numeric cases in the research doc.
8. **Colormaps**: `colorous` crate for matplotlib-faithful builtins (viridis /
   magma / plasma / inferno / cividis / turbo). `enterpolation::Linear` +
   `Clamp` only for **user-supplied** control points.
9. **GeoTransform**: `geo::AffineTransform<f64>` (no standalone `affine`
   crate exists). Internal `GeoTransform([f64;6])` newtype for ergonomics.
   Derive from async-tiff's `model_tiepoint()` + `model_pixel_scale()` or
   `model_transformation()`.
10. **Mask priority** (rio-tiler parity): alpha band > GDAL_NODATA value > none.
    Generate alpha channel from mask in the encode step.
11. **TMS**: `tile_grid::tms().lookup("WebMercatorQuad" | "WorldCRS84Quad")`.
    WebMercatorQuad is NW-origin and matches XYZ natively (no Y flip).
    Helper `slippy_to_tms_y(tms, z, y)` exists in `tms.rs` for SW-origin TMSs.
12. **PROJ axis-order trap**: `Proj::new_known_crs("EPSG:4326", ...)` normalises
    to `(lon, lat)`, opposite of pyproj 3 default. Documented in `crs.rs`.
13. **Errors**: `thiserror`-backed `AgError`; no panics across library boundary.
    URL parsing errors → 400; unknown TMS / unknown colormap → 422.
14. **No global state**: `Tiler` is `Send + Sync`, shareable as `Arc<Tiler>`
    for axum (`State<Arc<Tiler>>`).
15. **PROJ feature flag**: feature-gated. Fast path when `src_crs == dst_crs`
    skips the `proj` dependency entirely.

## 6b. Performance Principles

Adopted from `async-geotiff` (Python wrapper of async-tiff) — their explicit
performance commitments are exactly the right model for our Rust crate:

1. **Rust core throughout** — no Python in the hot path. We don't pay Python
   ↔ Rust crossings; the entire pipeline (read → decode → warp → render →
   encode) is monomorphic Rust.
2. **CPU/IO split with two pools** — tokio for IO (object_store range
   fetches), rayon for CPU (decode + warp + render + encode). Pattern mirrors
   async-geotiff's `tokio_rayon` integration:
   ```rust
   // sketch
   let cpu = Arc::new(rayon::ThreadPoolBuilder::new().build()?);
   let bytes = tokio::task::spawn_blocking({
       let cpu = cpu.clone();
       move || cpu.install(|| { decode_warp_render(...) })
   }).await??;
   ```
   The async executor is never blocked by decode, predictor unpacking, PROJ
   warp, or PNG encode.
3. **Request coalescing for adjacent tiles** — two tiers:
   - **Within one tile**: `object_store::ObjectStore::get_ranges` merges
     adjacent COG tile byte-ranges into batched GETs. `async-tiff`'s
     `ObjectReader::fetch_tiles(coords)` already drives this.
   - **Across concurrent tile requests** *(post-MVP optimisation)*: a per-COG
     request coalescer that batches in-flight `Range<u64>` reads sharing a
     near-window. Behind `trait CoalescingReader: AsyncFileReader` so the
     base path stays simple.
4. **Zero-copy data flow** — keep `ndarray::ArrayView*` borrowing across
   pipeline stages; only materialise (`to_owned`) at the unavoidable seams
   (planar → interleaved permute, encode). `Bytes` is reference-counted, so
   compressed tile payloads pass through to the decoder without copy.
5. **Buffer-protocol-ready boundary** — define `RasterArray { data: Array3<T>,
   mask, transform, crs, nodata }` as the seam between read / warp / render.
   When/if we add PyO3 bindings later, `Array3<T>` exports a Python buffer
   protocol view (same trick as `async_tiff::PyArray`) with no copy. Not
   implemented in MVP, but the API surface is shaped for it.
6. **Static colormap LUTs** — `colorous` constants compile in as
   `&[[u8;3]]`. No runtime LUT generation on the hot path; user-supplied
   stops compile to a 256-entry LUT once at request parse and are cached.
7. **Precomputed style** — for a given `RenderSpec`, the parser produces a
   `CompiledRenderSpec` (LUT baked, color formula AST resolved, rescale
   factors precomputed). The hot path is a single fused pass.
8. **Bounded concurrency** — `tokio::sync::Semaphore` caps in-flight
   `spawn_blocking` decode tasks at `num_cpus`. Avoids unbounded rayon queue
   growth under burst load.

## 6c. Render Module Types (borrowed from map-engine)

map-engine's `cmap.rs` + `colour.rs` (~400 LoC) already nails the rendering
core; we port these types nearly verbatim, then layer titiler-style URL
parsing + rio-color formula on top.

### Core types

```rust
// colour.rs — port directly
pub enum Colour {
    Seq([f64; 4]),    // (r,g,b,a) in [0,1]
    Hex(String),      // "#RRGGBBAA"
}

// cmap.rs — five-variant style definition; serde untagged for ergonomic JSON
#[derive(Deserialize)] #[serde(untagged)]
pub enum ColourDefinition {
    Named(String),                       // viridis / inferno / ... -> colorous
    Colours(Vec<Colour>),                // equally-spaced gradient on [vmin,vmax]
    ColoursAndBreaks(Vec<(f64, Colour)>),// custom break points
    Discrete(Vec<(i64, Colour)>),        // value -> colour (classified rasters)
    Rgb([f64; 3], [f64; 3]),             // per-band (vmin,vmax) for 3-band false colour
}

// Precompiled, hot-path-ready
pub struct Composite {
    vmin: Option<Vec<f64>>,
    vmax: Option<Vec<f64>>,
    gradient: Option<Gradient>,                  // built from colorous OR enterpolation
    lut:      Option<[[u8; 4]; 256]>,            // baked LUT for gradient fast-path
    discrete: Option<HashMap<i64, [u8; 4]>>,     // for Discrete variant
    kind:     CompositeKind,                     // Gradient | Discrete | Rgb
    bands:    usize,                             // 1 or 3
}

pub trait HandleGet {
    fn get(&self, values: &[f64], nodata: Option<&[f64]>) -> [u8; 4];
}
```

### Dispatch (`HandleGet for Composite`)

- `Discrete` → `HashMap` lookup on `value.trunc() as i64`. Missing → transparent.
- `Gradient` (Named / Colours / ColoursAndBreaks) → LUT-indexed by
  `((v - vmin) / (vmax - vmin) * 255).clamp(0, 255) as u8`. Nodata → alpha 0.
- `Rgb` → per-band linear rescale + clamp to [0,1] → RGB · 255. Any band
  matching nodata → fully transparent.

### Alpha policy

Binary alpha (0 or 255), driven by nodata match (`|v - ndv| < eps`).
Matches map-engine. Soft alpha is post-MVP.

### Differences from map-engine

| Aspect | map-engine | async-geotiff-rs |
|---|---|---|
| Built-in colormaps | hard-coded `VIRIDIS7` etc. (7 stops, palette interpolates) | `colorous` constants (full LUTs, exact matplotlib) |
| Style source | JSON config file, preloaded | titiler-style URL query params, parsed per-request |
| Post-rescale ops | none | rio-color `color_formula` (gamma / sigmoidal / saturation) inserted **after** rescale and **before** colormap-or-RGB pack |
| Reader | GDAL sync | `async-tiff` async + `spawn_blocking` decode |
| Output buffer | `Array3<u8>` planar | `Array3<u8>` interleaved (H,W,4) |
| Iteration | `ndarray::lanes(Axis(0))` + `Composite::get` per-pixel | `Zip::par_for_each` over `(H,W,4)` view + same `Composite::get` (rayon-parallel) |

### Lifecycle

1. **Request arrives** → parse query → `RenderSpec`.
2. **Compile once per request**: build `Composite` (LUT baked, color_formula
   AST resolved). Hot path is then a single fused pass:
   `read → warp → rescale → color_formula → composite.get → resize → encode`.
3. **`Composite` is `Send + Sync + Clone`** (just `Vec`s + `Option`s).
   Future: per-COG cache of recently-used `Composite`s if profiling shows
   compile cost matters.

### Portable code from map-engine

- ✅ `Colour` enum + serde detection (range auto-detect, hex parse).
- ✅ `ColourDefinition` enum + `#[serde(untagged)]`.
- ✅ `HandleGet` trait + three `*_handle` functions.
- ✅ `From<&Style> for Composite` constructor logic.
- ⚙️ Replace map-engine's `VIRIDIS7` consts with `colorous::VIRIDIS` lookup
   inside `From<&Style>` when `ColourDefinition::Named(s)` matches a known name.
- ⚙️ Add `bake_lut(&Gradient) -> [[u8;4]; 256]` so the gradient hot path is a
   pure array index, not a `palette::Gradient::get` call per pixel.

## 7. Public API Sketch

```rust
let reader = CogReader::open("https://.../scene.tif").await?;
let tiler  = Tiler::builder(reader).build()?;

// Titiler-style query (bidx and rescale are *repeated*, not CSV):
let spec = RenderSpec::from_query(
    "bidx=1&bidx=2&bidx=3&rescale=0,3000&rescale=0,3000&rescale=0,3000\
     &colormap_name=viridis&color_formula=gamma%20RGB%201.2&resampling=bilinear"
)?;
let png: Bytes = tiler.tile(Tms::WebMercatorQuad, 10, 512, 384, &spec).await?;
```

**URL contract** (titiler-compatible subset):
- Path: `/tiles/{tms}/{z}/{x}/{y}.{ext}` where `ext ∈ {png, jpeg, webp}`.
- Default `tilesize = 256` (we deviate from titiler's 512 for wider XYZ-client
  compatibility; client can override with `?tilesize=512`).
- `bidx` and `rescale` are repeated query params, **not CSV**.
- v0 supports: `bidx`, `rescale`, `nodata`, `unscale`, `resampling`, `reproject`,
  `color_formula`, `colormap_name`, `tilesize`, `return_mask`.
- Post-MVP: `expression`, custom `colormap` JSON, `algorithm` /
  `algorithm_params`, `buffer`, `padding`.

## 8. Testing

- Unit: query parsing, colormap LUT, color_formula parser, overview pick, bbox math.
- Integration: small fixture COGs (single-band f32 DEM, 3-band u8 RGB,
  16-bit Landsat-style); request known tiles; compare with reference PNGs
  (pixel-diff tolerance).
- Bench (criterion): cold tile vs warm tile; 3857 fast path vs 4326 with warp;
  resize cost; encode cost.
- Coverage target ≥ 80% per project rules.

## 9. Dependencies (initial Cargo.toml)

```toml
async-tiff        = "0.3"
object_store      = { version = "0.11", features = ["http","aws"] }
tile-grid         = "0.6"
proj              = { version = "0.27", optional = true }
geo               = "0.29"   # AffineTransform (no separate `affine` crate)
ndarray           = { version = "0.16", features = ["rayon"] }
fast_image_resize = { version = "5", features = ["rayon"] }
palette           = "0.7"    # for Lab/Lch saturation per rio-color
enterpolation     = "0.3"    # user-supplied colormap stops only
colorous          = "1"      # built-in matplotlib colormaps
image             = { version = "0.25", default-features = false, features = ["png"] }
bytemuck          = "1"
bytes             = "1"
nom               = "7"      # color_formula parser
tokio             = { version = "1", features = ["macros","rt-multi-thread","sync"] }
thiserror         = "1"
serde             = { version = "1", features = ["derive"] }
serde_urlencoded  = "0.7"
tracing           = "0.1"

# server feature
axum              = { version = "0.7", optional = true }

[features]
default = ["proj"]
proj    = ["dep:proj"]
server  = ["dep:axum"]
webp    = ["image/webp"]
```

## 10. Out-of-Scope / Future

- Mosaic across multiple COGs (`trait MosaicSource`).
- Tile cache backends (`trait TileCache` → moka / redis).
- WebP / AVIF / JPEG.
- Vector overlays.
- Style files (QML / SLD / native JSON).
- WMTS / TileJSON metadata endpoints.
- Streaming / partial-tile rendering.

## 11. Open Questions Resolved

- Q1 crate layout → single crate, modular layout (this doc).
- Q2 MVP scope → as above; mosaic/cache reserved as traits only.
- Style input → titiler-style URL query params; no file-based styles.
