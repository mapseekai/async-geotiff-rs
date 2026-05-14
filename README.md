# async-geotiff

Pure-Rust async crate that reads cloud-hosted Cloud-Optimized GeoTIFFs (COGs)
and serves them as XYZ map tiles with titiler-style URL parameter styling.

**No GDAL.** All raster IO goes through [`async-tiff`] + [`object_store`].
Reprojection through [`proj`] (libproj system dep, feature-gated). Affine math
through [`raffine`] (the author's own Rust port of Python's `affine`).
Colormaps via [`colorous`] + [`enterpolation`]. Resampling via
[`fast_image_resize`]. PNG encoding via [`image`].

## Status

MVP (v0.0.1). See:

- Design spec: `docs/superpowers/specs/2026-05-14-async-geotiff-rs-design.md`
- Library research: `docs/superpowers/research/2026-05-14-library-deep-dive.md`
- Implementation plan: `docs/superpowers/plans/2026-05-14-async-geotiff-rs.md`

## Quick start

```bash
cargo run --example serve --features server -- https://example.com/cog.tif
# then
curl 'http://localhost:8080/tiles/WebMercatorQuad/0/0/0.png?colormap_name=viridis&rescale=0,3000'
```

## URL contract (titiler-compatible subset)

Path: `/tiles/{tms}/{z}/{x}/{y}.{ext}` where `ext ∈ {png}` (`jpeg`/`webp`
behind feature flags post-MVP).

| Parameter | Repeat | Example | Notes |
|---|---|---|---|
| `bidx`          | yes | `?bidx=1&bidx=2&bidx=3` | 1-indexed band selection |
| `rescale`       | yes | `?rescale=0,3000&rescale=0,3000` | per-band `min,max` stretch |
| `nodata`        | no  | `?nodata=nan` | overrides dataset nodata (accepts `nan`, `inf`, `-inf`, float) |
| `colormap_name` | no  | `?colormap_name=viridis` | from `colorous`: viridis/magma/inferno/plasma/cividis/turbo/greys |
| `color_formula` | no  | `?color_formula=gamma%20RGB%201.5` | rio-color subset (`gamma`, `sigmoidal`, `saturation`) |
| `resampling`    | no  | `?resampling=bilinear` | `nearest` (default), `bilinear` |
| `reproject`     | no  | `?reproject=bilinear` | warp-time kernel |
| `tilesize`      | no  | `?tilesize=512` | default 256 |

Default tile size is **256** (not titiler's 512) for wider XYZ-client
compatibility; pass `?tilesize=512` to override.

## Supported TileMatrixSets

- `WebMercatorQuad` (EPSG:3857) — default for slippy XYZ clients
- `WorldCRS84Quad` (EPSG:4326)

## TMS supported pixel types

Source COG can be U8 / U16 / I16 / U32 / I32 / F32 / F64. Internally
unified to F64 at the read/warp seam.

## Build requirements

- Rust ≥ 1.85 (the 2026 crate ecosystem needs `edition2024` transitives;
  no `rust-toolchain.toml` is committed, so any stable ≥1.85 works).
- For the default `proj` feature: `libproj` 9.x on the system
  (`brew install proj` on macOS; `apt install libproj-dev libsqlite3-dev
  libtiff-dev clang` on Debian/Ubuntu).
  Build without it via `cargo build --no-default-features`.

## Architecture

```
HTTP request (axum)
  ├─ TMS.tile_bbox(z, x, y)            tile-grid
  ├─ CogReader.read_window              async-tiff + object_store
  │    └─ Tile::decode in spawn_blocking
  ├─ warp::warp (if src_crs ≠ dst_crs)  proj coarse-grid + bilinear
  │    └─ runs in spawn_blocking (Proj is !Send + !Sync)
  ├─ render pipeline:                   ndarray + palette
  │    ├─ rescale per band
  │    ├─ color_formula (sRGB Lab/Lch)
  │    └─ composite (LUT for gradient / HashMap for discrete / per-band rgb)
  └─ encode PNG                         image
```

Module layout:

```
src/
  error.rs            AgError + AgResult
  bounds.rs           Bbox + GeoTransform (wraps raffine::Affine)
  crs.rs              Crs identifier
  pixel.rs            PixelData + ImageData
  tms.rs              WebMercatorQuad + WorldCRS84Quad
  io/                 CogReader (async-tiff wrapper)
  warp/               Projector + Resampling + coarse-grid warp
  render/             colour, colormap, color_formula, composite, encode, pipeline
  style/              titiler-style URL query parser + compiler
  tiler.rs            orchestrator
  server/             axum routes (feature `server`)
```

## License

MIT OR Apache-2.0
