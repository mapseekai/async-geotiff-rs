# async-geotiff-rs Design Spec

Date: 2026-05-14
Status: Draft v1 (MVP)

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

## 6. Key Design Decisions

1. **Overview selection**: pick first overview level with `res <= target_res`;
   fall back to base. Avoid reading at full res when zoom is low.
2. **Warp strategy**: inverse-mapping at destination pixels (rio-tiler parity)
   minimises source-window size and avoids whole-image materialisation.
3. **Concurrency**: `async-tiff` handles per-chunk concurrency inside one read.
   Higher-level fanout (many tiles) is the caller's responsibility.
4. **Color formula**: parse a subset of rio-color's mini-DSL:
   - `gamma RGB 1.5`
   - `sigmoidal RGB 5 0.5`
   - `saturation 1.2`
   Implemented via `palette` (sRGB <-> linear / HSV) operating on `Array3<f32>`.
5. **Built-in colormaps**: viridis, magma, inferno, plasma, cividis, gray, terrain.
   Encoded as compile-time `[[f32; 3]; 256]` LUTs, with `enterpolation` for
   arbitrary-length / user-supplied control points.
6. **Errors**: `thiserror`-backed `AgError`; no panics across library boundary.
7. **No global state**: `Tiler` is `Send + Sync`, shareable as `Arc<Tiler>` for
   axum (`State<Arc<Tiler>>`).
8. **PROJ dependency**: feature-gated `proj` (system lib). Fast path when
   `src_crs == dst_crs` does not require PROJ.

## 7. Public API Sketch

```rust
let reader = CogReader::open("https://.../scene.tif").await?;
let tiler = Tiler::builder(reader).build()?;

let spec = RenderSpec::from_query("bidx=1&rescale=0,3000&colormap_name=viridis")?;
let png: Bytes = tiler.tile(TileMatrixSet::WebMercatorQuad, 10, 512, 384, &spec).await?;
```

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
async-tiff       = "*"   # pin once published version known
tile-grid        = "*"
proj             = { version = "*", optional = true }
ndarray          = "*"
affine           = "*"   # the `affine` crate
fast_image_resize= "*"
palette          = "*"
enterpolation    = "*"
image            = { version = "*", default-features = false, features = ["png"] }
bytes            = "*"
tokio            = { version = "*", features = ["macros","rt-multi-thread"] }
thiserror        = "*"
serde            = { version = "*", features = ["derive"] }
serde_urlencoded = "*"
tracing          = "*"

# server feature
axum             = { version = "*", optional = true }

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
