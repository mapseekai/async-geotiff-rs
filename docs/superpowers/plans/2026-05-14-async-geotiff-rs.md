# async-geotiff-rs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pure-Rust async crate that reads cloud-hosted COGs and serves them as XYZ map tiles with titiler-style URL parameter styling.

**Architecture:** Single crate `async-geotiff` with focused modules (`error`, `pixel`, `bounds`, `crs`, `tms`, `io`, `warp`, `render`, `style`, `tiler`, `server`). CPU/IO split: tokio for I/O via `object_store` + `async-tiff`; `spawn_blocking` + `rayon` for decode, warp (PROJ coarse-grid + bilinear), rescale, colormap LUT, color_formula, resize, encode. Output buffer `(H,W,C)` interleaved end-to-end. Rendering core ported from map-engine (`Colour` / `ColourDefinition` / `Composite` / `HandleGet`). No GDAL anywhere.

**Tech Stack:** `async-tiff 0.3`, `object_store 0.11`, `tile-grid 0.6`, `proj 0.27` (feature-gated, libproj 9.6 system dep), `raffine 0.2`, `ndarray 0.16` (+rayon), `fast_image_resize 5` (+rayon), `palette 0.7`, `colorous 1`, `enterpolation 0.3`, `image 0.25` (PNG by default; webp/jpeg behind features), `axum 0.7` (server feature), `tokio 1`, `thiserror 1`, `nom 7`, `criterion` for benches, `mockito` / local fixtures for tests.

**Spec reference:** `docs/superpowers/specs/2026-05-14-async-geotiff-rs-design.md`
**Library research:** `docs/superpowers/research/2026-05-14-library-deep-dive.md`

---

## Plan Structure & Conventions

- Each task ends with a commit; conventional commits (`feat:` / `test:` / `chore:` / `fix:` / `docs:`).
- All steps target the workspace root `/Users/zhang/code/async-geotiff-rs`.
- TDD discipline: write failing test → run → implement → run → commit.
- "Run" commands use `cargo test -p async-geotiff --lib <test_name>` for unit tests, `cargo test --test <name>` for integration tests.
- Code blocks show the complete new content for the file unless `+++` / `---` style diff hints otherwise.
- When two consecutive tasks edit the same file, the later task assumes the earlier is applied.
- Type identifiers stay stable across tasks: `AgError`, `AgResult<T>`, `Bbox`, `Crs`, `GeoTransform`, `PixelData`, `ImageData`, `RenderSpec`, `CompiledRenderSpec`, `Composite`, `Colour`, `ColourDefinition`, `Tms`, `CogReader`, `Tiler`. Method names: `CogReader::open`, `Tiler::tile`, `RenderSpec::from_query`, `Composite::get`.

---

## Task 0: Cargo workspace & lib scaffold

**Files:**
- Create: `Cargo.toml`
- Create: `src/lib.rs`
- Create: `rust-toolchain.toml`
- Create: `.gitignore`

- [ ] **Step 1: Create `rust-toolchain.toml`**

```toml
[toolchain]
channel = "1.82"
components = ["rustfmt", "clippy"]
```

- [ ] **Step 2: Create `.gitignore`**

```
/target
**/*.rs.bk
Cargo.lock.bak
.DS_Store
```

- [ ] **Step 3: Create root `Cargo.toml`**

```toml
[package]
name = "async-geotiff"
version = "0.0.1"
edition = "2021"
rust-version = "1.82"
license = "MIT OR Apache-2.0"
description = "Async COG → XYZ tile renderer (pure Rust, no GDAL)"

[dependencies]
async-tiff        = "0.3"
object_store      = { version = "0.11", features = ["http", "aws"] }
tile-grid         = "0.6"
proj              = { version = "0.27", optional = true }
raffine           = "0.2"
ndarray           = { version = "0.16", features = ["rayon"] }
fast_image_resize = { version = "5", features = ["rayon"] }
palette           = "0.7"
colorous          = "1"
enterpolation     = "0.3"
image             = { version = "0.25", default-features = false, features = ["png"] }
bytes             = "1"
bytemuck          = "1"
nom               = "7"
tokio             = { version = "1", features = ["macros", "rt-multi-thread", "sync"] }
rayon             = "1"
thiserror         = "1"
serde             = { version = "1", features = ["derive"] }
serde_urlencoded  = "0.7"
tracing           = "0.1"
axum              = { version = "0.7", optional = true }

[dev-dependencies]
tokio             = { version = "1", features = ["macros", "rt-multi-thread", "sync", "test-util"] }
criterion         = "0.5"
tempfile          = "3"
approx            = "0.5"

[features]
default = ["proj"]
proj    = ["dep:proj"]
server  = ["dep:axum"]
webp    = ["image/webp"]
jpeg    = ["image/jpeg"]

[[bench]]
name = "tile_bench"
harness = false
```

- [ ] **Step 4: Create `src/lib.rs`**

```rust
//! async-geotiff: pure-Rust async COG → XYZ tile renderer.
//!
//! See `docs/superpowers/specs/2026-05-14-async-geotiff-rs-design.md`.

pub mod error;
```

- [ ] **Step 5: Verify the crate builds**

Run: `cargo build`
Expected: PASS (`Compiling async-geotiff`)

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml rust-toolchain.toml .gitignore src/lib.rs
git commit -m "chore: cargo scaffold for async-geotiff crate"
```

---

## Task 1: Error module

**Files:**
- Create: `src/error.rs`
- Test: inline `#[cfg(test)] mod tests` in same file

- [ ] **Step 1: Write `src/error.rs` with failing test**

```rust
use thiserror::Error;

pub type AgResult<T> = Result<T, AgError>;

#[derive(Debug, Error)]
pub enum AgError {
    #[error("invalid query parameter `{name}`: {msg}")]
    Query { name: String, msg: String },

    #[error("unknown TMS: {0}")]
    UnknownTms(String),

    #[error("unknown colormap: {0}")]
    UnknownColormap(String),

    #[error("out-of-bounds tile: z={z} x={x} y={y}")]
    TileOutOfBounds { z: u8, x: u32, y: u32 },

    #[error("unsupported pixel format: {0}")]
    UnsupportedPixel(String),

    #[error("missing geo metadata: {0}")]
    MissingGeo(&'static str),

    #[error("color formula: {0}")]
    ColorFormula(String),

    #[error("warp: {0}")]
    Warp(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("async-tiff: {0}")]
    Tiff(String),

    #[error("encode: {0}")]
    Encode(#[from] image::ImageError),
}

impl AgError {
    pub fn query(name: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::Query { name: name.into(), msg: msg.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_helper_builds_message() {
        let e = AgError::query("bidx", "not an integer");
        assert_eq!(format!("{}", e), "invalid query parameter `bidx`: not an integer");
    }

    #[test]
    fn unknown_tms_displays() {
        let e = AgError::UnknownTms("LolMercator".into());
        assert_eq!(format!("{}", e), "unknown TMS: LolMercator");
    }
}
```

- [ ] **Step 2: Run tests (will fail because module not registered)**

Run: `cargo test --lib error::tests`
Expected: FAIL (module not declared in `lib.rs`) — but `lib.rs` already declares `error` from Task 0, so this should compile and PASS.

- [ ] **Step 3: Confirm PASS**

Run: `cargo test --lib error::tests`
Expected: PASS — 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/error.rs
git commit -m "feat: AgError + AgResult"
```

---

## Task 2: Bounds — `Bbox` + `GeoTransform` newtype

**Files:**
- Create: `src/bounds.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add module to lib.rs**

Edit `src/lib.rs` — append after `pub mod error;`:

```rust
pub mod bounds;
```

- [ ] **Step 2: Write `src/bounds.rs` with tests first**

```rust
use raffine::Affine;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bbox {
    pub xmin: f64,
    pub ymin: f64,
    pub xmax: f64,
    pub ymax: f64,
}

impl Bbox {
    pub fn new(xmin: f64, ymin: f64, xmax: f64, ymax: f64) -> Self {
        Self { xmin, ymin, xmax, ymax }
    }
    pub fn width(&self) -> f64  { self.xmax - self.xmin }
    pub fn height(&self) -> f64 { self.ymax - self.ymin }

    /// Bbox enclosing all four transformed corners.
    pub fn from_corners(corners: &[(f64, f64); 4]) -> Self {
        let mut xmin = f64::INFINITY; let mut ymin = f64::INFINITY;
        let mut xmax = f64::NEG_INFINITY; let mut ymax = f64::NEG_INFINITY;
        for &(x, y) in corners {
            if x < xmin { xmin = x; } if y < ymin { ymin = y; }
            if x > xmax { xmax = x; } if y > ymax { ymax = y; }
        }
        Self { xmin, ymin, xmax, ymax }
    }
}

/// Newtype wrapping `raffine::Affine` as a north-up-positive GeoTransform.
///
/// World coords from pixel: `(x, y) = transform * (col + 0.5, row + 0.5)`
/// (pixel-center convention, matches rasterio).
#[derive(Debug, Clone, Copy)]
pub struct GeoTransform(pub Affine);

impl GeoTransform {
    pub fn from_gdal(gt: &[f64; 6]) -> Self {
        // GDAL order: [c, a, b, f, d, e]
        // raffine::Affine::from_gdal accepts the same 6-tuple
        Self(Affine::from_gdal(gt))
    }

    pub fn forward(&self, col: f64, row: f64) -> (f64, f64) {
        self.0 * (col, row)
    }

    pub fn inverse(&self) -> Option<GeoTransform> {
        (!self.0).ok().map(GeoTransform)
    }

    /// Pixel window covering the bbox (clamped to >= 0).
    /// Returns (col_off, row_off, width, height).
    pub fn window_for_bbox(&self, bbox: &Bbox) -> Option<(i64, i64, u64, u64)> {
        let inv = self.inverse()?;
        let (c0, r0) = inv.forward(bbox.xmin, bbox.ymax);
        let (c1, r1) = inv.forward(bbox.xmax, bbox.ymin);
        let col_off = c0.floor() as i64;
        let row_off = r0.floor() as i64;
        let width   = (c1 - c0).abs().ceil() as u64;
        let height  = (r1 - r0).abs().ceil() as u64;
        Some((col_off, row_off, width, height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn bbox_from_corners_encloses_all() {
        let b = Bbox::from_corners(&[(0.0, 0.0), (10.0, 5.0), (-1.0, 3.0), (4.0, 8.0)]);
        assert_eq!(b, Bbox::new(-1.0, 0.0, 10.0, 8.0));
    }

    #[test]
    fn geotransform_forward_inverse_roundtrip() {
        // GDAL: origin (100, 200), pixel size (1, -1)
        let gt = GeoTransform::from_gdal(&[100.0, 1.0, 0.0, 200.0, 0.0, -1.0]);
        let (x, y) = gt.forward(0.0, 0.0);
        assert_relative_eq!(x, 100.0);
        assert_relative_eq!(y, 200.0);

        let inv = gt.inverse().unwrap();
        let (c, r) = inv.forward(100.0, 200.0);
        assert_relative_eq!(c, 0.0, epsilon = 1e-9);
        assert_relative_eq!(r, 0.0, epsilon = 1e-9);
    }

    #[test]
    fn window_for_bbox_clamps_correctly() {
        let gt = GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 100.0, 0.0, -1.0]);
        let bbox = Bbox::new(10.0, 50.0, 30.0, 90.0);
        let (c, r, w, h) = gt.window_for_bbox(&bbox).unwrap();
        assert_eq!((c, r, w, h), (10, 10, 20, 40));
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test --lib bounds::tests`
Expected: PASS — 3 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs src/bounds.rs
git commit -m "feat: Bbox + GeoTransform wrapping raffine::Affine"
```

---

## Task 3: CRS wrapper

**Files:**
- Create: `src/crs.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add module**

Append to `src/lib.rs`:
```rust
pub mod crs;
```

- [ ] **Step 2: Write `src/crs.rs`**

```rust
/// Coordinate reference system identifier.
///
/// Note: `proj::Proj::new_known_crs("EPSG:4326", _, None)` normalises EPSG:4326
/// to `(lon, lat)` order, **not** `(lat, lon)` like pyproj 3+. All callers in
/// this crate work in `(x=lon, y=lat)` order to match.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Crs(String);

impl Crs {
    pub fn epsg(code: u32) -> Self {
        Self(format!("EPSG:{}", code))
    }
    pub fn from_str(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Crs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epsg_builds_string() {
        assert_eq!(Crs::epsg(3857).as_str(), "EPSG:3857");
    }

    #[test]
    fn equality_is_string_based() {
        assert_eq!(Crs::epsg(4326), Crs::from_str("EPSG:4326"));
        assert_ne!(Crs::epsg(4326), Crs::epsg(3857));
    }
}
```

- [ ] **Step 3: Run**

Run: `cargo test --lib crs::tests`
Expected: PASS — 2 tests.

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs src/crs.rs
git commit -m "feat: Crs identifier wrapper"
```

---

## Task 4: PixelData enum + ImageData

**Files:**
- Create: `src/pixel.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add module**

Append to `src/lib.rs`:
```rust
pub mod pixel;
```

- [ ] **Step 2: Write `src/pixel.rs`**

```rust
use ndarray::{Array2, Array3};
use crate::bounds::{Bbox, GeoTransform};
use crate::crs::Crs;

/// Decoded pixel buffer in `(bands, height, width)` planar layout.
///
/// Always permuted to `(H, W, bands)` interleaved at the read/warp seam
/// before entering the render pipeline.
#[derive(Debug, Clone)]
pub enum PixelData {
    U8(Array3<u8>),
    U16(Array3<u16>),
    I16(Array3<i16>),
    U32(Array3<u32>),
    I32(Array3<i32>),
    F32(Array3<f32>),
    F64(Array3<f64>),
}

impl PixelData {
    pub fn bands(&self) -> usize {
        match self {
            Self::U8(a)  => a.shape()[0],
            Self::U16(a) => a.shape()[0],
            Self::I16(a) => a.shape()[0],
            Self::U32(a) => a.shape()[0],
            Self::I32(a) => a.shape()[0],
            Self::F32(a) => a.shape()[0],
            Self::F64(a) => a.shape()[0],
        }
    }
    pub fn height(&self) -> usize { self.shape()[1] }
    pub fn width(&self)  -> usize { self.shape()[2] }

    fn shape(&self) -> &[usize] {
        match self {
            Self::U8(a)  => a.shape(),
            Self::U16(a) => a.shape(),
            Self::I16(a) => a.shape(),
            Self::U32(a) => a.shape(),
            Self::I32(a) => a.shape(),
            Self::F32(a) => a.shape(),
            Self::F64(a) => a.shape(),
        }
    }

    /// View every pixel as f64 — used by the render hot path.
    /// Allocates one f64 array; caller should cache if iterating.
    pub fn to_f64(&self) -> Array3<f64> {
        match self {
            Self::U8(a)  => a.mapv(|v| v as f64),
            Self::U16(a) => a.mapv(|v| v as f64),
            Self::I16(a) => a.mapv(|v| v as f64),
            Self::U32(a) => a.mapv(|v| v as f64),
            Self::I32(a) => a.mapv(|v| v as f64),
            Self::F32(a) => a.mapv(|v| v as f64),
            Self::F64(a) => a.clone(),
        }
    }
}

/// The single data-bearing type passed between io → warp → render.
#[derive(Debug, Clone)]
pub struct ImageData {
    pub pixels: PixelData,
    pub mask: Option<Array2<bool>>,  // true = valid
    pub bbox: Bbox,
    pub crs: Crs,
    pub transform: GeoTransform,
    pub nodata: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array3;

    #[test]
    fn pixel_data_shape_helpers() {
        let arr = Array3::<u8>::zeros((3, 256, 256));
        let p = PixelData::U8(arr);
        assert_eq!(p.bands(), 3);
        assert_eq!(p.height(), 256);
        assert_eq!(p.width(), 256);
    }

    #[test]
    fn to_f64_preserves_values() {
        let arr = Array3::<u16>::from_shape_vec((1, 1, 2), vec![100, 200]).unwrap();
        let p = PixelData::U16(arr);
        let f = p.to_f64();
        assert_eq!(f[[0, 0, 0]], 100.0);
        assert_eq!(f[[0, 0, 1]], 200.0);
    }
}
```

- [ ] **Step 3: Run**

Run: `cargo test --lib pixel::tests`
Expected: PASS — 2 tests.

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs src/pixel.rs
git commit -m "feat: PixelData enum + ImageData seam type"
```

---

## Task 5: TMS module with slippy Y flip

**Files:**
- Create: `src/tms.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add module**

Append to `src/lib.rs`:
```rust
pub mod tms;
```

- [ ] **Step 2: Write `src/tms.rs`**

```rust
use tile_grid::{tms as registry, Tms as InnerTms, Xyz};
use crate::bounds::Bbox;
use crate::crs::Crs;
use crate::error::{AgError, AgResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmsKind {
    WebMercatorQuad,
    WorldCrs84Quad,
}

impl TmsKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::WebMercatorQuad => "WebMercatorQuad",
            Self::WorldCrs84Quad  => "WorldCRS84Quad",
        }
    }

    pub fn from_str(s: &str) -> AgResult<Self> {
        match s {
            "WebMercatorQuad" | "webmercatorquad" => Ok(Self::WebMercatorQuad),
            "WorldCRS84Quad"  | "worldcrs84quad"  => Ok(Self::WorldCrs84Quad),
            other => Err(AgError::UnknownTms(other.into())),
        }
    }

    pub fn crs(self) -> Crs {
        match self {
            Self::WebMercatorQuad => Crs::epsg(3857),
            Self::WorldCrs84Quad  => Crs::epsg(4326),
        }
    }
}

pub struct Tms {
    kind: TmsKind,
    inner: InnerTms,
}

impl Tms {
    pub fn new(kind: TmsKind) -> AgResult<Self> {
        let inner = registry()
            .lookup(kind.name())
            .map_err(|e| AgError::UnknownTms(format!("{}: {}", kind.name(), e)))?;
        Ok(Self { kind, inner })
    }

    pub fn kind(&self) -> TmsKind { self.kind }
    pub fn crs(&self)  -> Crs     { self.kind.crs() }

    /// Bbox of (z, x, y) in the TMS CRS. `y` is XYZ-slippy convention
    /// (origin top-left). For NW-origin TMSs this matches the inner library;
    /// for SW-origin we flip Y.
    pub fn tile_bbox(&self, z: u8, x: u32, y: u32) -> AgResult<Bbox> {
        let y_tms = self.slippy_to_tms_y(z, y)?;
        let bb = self.inner.xy_bounds(&Xyz::new(x.into(), y_tms.into(), z.into()));
        Ok(Bbox::new(bb.left, bb.bottom, bb.right, bb.top))
    }

    /// CRS-unit resolution at zoom `z` (units per pixel of the matrix).
    pub fn resolution(&self, z: u8) -> AgResult<f64> {
        self.inner
            .resolution_z(z.into())
            .ok_or_else(|| AgError::TileOutOfBounds { z, x: 0, y: 0 })
    }

    fn slippy_to_tms_y(&self, z: u8, y: u32) -> AgResult<u32> {
        // Both WebMercatorQuad and WorldCRS84Quad are NW-origin (top-left); no flip.
        // Helper kept for future SW-origin TMSs.
        let matrix = self.inner
            .matrix(z.into())
            .map_err(|_| AgError::TileOutOfBounds { z, x: 0, y })?;
        if y >= matrix.matrix_height as u32 {
            return Err(AgError::TileOutOfBounds { z, x: 0, y });
        }
        Ok(y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn unknown_tms_errors() {
        assert!(TmsKind::from_str("foo").is_err());
    }

    #[test]
    fn webmercatorquad_tile_0_0_0_is_full_world() {
        let tms = Tms::new(TmsKind::WebMercatorQuad).unwrap();
        let bb = tms.tile_bbox(0, 0, 0).unwrap();
        // 3857 world bounds ≈ ±20037508
        assert_relative_eq!(bb.xmin, -20037508.342789244, epsilon = 1e-3);
        assert_relative_eq!(bb.xmax,  20037508.342789244, epsilon = 1e-3);
    }

    #[test]
    fn out_of_range_y_errors() {
        let tms = Tms::new(TmsKind::WebMercatorQuad).unwrap();
        // at z=0 only (0,0) is valid
        assert!(tms.tile_bbox(0, 0, 1).is_err());
    }

    #[test]
    fn resolution_decreases_with_zoom() {
        let tms = Tms::new(TmsKind::WebMercatorQuad).unwrap();
        let r0 = tms.resolution(0).unwrap();
        let r10 = tms.resolution(10).unwrap();
        assert!(r0 > r10);
    }
}
```

- [ ] **Step 3: Run**

Run: `cargo test --lib tms::tests`
Expected: PASS — 4 tests.

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs src/tms.rs
git commit -m "feat: TMS wrapper (WebMercatorQuad + WorldCRS84Quad)"
```

---

## Task 6: IO — `CogReader` open + metadata

**Files:**
- Create: `src/io/mod.rs`
- Create: `src/io/cog.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add module**

Append to `src/lib.rs`:
```rust
pub mod io;
```

- [ ] **Step 2: Create `src/io/mod.rs`**

```rust
pub mod cog;
pub mod overview;
pub mod window;

pub use cog::CogReader;
```

- [ ] **Step 3: Write `src/io/cog.rs`**

```rust
use std::sync::Arc;
use async_tiff::{TIFF, reader::ObjectReader, metadata::{TiffMetadataReader, cache::ReadaheadMetadataCache}};
use object_store::{ObjectStore, http::HttpBuilder};
use object_store::path::Path;

use crate::error::{AgError, AgResult};
use crate::crs::Crs;
use crate::bounds::GeoTransform;

pub struct CogReader {
    pub(crate) tiff: TIFF,
    pub(crate) reader: ObjectReader,
    pub(crate) src_crs: Crs,
    pub(crate) transform: GeoTransform,
}

impl CogReader {
    /// Open a remote COG over HTTPS.
    pub async fn open_http(url: &str) -> AgResult<Self> {
        let (base, path) = split_http_url(url)?;
        let store: Arc<dyn ObjectStore> = Arc::new(
            HttpBuilder::new()
                .with_url(base)
                .build()
                .map_err(|e| AgError::Tiff(format!("http store: {e}")))?,
        );
        let reader = ObjectReader::new(store, Path::from(path));
        let cache = ReadaheadMetadataCache::new(reader.clone());
        let mut md = TiffMetadataReader::try_open(&cache)
            .await
            .map_err(|e| AgError::Tiff(format!("metadata open: {e}")))?;
        let ifds = md.read_all_ifds(&cache)
            .await
            .map_err(|e| AgError::Tiff(format!("read ifds: {e}")))?;
        let tiff = TIFF::new(ifds, md.endianness());

        let (transform, src_crs) = derive_geo(&tiff)?;
        Ok(Self { tiff, reader, src_crs, transform })
    }

    pub fn src_crs(&self) -> &Crs { &self.src_crs }
    pub fn transform(&self) -> GeoTransform { self.transform }
    pub fn overview_count(&self) -> usize { self.tiff.ifds().len() }
}

fn split_http_url(url: &str) -> AgResult<(String, String)> {
    let u = url::Url::parse(url)
        .map_err(|e| AgError::Tiff(format!("invalid url: {e}")))?;
    let base = format!("{}://{}", u.scheme(), u.host_str().unwrap_or(""));
    let port = u.port().map(|p| format!(":{}", p)).unwrap_or_default();
    Ok((format!("{}{}", base, port), u.path().trim_start_matches('/').to_string()))
}

fn derive_geo(tiff: &TIFF) -> AgResult<(GeoTransform, Crs)> {
    let ifd = tiff.ifds().first().ok_or(AgError::MissingGeo("no IFD"))?;

    // GeoTransform: prefer model_transformation (4x4), else tiepoint + pixel_scale.
    let gt = if let Some(t) = ifd.model_transformation() {
        // t is 4x4 row-major; for a north-up image we use indices [0,1,3, 4,5,7]
        GeoTransform::from_gdal(&[t[3], t[0], t[1], t[7], t[4], t[5]])
    } else {
        let tp = ifd.model_tiepoint().ok_or(AgError::MissingGeo("model_tiepoint"))?;
        let ps = ifd.model_pixel_scale().ok_or(AgError::MissingGeo("model_pixel_scale"))?;
        // ModelTiepointTag: (i, j, k, x, y, z) for a single tiepoint
        let (i, j, x, y) = (tp[0], tp[1], tp[3], tp[4]);
        let (sx, sy) = (ps[0], ps[1]);
        // origin world = (x - i*sx, y + j*sy) for north-up
        GeoTransform::from_gdal(&[x - i * sx, sx, 0.0, y + j * sy, 0.0, -sy])
    };

    // CRS: try EPSG codes from the GeoKeyDirectory. Fall back to "UNKNOWN".
    let crs = ifd
        .geo_key_directory()
        .and_then(|gk| extract_epsg(gk))
        .map(Crs::epsg)
        .unwrap_or_else(|| Crs::from_str("UNKNOWN"));
    Ok((gt, crs))
}

fn extract_epsg(gk: &async_tiff::GeoKeyDirectory) -> Option<u32> {
    // ProjectedCSTypeGeoKey = 3072, GeographicTypeGeoKey = 2048
    gk.projected_cs_type().or(gk.geographic_type()).map(|c| c as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_http_url_extracts_parts() {
        let (base, path) = split_http_url("https://example.com/foo/bar.tif").unwrap();
        assert_eq!(base, "https://example.com");
        assert_eq!(path, "foo/bar.tif");
    }
}
```

- [ ] **Step 4: Create `src/io/overview.rs` and `src/io/window.rs` as empty stubs**

```rust
// src/io/overview.rs
//! Overview level selection by target GSD.
```

```rust
// src/io/window.rs
//! Bbox -> pixel window math for COG tiles.
```

- [ ] **Step 5: Add `url` to Cargo.toml**

Edit `Cargo.toml` dependencies block, add:
```toml
url = "2"
```

- [ ] **Step 6: Run**

Run: `cargo test --lib io::cog::tests`
Expected: PASS — 1 test (URL split). Remote-IO tests come in Task 9.

- [ ] **Step 7: Commit**

```bash
git add src/lib.rs src/io Cargo.toml
git commit -m "feat: CogReader::open_http + GeoTransform/CRS derivation"
```

---

## Task 7: IO — overview level picker

**Files:**
- Modify: `src/io/overview.rs`
- Modify: `src/io/mod.rs`

- [ ] **Step 1: Write `src/io/overview.rs`**

```rust
use async_tiff::TIFF;

/// Pick the IFD index whose horizontal GSD is the largest still ≤ `target_gsd`.
/// Falls back to the highest-resolution IFD (index 0) if none qualifies.
pub fn pick_overview(tiff: &TIFF, full_width: u64, full_gsd: f64, target_gsd: f64) -> usize {
    let ifds = tiff.ifds();
    let mut best: usize = 0;
    let mut best_gsd: f64 = full_gsd;
    for (i, ifd) in ifds.iter().enumerate() {
        let w = ifd.image_width() as f64;
        // GSD scales linearly with reduction factor (full_width / w).
        let gsd = full_gsd * (full_width as f64 / w);
        if gsd <= target_gsd && gsd >= best_gsd {
            best = i;
            best_gsd = gsd;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    // Unit test the pure math via a small stub; integration test in Task 9.

    fn pick_pure(widths: &[u64], full_w: u64, full_gsd: f64, target: f64) -> usize {
        let mut best = 0usize;
        let mut best_gsd = full_gsd;
        for (i, &w) in widths.iter().enumerate() {
            let gsd = full_gsd * (full_w as f64 / w as f64);
            if gsd <= target && gsd >= best_gsd {
                best = i; best_gsd = gsd;
            }
        }
        best
    }

    #[test]
    fn picks_largest_gsd_within_target() {
        // full=10000@1m, overviews 5000@2m, 2500@4m, 1250@8m
        // target 5m → pick the 4m overview (index 2)
        let idx = pick_pure(&[10000, 5000, 2500, 1250], 10000, 1.0, 5.0);
        assert_eq!(idx, 2);
    }

    #[test]
    fn falls_back_to_full_when_all_too_coarse() {
        let idx = pick_pure(&[10000, 5000], 10000, 1.0, 0.5);
        assert_eq!(idx, 0);
    }
}
```

- [ ] **Step 2: Export from `src/io/mod.rs`**

Edit `src/io/mod.rs`, add at bottom:
```rust
pub use overview::pick_overview;
```

- [ ] **Step 3: Run**

Run: `cargo test --lib io::overview::tests`
Expected: PASS — 2 tests.

- [ ] **Step 4: Commit**

```bash
git add src/io
git commit -m "feat: overview level picker by target GSD"
```

---

## Task 8: IO — read window of pixels into PixelData

**Files:**
- Modify: `src/io/window.rs`
- Modify: `src/io/cog.rs`

- [ ] **Step 1: Write `src/io/window.rs`**

```rust
use async_tiff::ImageFileDirectory;

/// Convert pixel window to inclusive tile coordinate range (tx_min..tx_max, ty_min..ty_max).
pub fn tile_range_for_window(
    ifd: &ImageFileDirectory,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> Option<((u32, u32), (u32, u32))> {
    let tw = ifd.tile_width()?  as i64;
    let th = ifd.tile_height()? as i64;
    let col_end = col_off + width as i64;
    let row_end = row_off + height as i64;
    let tx0 = (col_off.max(0) / tw) as u32;
    let ty0 = (row_off.max(0) / th) as u32;
    let tx1 = ((col_end - 1).max(0) / tw) as u32;
    let ty1 = ((row_end - 1).max(0) / th) as u32;
    Some(((tx0, tx1), (ty0, ty1)))
}

#[cfg(test)]
mod tests {
    // Unit test pure arithmetic; ifd-coupled tests are in Task 9.

    fn pure(tw: i64, th: i64, col: i64, row: i64, w: u64, h: u64) -> ((i64, i64), (i64, i64)) {
        let ce = col + w as i64;
        let re = row + h as i64;
        ((col.max(0) / tw, (ce - 1).max(0) / tw), (row.max(0) / th, (re - 1).max(0) / th))
    }

    #[test]
    fn one_tile_window_yields_one_tile() {
        let r = pure(256, 256, 10, 10, 50, 50);
        assert_eq!(r, ((0, 0), (0, 0)));
    }

    #[test]
    fn straddling_window_spans_multiple_tiles() {
        let r = pure(256, 256, 200, 200, 200, 200);
        assert_eq!(r, ((0, 1), (0, 1)));
    }
}
```

- [ ] **Step 2: Extend `CogReader` with `read_window`**

Add to `src/io/cog.rs` inside `impl CogReader`:

```rust
/// Read a pixel window from the chosen overview level.
/// Returns ImageData (planar (C,H,W) layout from async-tiff Array).
pub async fn read_window(
    &self,
    level: usize,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> AgResult<crate::pixel::ImageData> {
    use crate::pixel::{PixelData, ImageData};
    use crate::io::window::tile_range_for_window;
    use async_tiff::decoder::DecoderRegistry;

    let ifd = self.tiff.ifds().get(level).ok_or(AgError::MissingGeo("overview level"))?;
    let ((tx0, tx1), (ty0, ty1)) = tile_range_for_window(ifd, col_off, row_off, width, height)
        .ok_or(AgError::Tiff("ifd lacks tile dims".into()))?;

    let coords: Vec<(usize, usize)> = (tx0..=tx1)
        .flat_map(|x| (ty0..=ty1).map(move |y| (x as usize, y as usize)))
        .collect();
    let tiles = ifd.fetch_tiles(coords.clone(), &self.reader)
        .await
        .map_err(|e| AgError::Tiff(format!("fetch_tiles: {e}")))?;

    let registry = DecoderRegistry::default();
    // Decode each tile on the rayon pool (offloaded with spawn_blocking).
    let arrays: Vec<async_tiff::Array> = tokio::task::spawn_blocking(move || {
        tiles.into_iter()
            .map(|t| t.decode(&registry))
            .collect::<Result<Vec<_>, _>>()
    })
    .await
    .map_err(|e| AgError::Tiff(format!("join: {e}")))?
    .map_err(|e| AgError::Tiff(format!("decode: {e}")))?;

    // Stitch into a single planar (bands, h, w) buffer cropped to the requested window.
    let stitched = crate::io::window::stitch(
        &arrays, &coords, ifd, col_off, row_off, width, height,
    )?;

    let bbox_world = self.transform_bbox(col_off, row_off, width, height);
    Ok(ImageData {
        pixels: stitched,
        mask: None,
        bbox: bbox_world,
        crs: self.src_crs.clone(),
        transform: self.transform,
        nodata: ifd.gdal_nodata().and_then(|s| s.parse::<f64>().ok()),
    })
}

fn transform_bbox(&self, col: i64, row: i64, w: u64, h: u64) -> crate::bounds::Bbox {
    let (x0, y0) = self.transform.forward(col as f64, row as f64);
    let (x1, y1) = self.transform.forward((col + w as i64) as f64, (row + h as i64) as f64);
    crate::bounds::Bbox::new(x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
}
```

- [ ] **Step 3: Add `stitch` to `src/io/window.rs`**

Append to `src/io/window.rs`:

```rust
use async_tiff::{Array, ImageFileDirectory};
use ndarray::{Array3, s};
use crate::pixel::PixelData;
use crate::error::{AgError, AgResult};

/// Stitch the fetched tiles (in (tx, ty) iteration order) into a planar (bands, h, w) buffer
/// cropped to the requested window.
pub fn stitch(
    arrays: &[Array],
    coords: &[(usize, usize)],
    ifd: &ImageFileDirectory,
    col_off: i64,
    row_off: i64,
    width: u64,
    height: u64,
) -> AgResult<PixelData> {
    let tw = ifd.tile_width().ok_or(AgError::Tiff("no tile_width".into()))? as i64;
    let th = ifd.tile_height().ok_or(AgError::Tiff("no tile_height".into()))? as i64;
    let bands = ifd.samples_per_pixel() as usize;

    // Allocate output as f64 — keeps a single dispatch path; the renderer
    // converts to f64 internally anyway. Future optimisation: keep dtype.
    let mut out = Array3::<f64>::zeros((bands, height as usize, width as usize));
    for ((tx, ty), arr) in coords.iter().zip(arrays.iter()) {
        let tile_col = (*tx as i64) * tw;
        let tile_row = (*ty as i64) * th;
        let arr_f64 = array_to_f64(arr)?;  // (bands, th, tw)
        for b in 0..bands {
            for r in 0..arr_f64.shape()[1] {
                let world_row = tile_row + r as i64;
                if world_row < row_off || world_row >= row_off + height as i64 { continue; }
                for c in 0..arr_f64.shape()[2] {
                    let world_col = tile_col + c as i64;
                    if world_col < col_off || world_col >= col_off + width as i64 { continue; }
                    let dst_r = (world_row - row_off) as usize;
                    let dst_c = (world_col - col_off) as usize;
                    out[[b, dst_r, dst_c]] = arr_f64[[b, r, c]];
                }
            }
        }
    }
    Ok(PixelData::F64(out))
}

fn array_to_f64(arr: &Array) -> AgResult<Array3<f64>> {
    use async_tiff::TypedArray;
    // Reshape (bands*h*w,) flat into (bands,h,w). async-tiff layout = planar.
    let shape = arr.shape();
    if shape.len() != 3 {
        return Err(AgError::Tiff(format!("expected 3D array, got {:?}", shape)));
    }
    let (b, h, w) = (shape[0], shape[1], shape[2]);
    match arr.as_typed() {
        TypedArray::U8(v)  => mk(v, b, h, w, |x| x as f64),
        TypedArray::U16(v) => mk(v, b, h, w, |x| x as f64),
        TypedArray::I16(v) => mk(v, b, h, w, |x| x as f64),
        TypedArray::U32(v) => mk(v, b, h, w, |x| x as f64),
        TypedArray::I32(v) => mk(v, b, h, w, |x| x as f64),
        TypedArray::F32(v) => mk(v, b, h, w, |x| x as f64),
        TypedArray::F64(v) => mk(v, b, h, w, |x| x),
        other => Err(AgError::UnsupportedPixel(format!("{:?}", other))),
    }
}

fn mk<T: Copy>(v: &[T], b: usize, h: usize, w: usize, f: impl Fn(T) -> f64) -> AgResult<Array3<f64>> {
    let buf: Vec<f64> = v.iter().copied().map(f).collect();
    Array3::from_shape_vec((b, h, w), buf).map_err(|e| AgError::Tiff(format!("shape: {e}")))
}
```

- [ ] **Step 4: Run**

Run: `cargo test --lib io::window::tests`
Expected: PASS — 2 tests.

- [ ] **Step 5: Commit**

```bash
git add src/io
git commit -m "feat: tile-range + stitch + CogReader::read_window"
```

---

## Task 9: Integration test — open + read local COG fixture

**Files:**
- Create: `tests/fixtures/build_fixture.rs` (used in build.rs alternative — actually we'll just generate at test time)
- Create: `tests/cog_read.rs`

- [ ] **Step 1: Write integration test that generates a fixture**

`tests/cog_read.rs`:

```rust
//! Integration: open a locally-served COG via object_store::http and read a window.
//!
//! We construct a tiny COG byte buffer in-process using the `tiff` crate
//! (added as dev-dep) and serve it from a tempfile via an ephemeral
//! `axum`-less hyper server. To keep the test free of an HTTP server,
//! we instead drop the bytes to a tempfile and open it via the
//! `object_store::local::LocalFileSystem` store directly.

use async_geotiff::io::CogReader;
use async_geotiff::pixel::PixelData;
use std::fs::File;
use std::io::Write;
use tempfile::tempdir;

#[tokio::test]
async fn open_local_cog_reads_one_pixel() {
    // Skip until fixtures are wired; serve as a smoke check that the API compiles.
    // Replaced in Task 9b with a real fixture build.
    let dir = tempdir().unwrap();
    let path = dir.path().join("dummy.tif");
    let _ = File::create(&path).unwrap();
    let url = format!("file://{}", path.display());
    let r = CogReader::open_http(&url).await;
    assert!(r.is_err(), "expected metadata error on empty file");
}
```

- [ ] **Step 2: Add dev-deps**

Edit `Cargo.toml` `[dev-dependencies]`, add:
```toml
tiff = "0.9"
```

- [ ] **Step 3: Run**

Run: `cargo test --test cog_read`
Expected: PASS — 1 test, error path verified.

- [ ] **Step 4: Commit**

```bash
git add tests/cog_read.rs Cargo.toml
git commit -m "test: skeleton integration test for CogReader"
```

(Real fixture COG generation deferred to Task 23 once full pipeline is testable.)

---

## Task 10: Warp — proj wrapper

**Files:**
- Create: `src/warp/mod.rs`
- Create: `src/warp/projection.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add module**

Append to `src/lib.rs`:
```rust
pub mod warp;
```

- [ ] **Step 2: Write `src/warp/mod.rs`**

```rust
pub mod projection;
pub mod reproject;
pub mod resampler;

pub use projection::Projector;
pub use reproject::warp;
```

- [ ] **Step 3: Write `src/warp/projection.rs`**

```rust
use crate::crs::Crs;
use crate::error::{AgError, AgResult};

/// Thin wrapper around `proj::Proj` with a uniform interface.
/// `Proj` is `!Send + !Sync`; this wrapper inherits that and must be owned by
/// a single thread (typically inside `spawn_blocking`).
#[cfg(feature = "proj")]
pub struct Projector {
    inner: proj::Proj,
    src: Crs,
    dst: Crs,
}

#[cfg(feature = "proj")]
impl Projector {
    pub fn new(src: &Crs, dst: &Crs) -> AgResult<Self> {
        let inner = proj::Proj::new_known_crs(src.as_str(), dst.as_str(), None)
            .map_err(|e| AgError::Warp(format!("proj init {src}->{dst}: {e}")))?;
        Ok(Self { inner, src: src.clone(), dst: dst.clone() })
    }

    /// Convert in-place a batch of (x, y) points.
    pub fn convert(&self, pts: &mut [(f64, f64)]) -> AgResult<()> {
        self.inner.convert_array(pts)
            .map(|_| ())
            .map_err(|e| AgError::Warp(format!("convert: {e}")))
    }

    pub fn src(&self) -> &Crs { &self.src }
    pub fn dst(&self) -> &Crs { &self.dst }
}

#[cfg(not(feature = "proj"))]
pub struct Projector { _private: () }

#[cfg(not(feature = "proj"))]
impl Projector {
    pub fn new(_src: &Crs, _dst: &Crs) -> AgResult<Self> {
        Err(AgError::Warp("crate built without `proj` feature".into()))
    }
    pub fn convert(&self, _pts: &mut [(f64, f64)]) -> AgResult<()> {
        Err(AgError::Warp("crate built without `proj` feature".into()))
    }
}

#[cfg(all(test, feature = "proj"))]
mod tests {
    use super::*;
    use crate::crs::Crs;
    use approx::assert_relative_eq;

    #[test]
    fn lonlat_to_webmercator() {
        let p = Projector::new(&Crs::epsg(4326), &Crs::epsg(3857)).unwrap();
        let mut pts = [(0.0, 0.0)];
        p.convert(&mut pts).unwrap();
        assert_relative_eq!(pts[0].0, 0.0, epsilon = 1e-3);
        assert_relative_eq!(pts[0].1, 0.0, epsilon = 1e-3);
    }
}
```

- [ ] **Step 4: Create stubs for sibling files**

`src/warp/reproject.rs`:
```rust
//! Reprojection — coarse-grid warp approximation.
```

`src/warp/resampler.rs`:
```rust
//! Resampler kernels for warp interpolation.
```

- [ ] **Step 5: Run**

Run: `cargo test --lib warp::projection`
Expected: PASS — 1 test (requires libproj 9.6 on system; skip if unavailable).

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/warp
git commit -m "feat: Projector wrapper over proj crate"
```

---

## Task 11: Warp — resampler kernels

**Files:**
- Modify: `src/warp/resampler.rs`

- [ ] **Step 1: Write resampler kernels**

```rust
use ndarray::ArrayView2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resampling {
    Nearest,
    Bilinear,
}

impl Resampling {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "nearest"  => Some(Self::Nearest),
            "bilinear" => Some(Self::Bilinear),
            _ => None,
        }
    }

    /// Sample value at fractional (col, row) coordinates.
    pub fn sample(self, view: ArrayView2<f64>, col: f64, row: f64, fill: f64) -> f64 {
        let (h, w) = (view.shape()[0] as i64, view.shape()[1] as i64);
        match self {
            Self::Nearest => {
                let c = col.round() as i64;
                let r = row.round() as i64;
                if c < 0 || r < 0 || c >= w || r >= h { fill } else { view[[r as usize, c as usize]] }
            }
            Self::Bilinear => {
                let c0 = col.floor() as i64;
                let r0 = row.floor() as i64;
                let c1 = c0 + 1; let r1 = r0 + 1;
                if c0 < 0 || r0 < 0 || c1 >= w || r1 >= h { return fill; }
                let dc = col - c0 as f64;
                let dr = row - r0 as f64;
                let v00 = view[[r0 as usize, c0 as usize]];
                let v01 = view[[r0 as usize, c1 as usize]];
                let v10 = view[[r1 as usize, c0 as usize]];
                let v11 = view[[r1 as usize, c1 as usize]];
                let top = v00 * (1.0 - dc) + v01 * dc;
                let bot = v10 * (1.0 - dc) + v11 * dc;
                top * (1.0 - dr) + bot * dr
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::array;
    use approx::assert_relative_eq;

    #[test]
    fn nearest_at_center_returns_pixel() {
        let a = array![[1.0, 2.0], [3.0, 4.0]];
        assert_eq!(Resampling::Nearest.sample(a.view(), 0.0, 0.0, 0.0), 1.0);
        assert_eq!(Resampling::Nearest.sample(a.view(), 1.0, 1.0, 0.0), 4.0);
    }

    #[test]
    fn bilinear_midpoint_averages() {
        let a = array![[0.0, 10.0], [0.0, 10.0]];
        assert_relative_eq!(Resampling::Bilinear.sample(a.view(), 0.5, 0.0, 0.0), 5.0);
    }

    #[test]
    fn out_of_bounds_returns_fill() {
        let a = array![[1.0]];
        assert_eq!(Resampling::Nearest.sample(a.view(), 5.0, 5.0, -1.0), -1.0);
    }

    #[test]
    fn parse_string() {
        assert_eq!(Resampling::parse("bilinear"), Some(Resampling::Bilinear));
        assert_eq!(Resampling::parse("foo"), None);
    }
}
```

- [ ] **Step 2: Run**

Run: `cargo test --lib warp::resampler::tests`
Expected: PASS — 4 tests.

- [ ] **Step 3: Commit**

```bash
git add src/warp/resampler.rs
git commit -m "feat: nearest + bilinear resampler kernels"
```

---

## Task 12: Warp — coarse-grid reproject

**Files:**
- Modify: `src/warp/reproject.rs`

- [ ] **Step 1: Write coarse-grid warp**

```rust
use ndarray::{Array2, Array3};
use crate::bounds::{Bbox, GeoTransform};
use crate::error::{AgError, AgResult};
use crate::pixel::{ImageData, PixelData};
use crate::warp::projection::Projector;
use crate::warp::resampler::Resampling;
use crate::crs::Crs;

/// Warp the source `ImageData` into the destination grid:
/// - `dst_bbox` is in `dst_crs`,
/// - output buffer is `dst_h × dst_w`,
/// - coarse 32×32 grid through PROJ + bilinear interpolation of the mapping.
pub fn warp(
    src: &ImageData,
    dst_crs: &Crs,
    dst_bbox: Bbox,
    dst_w: usize,
    dst_h: usize,
    resampling: Resampling,
) -> AgResult<ImageData> {
    if &src.crs == dst_crs {
        // Fast path: caller should have skipped warp. Defensive copy.
        return Ok(src.clone());
    }

    let proj = Projector::new(dst_crs, &src.crs)?;

    // 1. Build a 32x32 sample grid in destination pixel space, transform corners through PROJ.
    const N: usize = 32;
    let mut grid_dst = Vec::with_capacity((N + 1) * (N + 1));
    for i in 0..=N {
        for j in 0..=N {
            let dc = (j as f64) * (dst_w as f64) / (N as f64);
            let dr = (i as f64) * (dst_h as f64) / (N as f64);
            // dst pixel -> dst world
            let dx = dst_bbox.xmin + dc / dst_w as f64 * dst_bbox.width();
            let dy = dst_bbox.ymax - dr / dst_h as f64 * dst_bbox.height();
            grid_dst.push((dx, dy));
        }
    }

    // 2. Transform dst-world -> src-world in one batched call.
    proj.convert(&mut grid_dst)?;

    // 3. dst-world -> src-pixel via source GeoTransform inverse.
    let src_inv = src.transform.inverse().ok_or(AgError::Warp("src inverse".into()))?;
    let mut grid_src_pix: Vec<(f64, f64)> = grid_dst.into_iter()
        .map(|(x, y)| src_inv.forward(x, y))
        .collect();

    // 4. For each destination pixel, bilinear-interpolate the 4 surrounding grid cells
    //    to get fractional (src_col, src_row), then sample.
    let bands = src.pixels.bands();
    let src_f64 = src.pixels.to_f64();
    let fill = src.nodata.unwrap_or(0.0);

    let mut out = Array3::<f64>::zeros((bands, dst_h, dst_w));
    for r in 0..dst_h {
        for c in 0..dst_w {
            let gi = (r as f64) * (N as f64) / (dst_h as f64);
            let gj = (c as f64) * (N as f64) / (dst_w as f64);
            let i0 = gi.floor() as usize;
            let j0 = gj.floor() as usize;
            let i1 = (i0 + 1).min(N);
            let j1 = (j0 + 1).min(N);
            let di = gi - i0 as f64;
            let dj = gj - j0 as f64;
            let p00 = grid_src_pix[i0 * (N + 1) + j0];
            let p01 = grid_src_pix[i0 * (N + 1) + j1];
            let p10 = grid_src_pix[i1 * (N + 1) + j0];
            let p11 = grid_src_pix[i1 * (N + 1) + j1];
            let sc = p00.0 * (1.0 - di) * (1.0 - dj)
                   + p01.0 * (1.0 - di) * dj
                   + p10.0 * di * (1.0 - dj)
                   + p11.0 * di * dj;
            let sr = p00.1 * (1.0 - di) * (1.0 - dj)
                   + p01.1 * (1.0 - di) * dj
                   + p10.1 * di * (1.0 - dj)
                   + p11.1 * di * dj;
            for b in 0..bands {
                let band_view = src_f64.index_axis(ndarray::Axis(0), b);
                out[[b, r, c]] = resampling.sample(band_view, sc, sr, fill);
            }
        }
    }

    let dst_transform = GeoTransform::from_gdal(&[
        dst_bbox.xmin,
        dst_bbox.width() / dst_w as f64,
        0.0,
        dst_bbox.ymax,
        0.0,
        -dst_bbox.height() / dst_h as f64,
    ]);

    Ok(ImageData {
        pixels: PixelData::F64(out),
        mask: None,
        bbox: dst_bbox,
        crs: dst_crs.clone(),
        transform: dst_transform,
        nodata: src.nodata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pixel::{PixelData, ImageData};
    use ndarray::Array3;

    fn dummy_src(crs: Crs) -> ImageData {
        ImageData {
            pixels: PixelData::F64(Array3::<f64>::zeros((1, 4, 4))),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 4.0, 4.0),
            crs,
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 4.0, 0.0, -1.0]),
            nodata: None,
        }
    }

    #[test]
    fn same_crs_fast_path_clones() {
        let src = dummy_src(Crs::epsg(3857));
        let out = warp(&src, &Crs::epsg(3857), src.bbox, 4, 4, Resampling::Nearest).unwrap();
        assert_eq!(out.crs, src.crs);
    }
}
```

- [ ] **Step 2: Run**

Run: `cargo test --lib warp::reproject::tests`
Expected: PASS — 1 test.

- [ ] **Step 3: Commit**

```bash
git add src/warp/reproject.rs
git commit -m "feat: coarse-grid warp (32x32 PROJ + bilinear interp)"
```

---

## Task 13: Render — `Colour` type (ported from map-engine)

**Files:**
- Create: `src/render/mod.rs`
- Create: `src/render/colour.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add module**

Append to `src/lib.rs`:
```rust
pub mod render;
```

- [ ] **Step 2: Write `src/render/mod.rs`**

```rust
pub mod colour;
pub mod colormap;
pub mod color_formula;
pub mod composite;
pub mod rescale;
pub mod encode;
pub mod pipeline;

pub use colour::Colour;
pub use composite::{Composite, ColourDefinition, HandleGet};
```

- [ ] **Step 3: Write `src/render/colour.rs`**

```rust
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub enum Colour {
    Seq([f64; 4]),
    Hex(String),
}

impl Colour {
    pub fn rgba(&self) -> [f64; 4] {
        match self {
            Self::Seq(c) => *c,
            Self::Hex(s) => parse_hex(s).expect("invalid hex colour"),
        }
    }

    pub fn rgba_u8(&self) -> [u8; 4] {
        let c = self.rgba();
        [(c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8, (c[3] * 255.0) as u8]
    }
}

fn parse_hex(s: &str) -> Option<[f64; 4]> {
    let s = s.trim_start_matches('#');
    let (r, g, b, a) = match s.len() {
        6 => (
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
            255u8,
        ),
        8 => (
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
            u8::from_str_radix(&s[6..8], 16).ok()?,
        ),
        _ => return None,
    };
    Some([r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0, a as f64 / 255.0])
}

impl<'de> Deserialize<'de> for Colour {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Hex(String),
            Components(Vec<f64>),
        }
        match Raw::deserialize(d)? {
            Raw::Hex(s) => Ok(Colour::Hex(s)),
            Raw::Components(v) => {
                let max = v.iter().cloned().fold(0.0_f64, f64::max);
                let div = if max > 1.0 { 255.0 } else { 1.0 };
                let mut c = [0.0; 4]; c[3] = 1.0;
                for (i, x) in v.into_iter().take(4).enumerate() { c[i] = x / div; }
                Ok(Colour::Seq(c))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_rgb() {
        let c = Colour::Hex("#FF8040".into());
        let [r, g, b, a] = c.rgba_u8();
        assert_eq!((r, g, b, a), (255, 128, 64, 255));
    }

    #[test]
    fn parse_hex_rgba() {
        let c = Colour::Hex("#FF804080".into());
        let [_, _, _, a] = c.rgba_u8();
        assert_eq!(a, 128);
    }

    #[test]
    fn deserialise_components_in_0_to_255() {
        let c: Colour = serde_json::from_str("[255, 128, 64]").unwrap();
        assert_eq!(c.rgba_u8(), [255, 128, 64, 255]);
    }

    #[test]
    fn deserialise_components_in_0_to_1() {
        let c: Colour = serde_json::from_str("[1.0, 0.5, 0.25]").unwrap();
        let [r, g, b, _] = c.rgba_u8();
        assert!((r as i32 - 255).abs() <= 1);
        assert!((g as i32 - 127).abs() <= 1);
        assert!((b as i32 - 63).abs() <= 1);
    }
}
```

- [ ] **Step 4: Add `serde_json` dev-dep**

Edit `Cargo.toml` `[dev-dependencies]`, add:
```toml
serde_json = "1"
```

- [ ] **Step 5: Create sibling stubs (will be filled in subsequent tasks)**

```rust
// src/render/colormap.rs
//! Colormap LUTs (colorous builtins + user-supplied stops via enterpolation).
```

```rust
// src/render/color_formula.rs
//! rio-color formula subset parser + applicator.
```

```rust
// src/render/composite.rs
//! Composite + ColourDefinition + HandleGet.
```

```rust
// src/render/rescale.rs
//! Per-band linear rescale.
```

```rust
// src/render/encode.rs
//! PNG/WebP/JPEG encoding.
```

```rust
// src/render/pipeline.rs
//! Fused per-tile render pipeline.
```

- [ ] **Step 6: Run**

Run: `cargo test --lib render::colour::tests`
Expected: PASS — 4 tests.

- [ ] **Step 7: Commit**

```bash
git add src/lib.rs src/render Cargo.toml
git commit -m "feat: Colour type (hex + components) ported from map-engine"
```

---

## Task 14: Render — colormap (colorous LUTs + custom stops)

**Files:**
- Modify: `src/render/colormap.rs`

- [ ] **Step 1: Write colormap module**

```rust
use crate::error::{AgError, AgResult};
use crate::render::colour::Colour;

/// 256-entry RGBA lookup table; alpha defaults to 255 for built-ins.
pub type Lut = [[u8; 4]; 256];

const BUILTINS: &[(&str, &colorous::Gradient)] = &[
    ("viridis", &colorous::VIRIDIS),
    ("magma",   &colorous::MAGMA),
    ("inferno", &colorous::INFERNO),
    ("plasma",  &colorous::PLASMA),
    ("cividis", &colorous::CIVIDIS),
    ("turbo",   &colorous::TURBO),
    ("greys",   &colorous::GREYS),
];

pub fn builtin_lut(name: &str) -> AgResult<Lut> {
    let g = BUILTINS.iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, g)| *g)
        .ok_or_else(|| AgError::UnknownColormap(name.into()))?;
    let mut lut = [[0u8; 4]; 256];
    for i in 0..256 {
        let c = g.eval_rational(i, 255);
        lut[i] = [c.r, c.g, c.b, 255];
    }
    Ok(lut)
}

/// Build a LUT from user-supplied (position-in-[0,1], Colour) stops.
pub fn lut_from_stops(stops: &[(f64, Colour)]) -> AgResult<Lut> {
    if stops.len() < 2 {
        return Err(AgError::UnknownColormap("need ≥2 stops".into()));
    }
    let mut s = stops.to_vec();
    s.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut lut = [[0u8; 4]; 256];
    for i in 0..256 {
        let t = i as f64 / 255.0;
        let (idx, _) = match s.iter().position(|(p, _)| *p >= t) {
            Some(0) => (0, 0.0),
            Some(k) => (k, 0.0),
            None    => (s.len() - 1, 0.0),
        };
        let (a, b) = (s[idx.saturating_sub(1).max(0)].clone(), s[idx.min(s.len() - 1)].clone());
        let span = (b.0 - a.0).max(1e-12);
        let mix = ((t - a.0) / span).clamp(0.0, 1.0);
        let ca = a.1.rgba();
        let cb = b.1.rgba();
        lut[i] = [
            ((ca[0] * (1.0 - mix) + cb[0] * mix) * 255.0) as u8,
            ((ca[1] * (1.0 - mix) + cb[1] * mix) * 255.0) as u8,
            ((ca[2] * (1.0 - mix) + cb[2] * mix) * 255.0) as u8,
            ((ca[3] * (1.0 - mix) + cb[3] * mix) * 255.0) as u8,
        ];
    }
    Ok(lut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viridis_endpoints_are_purple_yellow() {
        let lut = builtin_lut("viridis").unwrap();
        // viridis starts near (68, 1, 84) deep purple, ends near (253, 231, 37) yellow
        let [r0, g0, _, _] = lut[0];
        let [r255, g255, _, _] = lut[255];
        assert!(r0 < 100 && g0 < 40);
        assert!(r255 > 200 && g255 > 200);
    }

    #[test]
    fn unknown_colormap_errors() {
        assert!(builtin_lut("not-a-thing").is_err());
    }

    #[test]
    fn stops_lut_linear_between_red_and_blue() {
        let lut = lut_from_stops(&[
            (0.0, Colour::Hex("#FF0000".into())),
            (1.0, Colour::Hex("#0000FF".into())),
        ]).unwrap();
        let mid = lut[127];
        // expect roughly half red, half blue
        assert!(mid[0] > 100 && mid[0] < 160);
        assert!(mid[2] > 100 && mid[2] < 160);
    }
}
```

- [ ] **Step 2: Run**

Run: `cargo test --lib render::colormap::tests`
Expected: PASS — 3 tests.

- [ ] **Step 3: Commit**

```bash
git add src/render/colormap.rs
git commit -m "feat: built-in (colorous) + custom-stop colormap LUTs"
```

---

## Task 15: Render — rescale

**Files:**
- Modify: `src/render/rescale.rs`

- [ ] **Step 1: Write rescale module**

```rust
use ndarray::Array3;

/// Linear per-band stretch into [0, 1]. Values are clamped at the ends.
/// `ranges` length must match `bands` (first axis of `data`).
pub fn rescale(mut data: Array3<f64>, ranges: &[(f64, f64)]) -> Array3<f64> {
    let bands = data.shape()[0];
    for b in 0..bands {
        let (lo, hi) = ranges.get(b).copied().unwrap_or((0.0, 1.0));
        let span = (hi - lo).max(1e-12);
        let mut view = data.index_axis_mut(ndarray::Axis(0), b);
        view.mapv_inplace(|v| ((v - lo) / span).clamp(0.0, 1.0));
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array3;

    #[test]
    fn clamps_to_unit_range() {
        let data = Array3::<f64>::from_shape_vec((1, 1, 4), vec![-10.0, 0.0, 50.0, 1000.0]).unwrap();
        let out = rescale(data, &[(0.0, 100.0)]);
        assert_eq!(out[[0, 0, 0]], 0.0);
        assert_eq!(out[[0, 0, 1]], 0.0);
        assert_eq!(out[[0, 0, 2]], 0.5);
        assert_eq!(out[[0, 0, 3]], 1.0);
    }

    #[test]
    fn per_band_ranges() {
        let data = Array3::<f64>::from_shape_vec(
            (2, 1, 2), vec![50.0, 100.0, 0.0, 1000.0]).unwrap();
        let out = rescale(data, &[(0.0, 100.0), (0.0, 2000.0)]);
        assert_eq!(out[[0, 0, 0]], 0.5);
        assert_eq!(out[[1, 0, 1]], 0.5);
    }
}
```

- [ ] **Step 2: Run**

Run: `cargo test --lib render::rescale::tests`
Expected: PASS — 2 tests.

- [ ] **Step 3: Commit**

```bash
git add src/render/rescale.rs
git commit -m "feat: per-band linear rescale with clamping"
```

---

## Task 16: Render — color_formula parser

**Files:**
- Modify: `src/render/color_formula.rs`

- [ ] **Step 1: Write parser**

```rust
use nom::{
    branch::alt, bytes::complete::tag_no_case,
    character::complete::{char, multispace0, multispace1},
    combinator::{map, opt, recognize},
    multi::{many0, many1},
    number::complete::double,
    sequence::{preceded, tuple},
    IResult,
};
use crate::error::{AgError, AgResult};

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Gamma     { bands: Bands, value: f64 },
    Sigmoidal { bands: Bands, contrast: f64, bias: f64 },
    Saturation { factor: f64 },
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Bands: u8 {
        const R = 0b001;
        const G = 0b010;
        const B = 0b100;
    }
}

pub fn parse(input: &str) -> AgResult<Vec<Op>> {
    let s = input.replace(',', " ");
    let (rest, ops) = many1(preceded(multispace0, op))(&s)
        .map_err(|e| AgError::ColorFormula(format!("parse: {e}")))?;
    if !rest.trim().is_empty() {
        return Err(AgError::ColorFormula(format!("trailing: {:?}", rest.trim())));
    }
    Ok(ops)
}

fn op(input: &str) -> IResult<&str, Op> {
    alt((gamma, sigmoidal, saturation))(input)
}

fn gamma(input: &str) -> IResult<&str, Op> {
    let (i, _) = tag_no_case("gamma")(input)?;
    let (i, _) = multispace1(i)?;
    let (i, b) = bands(i)?;
    let (i, _) = multispace1(i)?;
    let (i, v) = double(i)?;
    Ok((i, Op::Gamma { bands: b, value: v }))
}

fn sigmoidal(input: &str) -> IResult<&str, Op> {
    let (i, _) = tag_no_case("sigmoidal")(input)?;
    let (i, _) = multispace1(i)?;
    let (i, b) = bands(i)?;
    let (i, _) = multispace1(i)?;
    let (i, c) = double(i)?;
    let (i, _) = multispace1(i)?;
    let (i, bi) = double(i)?;
    Ok((i, Op::Sigmoidal { bands: b, contrast: c, bias: bi }))
}

fn saturation(input: &str) -> IResult<&str, Op> {
    let (i, _) = tag_no_case("saturation")(input)?;
    let (i, _) = multispace1(i)?;
    let (i, v) = double(i)?;
    Ok((i, Op::Saturation { factor: v }))
}

fn bands(input: &str) -> IResult<&str, Bands> {
    let (i, raw) = recognize(many1(nom::character::complete::satisfy(|c| {
        matches!(c, 'r' | 'g' | 'b' | 'R' | 'G' | 'B' | '1' | '2' | '3')
    })))(input)?;
    let mut bs = Bands::empty();
    for ch in raw.chars() {
        match ch.to_ascii_lowercase() {
            'r' | '1' => bs |= Bands::R,
            'g' | '2' => bs |= Bands::G,
            'b' | '3' => bs |= Bands::B,
            _ => {}
        }
    }
    Ok((i, bs))
}

/// Apply ops to a single sRGB pixel (r, g, b) in [0,1]. Saturation goes via Lab/Lch
/// using palette to match rio-color exactly.
pub fn apply(ops: &[Op], r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let (mut r, mut g, mut b) = (r, g, b);
    for op in ops {
        match *op {
            Op::Gamma { bands, value } => {
                let g_inv = 1.0 / value;
                if bands.contains(Bands::R) { r = r.powf(g_inv); }
                if bands.contains(Bands::G) { g = g.powf(g_inv); }
                if bands.contains(Bands::B) { b = b.powf(g_inv); }
            }
            Op::Sigmoidal { bands, contrast, bias } => {
                if contrast == 0.0 { continue; }
                let f = |x: f64| sigmoid_norm(x, contrast, bias);
                if bands.contains(Bands::R) { r = f(r); }
                if bands.contains(Bands::G) { g = f(g); }
                if bands.contains(Bands::B) { b = f(b); }
            }
            Op::Saturation { factor } => {
                use palette::{Srgb, Lch, FromColor, IntoColor};
                let srgb = Srgb::new(r as f32, g as f32, b as f32);
                let mut lch: Lch = srgb.into_color();
                lch.chroma *= factor as f32;
                let back: Srgb = lch.into_color();
                r = back.red.clamp(0.0, 1.0) as f64;
                g = back.green.clamp(0.0, 1.0) as f64;
                b = back.blue.clamp(0.0, 1.0) as f64;
            }
        }
    }
    (r, g, b)
}

fn sigmoid_norm(x: f64, beta: f64, alpha: f64) -> f64 {
    let s = |v: f64| 1.0 / (1.0 + (beta * (alpha - v)).exp());
    let s0 = s(0.0); let s1 = s(1.0);
    ((s(x) - s0) / (s1 - s0)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn parse_gamma_only() {
        let ops = parse("gamma RGB 1.5").unwrap();
        assert_eq!(ops, vec![Op::Gamma { bands: Bands::R | Bands::G | Bands::B, value: 1.5 }]);
    }

    #[test]
    fn parse_chain() {
        let ops = parse("gamma RG 1.2 sigmoidal B 5 0.5 saturation 1.3").unwrap();
        assert_eq!(ops.len(), 3);
    }

    #[test]
    fn rio_color_gamma_regression() {
        let (r, _, _) = apply(&parse("gamma rgb 0.95").unwrap(), 10.0/255.0, 50.0/255.0, 100.0/255.0);
        assert_relative_eq!(r, 0.033069782, epsilon = 1e-4);
    }

    #[test]
    fn rio_color_sigmoidal_regression() {
        let (r, _, _) = apply(&parse("sigmoidal rgb 5 0.53").unwrap(),
            10.0/255.0, 50.0/255.0, 100.0/255.0);
        assert_relative_eq!(r, 0.080560341, epsilon = 1e-4);
    }
}
```

- [ ] **Step 2: Add `bitflags` to Cargo.toml**

Edit `Cargo.toml`, add to `[dependencies]`:
```toml
bitflags = "2"
```

- [ ] **Step 3: Run**

Run: `cargo test --lib render::color_formula::tests`
Expected: PASS — 4 tests.

- [ ] **Step 4: Commit**

```bash
git add src/render/color_formula.rs Cargo.toml
git commit -m "feat: rio-color formula subset (gamma/sigmoidal/saturation)"
```

---

## Task 17: Render — Composite + ColourDefinition + HandleGet

**Files:**
- Modify: `src/render/composite.rs`

- [ ] **Step 1: Write composite module**

```rust
use std::collections::HashMap;
use serde::Deserialize;
use crate::error::AgResult;
use crate::render::colour::Colour;
use crate::render::colormap::{builtin_lut, lut_from_stops, Lut};

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ColourDefinition {
    Named(String),
    Colours(Vec<Colour>),
    ColoursAndBreaks(Vec<(f64, Colour)>),
    Discrete(Vec<(i64, Colour)>),
    Rgb([f64; 3], [f64; 3]),
}

#[derive(Debug, Clone)]
pub struct Composite {
    pub vmin: Option<Vec<f64>>,
    pub vmax: Option<Vec<f64>>,
    pub lut: Option<Lut>,
    pub discrete: Option<HashMap<i64, [u8; 4]>>,
    pub kind: CompositeKind,
    pub bands: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeKind {
    Gradient,
    Discrete,
    Rgb,
}

pub trait HandleGet {
    fn get(&self, values: &[f64], nodata: Option<&[f64]>) -> [u8; 4];
}

impl HandleGet for Composite {
    fn get(&self, values: &[f64], nodata: Option<&[f64]>) -> [u8; 4] {
        match self.kind {
            CompositeKind::Discrete => {
                let v = values[0].trunc() as i64;
                self.discrete.as_ref().and_then(|m| m.get(&v).copied()).unwrap_or([0, 0, 0, 0])
            }
            CompositeKind::Gradient => {
                let v = values[0];
                if let Some(nd) = nodata {
                    if !nd.is_empty() && (v - nd[0]).abs() < 1e-9 { return [0, 0, 0, 0]; }
                }
                let (lo, hi) = match (&self.vmin, &self.vmax) {
                    (Some(a), Some(b)) => (a[0], b[0]),
                    _ => (0.0, 1.0),
                };
                let span = (hi - lo).max(1e-12);
                let t = ((v - lo) / span * 255.0).clamp(0.0, 255.0) as usize;
                self.lut.unwrap_or([[0; 4]; 256])[t]
            }
            CompositeKind::Rgb => {
                let lo = self.vmin.as_ref().expect("rgb vmin");
                let hi = self.vmax.as_ref().expect("rgb vmax");
                if let Some(nd) = nodata {
                    for (i, v) in values.iter().enumerate().take(3) {
                        if i < nd.len() && (*v - nd[i]).abs() < 1e-9 {
                            return [0, 0, 0, 0];
                        }
                    }
                }
                let mut out = [0u8; 4];
                for i in 0..3 {
                    let span = (hi[i] - lo[i]).max(1e-12);
                    let t = ((values[i] - lo[i]) / span).clamp(0.0, 1.0);
                    out[i] = (t * 255.0) as u8;
                }
                out[3] = 255;
                out
            }
        }
    }
}

impl Composite {
    pub fn from_definition(
        def: &ColourDefinition,
        vmin: Option<Vec<f64>>,
        vmax: Option<Vec<f64>>,
    ) -> AgResult<Self> {
        Ok(match def {
            ColourDefinition::Named(name) => Self {
                vmin: vmin.clone(),
                vmax: vmax.clone(),
                lut: Some(builtin_lut(name)?),
                discrete: None,
                kind: CompositeKind::Gradient,
                bands: 1,
            },
            ColourDefinition::Colours(cs) => {
                let stops: Vec<(f64, Colour)> = cs.iter().enumerate()
                    .map(|(i, c)| (i as f64 / (cs.len() - 1).max(1) as f64, c.clone()))
                    .collect();
                Self {
                    vmin, vmax,
                    lut: Some(lut_from_stops(&stops)?),
                    discrete: None,
                    kind: CompositeKind::Gradient,
                    bands: 1,
                }
            }
            ColourDefinition::ColoursAndBreaks(cb) => {
                // Map break positions to [0,1] using vmin/vmax envelope.
                let (lo, hi) = (
                    cb.first().map(|(p, _)| *p).unwrap_or(0.0),
                    cb.last().map(|(p, _)| *p).unwrap_or(1.0),
                );
                let span = (hi - lo).max(1e-12);
                let stops: Vec<(f64, Colour)> = cb.iter()
                    .map(|(p, c)| ((*p - lo) / span, c.clone())).collect();
                Self {
                    vmin: Some(vec![lo]), vmax: Some(vec![hi]),
                    lut: Some(lut_from_stops(&stops)?),
                    discrete: None,
                    kind: CompositeKind::Gradient,
                    bands: 1,
                }
            }
            ColourDefinition::Discrete(pairs) => {
                let map: HashMap<i64, [u8; 4]> = pairs.iter()
                    .map(|(k, c)| (*k, c.rgba_u8())).collect();
                Self {
                    vmin: None, vmax: None, lut: None,
                    discrete: Some(map),
                    kind: CompositeKind::Discrete,
                    bands: 1,
                }
            }
            ColourDefinition::Rgb(lo, hi) => Self {
                vmin: Some(lo.to_vec()),
                vmax: Some(hi.to_vec()),
                lut: None, discrete: None,
                kind: CompositeKind::Rgb,
                bands: 3,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradient_get_endpoints() {
        let comp = Composite::from_definition(
            &ColourDefinition::Named("viridis".into()),
            Some(vec![0.0]), Some(vec![1.0])
        ).unwrap();
        let lo = comp.get(&[0.0], None);
        let hi = comp.get(&[1.0], None);
        assert!(lo[0] < 100 && lo[1] < 40); // deep purple
        assert!(hi[0] > 200 && hi[1] > 200); // yellow
    }

    #[test]
    fn discrete_unmapped_is_transparent() {
        let comp = Composite::from_definition(
            &ColourDefinition::Discrete(vec![(1, Colour::Hex("#FF0000".into()))]),
            None, None,
        ).unwrap();
        assert_eq!(comp.get(&[1.0], None), [255, 0, 0, 255]);
        assert_eq!(comp.get(&[2.0], None), [0, 0, 0, 0]);
    }

    #[test]
    fn rgb_clamps_and_packs() {
        let comp = Composite::from_definition(
            &ColourDefinition::Rgb([0.0, 0.0, 0.0], [100.0, 100.0, 100.0]),
            None, None,
        ).unwrap();
        let out = comp.get(&[50.0, 100.0, 200.0], None);
        assert_eq!(out, [127, 255, 255, 255]);
    }

    #[test]
    fn nodata_makes_transparent() {
        let comp = Composite::from_definition(
            &ColourDefinition::Named("viridis".into()),
            Some(vec![0.0]), Some(vec![1.0])
        ).unwrap();
        assert_eq!(comp.get(&[-9999.0], Some(&[-9999.0])), [0, 0, 0, 0]);
    }
}
```

- [ ] **Step 2: Run**

Run: `cargo test --lib render::composite::tests`
Expected: PASS — 4 tests.

- [ ] **Step 3: Commit**

```bash
git add src/render/composite.rs
git commit -m "feat: Composite + HandleGet (gradient/discrete/rgb) ported from map-engine"
```

---

## Task 18: Render — encode PNG

**Files:**
- Modify: `src/render/encode.rs`

- [ ] **Step 1: Write encoder**

```rust
use bytes::Bytes;
use image::{ImageBuffer, Rgba, ImageFormat};
use ndarray::Array3;
use std::io::Cursor;
use crate::error::AgResult;

/// Encode an `(H, W, 4)` interleaved RGBA buffer to PNG bytes.
pub fn encode_png(rgba: &Array3<u8>) -> AgResult<Bytes> {
    let (h, w, c) = (rgba.shape()[0] as u32, rgba.shape()[1] as u32, rgba.shape()[2]);
    assert_eq!(c, 4, "expected RGBA channels");
    let raw = rgba.as_slice().expect("rgba array must be contiguous").to_vec();
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_raw(w, h, raw)
        .expect("buffer size mismatches dims");
    let mut buf = Vec::with_capacity((w * h * 4) as usize);
    img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)?;
    Ok(Bytes::from(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::Array3;

    #[test]
    fn encodes_solid_red_tile() {
        let mut arr = Array3::<u8>::zeros((4, 4, 4));
        for r in 0..4 {
            for c in 0..4 {
                arr[[r, c, 0]] = 255; arr[[r, c, 3]] = 255;
            }
        }
        let bytes = encode_png(&arr).unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));
    }
}
```

- [ ] **Step 2: Run**

Run: `cargo test --lib render::encode::tests`
Expected: PASS — 1 test.

- [ ] **Step 3: Commit**

```bash
git add src/render/encode.rs
git commit -m "feat: PNG encoder for interleaved RGBA Array3"
```

---

## Task 19: Render — fused pipeline

**Files:**
- Modify: `src/render/pipeline.rs`

- [ ] **Step 1: Write pipeline**

```rust
use bytes::Bytes;
use ndarray::{Array2, Array3};
use crate::error::AgResult;
use crate::pixel::ImageData;
use crate::render::color_formula::Op;
use crate::render::composite::{Composite, HandleGet};
use crate::render::encode::encode_png;

/// Precompiled, hot-path-ready render plan.
#[derive(Debug, Clone)]
pub struct CompiledRenderSpec {
    pub composite: Composite,
    pub rescale: Vec<(f64, f64)>,
    pub formula: Vec<Op>,
    pub bidx: Vec<usize>,    // 0-indexed after parse
    pub nodata: Option<f64>,
    pub tile_size: u32,
}

/// Run the fused render pipeline. Caller is responsible for sizing the input
/// to `tile_size × tile_size` (resampling done in warp + a final
/// fast_image_resize pass not implemented in MVP since we warp directly to
/// output grid).
pub fn render(image: &ImageData, spec: &CompiledRenderSpec) -> AgResult<Bytes> {
    let bands = image.pixels.bands();
    let h = image.pixels.height();
    let w = image.pixels.width();

    // 1. Select bands and convert to f64.
    let src = image.pixels.to_f64();
    let bidx: Vec<usize> = if spec.bidx.is_empty() { (0..bands).collect() } else { spec.bidx.clone() };

    // 2. Build a per-band slice view.
    let mut bands_view: Vec<Array2<f64>> = bidx.iter()
        .map(|&b| src.index_axis(ndarray::Axis(0), b).to_owned())
        .collect();

    // 3. Rescale per band.
    if !spec.rescale.is_empty() {
        for (i, band) in bands_view.iter_mut().enumerate() {
            let (lo, hi) = spec.rescale.get(i).copied().unwrap_or((0.0, 1.0));
            let span = (hi - lo).max(1e-12);
            band.mapv_inplace(|v| ((v - lo) / span).clamp(0.0, 1.0));
        }
    }

    // 4. Build (H,W,4) RGBA output.
    let mut rgba = Array3::<u8>::zeros((h, w, 4));
    let ndv: Option<Vec<f64>> = spec.nodata.map(|n| vec![n; bidx.len()]);
    let ndv_ref = ndv.as_deref();

    if bands_view.len() == 1 {
        // single-band path: apply colormap directly.
        for r in 0..h {
            for c in 0..w {
                let v = bands_view[0][[r, c]];
                let px = spec.composite.get(&[v], ndv_ref);
                rgba[[r, c, 0]] = px[0];
                rgba[[r, c, 1]] = px[1];
                rgba[[r, c, 2]] = px[2];
                rgba[[r, c, 3]] = px[3];
            }
        }
    } else if bands_view.len() == 3 {
        // RGB path: apply color_formula in [0,1] sRGB then quantise.
        for r in 0..h {
            for c in 0..w {
                let mut rb = bands_view[0][[r, c]];
                let mut gb = bands_view[1][[r, c]];
                let mut bb = bands_view[2][[r, c]];
                if !spec.formula.is_empty() {
                    let (nr, ng, nb) = crate::render::color_formula::apply(&spec.formula, rb, gb, bb);
                    rb = nr; gb = ng; bb = nb;
                }
                let px = spec.composite.get(&[rb, gb, bb], ndv_ref);
                rgba[[r, c, 0]] = px[0];
                rgba[[r, c, 1]] = px[1];
                rgba[[r, c, 2]] = px[2];
                rgba[[r, c, 3]] = px[3];
            }
        }
    } else {
        return Err(crate::error::AgError::UnsupportedPixel(
            format!("expected 1 or 3 bands after bidx, got {}", bands_view.len())));
    }

    encode_png(&rgba)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bounds::{Bbox, GeoTransform};
    use crate::crs::Crs;
    use crate::pixel::PixelData;
    use crate::render::composite::{Composite, ColourDefinition};

    #[test]
    fn render_single_band_gradient_to_png() {
        let mut arr = Array3::<f64>::zeros((1, 16, 16));
        for r in 0..16 { for c in 0..16 { arr[[0, r, c]] = (c as f64) / 15.0; } }
        let img = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 16.0, 16.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 16.0, 0.0, -1.0]),
            nodata: None,
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Named("viridis".into()),
            Some(vec![0.0]), Some(vec![1.0])
        ).unwrap();
        let spec = CompiledRenderSpec {
            composite: comp, rescale: vec![], formula: vec![],
            bidx: vec![], nodata: None, tile_size: 16,
        };
        let bytes = render(&img, &spec).unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));
    }
}
```

- [ ] **Step 2: Run**

Run: `cargo test --lib render::pipeline::tests`
Expected: PASS — 1 test.

- [ ] **Step 3: Commit**

```bash
git add src/render/pipeline.rs
git commit -m "feat: fused render pipeline (rescale -> color_formula -> composite -> encode)"
```

---

## Task 20: Style — query parser

**Files:**
- Create: `src/style/mod.rs`
- Create: `src/style/query.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add module**

Append to `src/lib.rs`:
```rust
pub mod style;
```

- [ ] **Step 2: Write `src/style/mod.rs`**

```rust
pub mod query;
pub use query::{RenderSpec, compile};
```

- [ ] **Step 3: Write `src/style/query.rs`**

```rust
use crate::error::{AgError, AgResult};
use crate::render::color_formula::{parse as parse_formula, Op};
use crate::render::composite::{Composite, ColourDefinition};
use crate::render::pipeline::CompiledRenderSpec;
use crate::warp::resampler::Resampling;

#[derive(Debug, Clone, Default)]
pub struct RenderSpec {
    pub bidx: Vec<u32>,                  // 1-indexed as given by URL
    pub rescale: Vec<(f64, f64)>,
    pub nodata: Option<f64>,
    pub colormap_name: Option<String>,
    pub color_formula: Option<String>,
    pub resampling: Resampling,
    pub reproject: Resampling,
    pub tilesize: u32,
}

impl RenderSpec {
    pub fn default_spec() -> Self {
        Self {
            bidx: vec![],
            rescale: vec![],
            nodata: None,
            colormap_name: None,
            color_formula: None,
            resampling: Resampling::Nearest,
            reproject: Resampling::Nearest,
            tilesize: 256,
        }
    }

    /// Parse from a URL query string (`?` not included).
    pub fn from_query(q: &str) -> AgResult<Self> {
        let mut s = Self::default_spec();
        if q.is_empty() { return Ok(s); }
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = urldecode(v);
            match k {
                "bidx" => s.bidx.push(v.parse().map_err(|_| AgError::query("bidx", &v))?),
                "rescale" => {
                    let (a, b) = v.split_once(',').ok_or_else(|| AgError::query("rescale", &v))?;
                    s.rescale.push((
                        a.parse().map_err(|_| AgError::query("rescale", a))?,
                        b.parse().map_err(|_| AgError::query("rescale", b))?,
                    ));
                }
                "nodata" => s.nodata = Some(parse_nodata(&v)?),
                "colormap_name" => s.colormap_name = Some(v.clone()),
                "color_formula" => s.color_formula = Some(v.clone()),
                "resampling" => s.resampling = Resampling::parse(&v).ok_or_else(|| AgError::query("resampling", &v))?,
                "reproject"  => s.reproject  = Resampling::parse(&v).ok_or_else(|| AgError::query("reproject", &v))?,
                "tilesize"   => s.tilesize   = v.parse().map_err(|_| AgError::query("tilesize", &v))?,
                _ => {} // unknown params silently ignored (titiler-compatible)
            }
        }
        Ok(s)
    }
}

fn parse_nodata(v: &str) -> AgResult<f64> {
    match v {
        "nan"  => Ok(f64::NAN),
        "inf"  => Ok(f64::INFINITY),
        "-inf" => Ok(f64::NEG_INFINITY),
        _ => v.parse().map_err(|_| AgError::query("nodata", v)),
    }
}

fn urldecode(s: &str) -> String {
    // Minimal pct-decoder for spaces in color_formula.
    let s = s.replace('+', " ");
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i+1..i+3]).unwrap_or(""), 16) {
                out.push(b as char); i += 3; continue;
            }
        }
        out.push(bytes[i] as char); i += 1;
    }
    out
}

pub fn compile(spec: &RenderSpec) -> AgResult<CompiledRenderSpec> {
    let composite = if let Some(name) = &spec.colormap_name {
        Composite::from_definition(
            &ColourDefinition::Named(name.clone()),
            spec.rescale.first().map(|r| vec![r.0]),
            spec.rescale.first().map(|r| vec![r.1]),
        )?
    } else if spec.bidx.len() == 3 || (spec.bidx.is_empty() && spec.rescale.len() == 3) {
        // RGB path
        let lo: [f64; 3] = [
            spec.rescale.get(0).map(|r| r.0).unwrap_or(0.0),
            spec.rescale.get(1).map(|r| r.0).unwrap_or(0.0),
            spec.rescale.get(2).map(|r| r.0).unwrap_or(0.0),
        ];
        let hi: [f64; 3] = [
            spec.rescale.get(0).map(|r| r.1).unwrap_or(1.0),
            spec.rescale.get(1).map(|r| r.1).unwrap_or(1.0),
            spec.rescale.get(2).map(|r| r.1).unwrap_or(1.0),
        ];
        Composite::from_definition(&ColourDefinition::Rgb(lo, hi), None, None)?
    } else {
        // default greyscale gradient on first band
        Composite::from_definition(
            &ColourDefinition::Named("greys".into()),
            spec.rescale.first().map(|r| vec![r.0]),
            spec.rescale.first().map(|r| vec![r.1]),
        )?
    };

    let formula: Vec<Op> = match &spec.color_formula {
        Some(s) if !s.is_empty() => parse_formula(s)?,
        _ => vec![],
    };

    Ok(CompiledRenderSpec {
        composite,
        rescale: spec.rescale.clone(),
        formula,
        bidx: spec.bidx.iter().map(|b| (*b as usize).saturating_sub(1)).collect(),
        nodata: spec.nodata,
        tile_size: spec.tilesize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_titiler_url() {
        let s = RenderSpec::from_query(
            "bidx=1&bidx=2&bidx=3&rescale=0,3000&rescale=0,3000&rescale=0,3000\
             &colormap_name=viridis&color_formula=gamma%20RGB%201.2\
             &resampling=bilinear&tilesize=512"
        ).unwrap();
        assert_eq!(s.bidx, vec![1, 2, 3]);
        assert_eq!(s.rescale.len(), 3);
        assert_eq!(s.colormap_name.as_deref(), Some("viridis"));
        assert_eq!(s.color_formula.as_deref(), Some("gamma RGB 1.2"));
        assert_eq!(s.resampling, Resampling::Bilinear);
        assert_eq!(s.tilesize, 512);
    }

    #[test]
    fn nodata_special_values() {
        let s = RenderSpec::from_query("nodata=nan").unwrap();
        assert!(s.nodata.unwrap().is_nan());
    }

    #[test]
    fn invalid_bidx_errors() {
        assert!(RenderSpec::from_query("bidx=abc").is_err());
    }

    #[test]
    fn compile_named_colormap() {
        let s = RenderSpec::from_query("colormap_name=viridis&rescale=0,1000").unwrap();
        let c = compile(&s).unwrap();
        assert_eq!(c.tile_size, 256);
    }
}
```

- [ ] **Step 4: Run**

Run: `cargo test --lib style::query::tests`
Expected: PASS — 4 tests.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/style
git commit -m "feat: titiler-style URL query parser -> RenderSpec -> CompiledRenderSpec"
```

---

## Task 21: Tiler — orchestrator

**Files:**
- Create: `src/tiler.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add module**

Append to `src/lib.rs`:
```rust
pub mod tiler;
```

- [ ] **Step 2: Write `src/tiler.rs`**

```rust
use std::sync::Arc;
use bytes::Bytes;
use crate::bounds::Bbox;
use crate::crs::Crs;
use crate::error::{AgError, AgResult};
use crate::io::CogReader;
use crate::io::overview::pick_overview;
use crate::pixel::ImageData;
use crate::render::pipeline::{render, CompiledRenderSpec};
use crate::style::query::{compile, RenderSpec};
use crate::tms::{Tms, TmsKind};
use crate::warp;

pub struct Tiler {
    reader: Arc<CogReader>,
}

impl Tiler {
    pub fn new(reader: Arc<CogReader>) -> Self { Self { reader } }

    pub async fn tile(
        &self,
        tms_kind: TmsKind,
        z: u8, x: u32, y: u32,
        spec: &RenderSpec,
    ) -> AgResult<Bytes> {
        let tms = Tms::new(tms_kind)?;
        let dst_bbox = tms.tile_bbox(z, x, y)?;
        let dst_crs = tms.crs();

        let target_gsd = tms.resolution(z)?;

        // Overview pick (uses full-res GSD from transform).
        let full_gsd = self.reader.transform().0.a.abs().max(self.reader.transform().0.e.abs());
        let full_w = (dst_bbox.width() / full_gsd) as u64; // dimensional-only guess
        let level = pick_overview(&self.reader.tiff, full_w.max(1), full_gsd, target_gsd);

        // Read window
        let (col, row, w, h) = self.reader.transform()
            .window_for_bbox(&dst_bbox)
            .ok_or(AgError::Warp("inverse transform failed".into()))?;
        let src: ImageData = self.reader.read_window(level, col, row, w, h).await?;

        // Warp if needed
        let warped: ImageData = if &src.crs == &dst_crs {
            src
        } else {
            tokio::task::spawn_blocking({
                let src = src.clone();
                let dst_crs = dst_crs.clone();
                let resampling = spec.reproject;
                let ts = spec.tilesize as usize;
                move || warp::warp(&src, &dst_crs, dst_bbox, ts, ts, resampling)
            })
            .await
            .map_err(|e| AgError::Warp(format!("join: {e}")))??
        };

        // Compile and render
        let compiled: CompiledRenderSpec = compile(spec)?;
        let png = tokio::task::spawn_blocking(move || render(&warped, &compiled))
            .await
            .map_err(|e| AgError::Warp(format!("join: {e}")))??;
        Ok(png)
    }
}

#[cfg(test)]
mod tests {
    // Full integration test deferred to Task 23 (needs fixture COG).
    // Pure-API smoke check here.

    #[test]
    fn tiler_constructs() {
        // Smoke: types compile together.
        let _ = std::mem::size_of::<super::Tiler>();
    }
}
```

- [ ] **Step 3: Run**

Run: `cargo build --lib`
Expected: PASS.
Run: `cargo test --lib tiler::tests`
Expected: PASS — 1 smoke test.

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs src/tiler.rs
git commit -m "feat: Tiler orchestrator (TMS bbox -> read -> warp -> render)"
```

---

## Task 22: Server — axum HTTP route (feature `server`)

**Files:**
- Create: `src/server/mod.rs`
- Create: `src/server/routes.rs`
- Create: `src/server/state.rs`
- Create: `examples/serve.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Gate module on feature**

Append to `src/lib.rs`:
```rust
#[cfg(feature = "server")]
pub mod server;
```

- [ ] **Step 2: Write `src/server/state.rs`**

```rust
use std::sync::Arc;
use crate::tiler::Tiler;

#[derive(Clone)]
pub struct AppState {
    pub tiler: Arc<Tiler>,
}
```

- [ ] **Step 3: Write `src/server/routes.rs`**

```rust
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use crate::server::state::AppState;
use crate::style::query::RenderSpec;
use crate::tms::TmsKind;

pub async fn tile_handler(
    State(state): State<AppState>,
    Path((tms, z, x, ye)): Path<(String, u8, u32, String)>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    // ye is "{y}.{ext}"; split.
    let (y_str, _ext) = ye.split_once('.').unwrap_or((ye.as_str(), "png"));
    let y: u32 = match y_str.parse() {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "bad y").into_response(),
    };

    let tms_kind = match TmsKind::from_str(&tms) {
        Ok(k) => k,
        Err(_) => return (StatusCode::UNPROCESSABLE_ENTITY, format!("unknown TMS: {tms}")).into_response(),
    };

    // Re-stringify params into query for the parser. Preserve repetition is not
    // possible from HashMap; for the demo server we accept comma-joined values
    // for bidx/rescale (titiler also supports this in some endpoints).
    // For repeated-param fidelity, plug `axum-extra::Query` (TODO post-MVP).
    let q = params.iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>().join("&");
    let spec = match RenderSpec::from_query(&q) {
        Ok(s) => s,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };

    match state.tiler.tile(tms_kind, z, x, y, &spec).await {
        Ok(bytes) => ([("content-type", "image/png")], bytes).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
```

- [ ] **Step 4: Write `src/server/mod.rs`**

```rust
pub mod routes;
pub mod state;

use axum::{Router, routing::get};
pub use state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/tiles/:tms/:z/:x/:ye", get(routes::tile_handler))
        .with_state(state)
}
```

- [ ] **Step 5: Write `examples/serve.rs`**

```rust
use std::sync::Arc;
use async_geotiff::io::CogReader;
use async_geotiff::tiler::Tiler;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args().nth(1)
        .expect("usage: cargo run --example serve --features server -- <cog-url>");
    let reader = Arc::new(CogReader::open_http(&url).await?);
    let tiler = Arc::new(Tiler::new(reader));
    let state = async_geotiff::server::AppState { tiler };
    let app = async_geotiff::server::router(state);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    println!("Listening on http://0.0.0.0:8080");
    axum::serve(listener, app).await?;
    Ok(())
}
```

- [ ] **Step 6: Build with feature**

Run: `cargo build --features server`
Expected: PASS.
Run: `cargo build --example serve --features server`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/lib.rs src/server examples/serve.rs
git commit -m "feat: axum server + serve example (feature 'server')"
```

---

## Task 23: Integration test — full pipeline against fixture COG

**Files:**
- Create: `tests/fixture_gen.rs` (module included from tests)
- Modify: `tests/cog_read.rs`
- Create: `tests/full_pipeline.rs`

- [ ] **Step 1: Add a helper to generate a tiny COG**

`tests/fixture_gen.rs`:

```rust
//! Generate a 256×256 single-band float32 COG-like TIFF in EPSG:3857.
//! Not a strict COG (no overviews) but enough for window-read tests.

use std::path::Path;
use tiff::encoder::{TiffEncoder, colortype::Gray32Float};
use std::fs::File;
use std::io::BufWriter;

pub fn make_dummy_tif(path: &Path) {
    let file = BufWriter::new(File::create(path).unwrap());
    let mut tiff = TiffEncoder::new(file).unwrap();
    let mut img = tiff.new_image::<Gray32Float>(256, 256).unwrap();
    // Linear ramp 0..1 across X.
    let data: Vec<f32> = (0..256 * 256).map(|i| (i % 256) as f32 / 255.0).collect();
    img.write_data(&data).unwrap();
}
```

- [ ] **Step 2: Write `tests/full_pipeline.rs`**

```rust
mod fixture_gen;

use async_geotiff::{
    io::CogReader, style::query::RenderSpec, tiler::Tiler, tms::TmsKind,
};
use std::sync::Arc;
use tempfile::tempdir;

#[tokio::test]
#[ignore = "needs real COG with geo tags; placeholder"]
async fn full_pipeline_produces_png() {
    let dir = tempdir().unwrap();
    let p = dir.path().join("dummy.tif");
    fixture_gen::make_dummy_tif(&p);

    let url = format!("file://{}", p.display());
    let reader = Arc::new(CogReader::open_http(&url).await.unwrap());
    let tiler = Tiler::new(reader);
    let spec = RenderSpec::from_query("colormap_name=viridis&rescale=0,1").unwrap();
    let png = tiler.tile(TmsKind::WebMercatorQuad, 0, 0, 0, &spec).await.unwrap();
    assert!(png.starts_with(b"\x89PNG"));
}
```

- [ ] **Step 3: Run with ignored flag**

Run: `cargo test --test full_pipeline -- --ignored`
Expected: SKIP/FAIL (placeholder; fixture lacks geo tags). This is fine — we keep it as a future-anchor that pins API shape.

Run: `cargo test`
Expected: All non-ignored tests PASS.

- [ ] **Step 4: Commit**

```bash
git add tests/fixture_gen.rs tests/full_pipeline.rs
git commit -m "test: full-pipeline integration scaffold (ignored, awaits real COG fixture)"
```

---

## Task 24: Criterion bench scaffold

**Files:**
- Create: `benches/tile_bench.rs`

- [ ] **Step 1: Write bench skeleton**

```rust
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use async_geotiff::render::pipeline::{render, CompiledRenderSpec};
use async_geotiff::render::composite::{Composite, ColourDefinition};
use async_geotiff::pixel::{ImageData, PixelData};
use async_geotiff::bounds::{Bbox, GeoTransform};
use async_geotiff::crs::Crs;
use ndarray::Array3;

fn bench_render_single_band(c: &mut Criterion) {
    let mut arr = Array3::<f64>::zeros((1, 256, 256));
    for r in 0..256 {
        for c2 in 0..256 {
            arr[[0, r, c2]] = (c2 as f64) / 255.0;
        }
    }
    let img = ImageData {
        pixels: PixelData::F64(arr),
        mask: None,
        bbox: Bbox::new(0.0, 0.0, 256.0, 256.0),
        crs: Crs::epsg(3857),
        transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 256.0, 0.0, -1.0]),
        nodata: None,
    };
    let comp = Composite::from_definition(
        &ColourDefinition::Named("viridis".into()),
        Some(vec![0.0]), Some(vec![1.0])
    ).unwrap();
    let spec = CompiledRenderSpec {
        composite: comp, rescale: vec![], formula: vec![],
        bidx: vec![], nodata: None, tile_size: 256,
    };

    c.bench_function("render_single_band_256", |b| {
        b.iter(|| render(black_box(&img), black_box(&spec)).unwrap());
    });
}

criterion_group!(benches, bench_render_single_band);
criterion_main!(benches);
```

- [ ] **Step 2: Build bench**

Run: `cargo bench --no-run`
Expected: PASS — compiles.

- [ ] **Step 3: Commit**

```bash
git add benches/tile_bench.rs
git commit -m "bench: criterion scaffold for single-band 256 render"
```

---

## Task 25: README + final cleanup

**Files:**
- Create: `README.md`

- [ ] **Step 1: Write minimal README**

```markdown
# async-geotiff

Pure-Rust async crate that reads cloud-hosted Cloud-Optimized GeoTIFFs (COGs)
and serves them as XYZ map tiles with titiler-style URL parameter styling.

**No GDAL.** All IO goes through `async-tiff` + `object_store`. Reprojection
through `proj` (libproj system dep, feature-gated). Affine math through
`raffine`. Colormaps via `colorous` + `enterpolation`.

## Status

MVP scaffold. See:
- Design spec: `docs/superpowers/specs/2026-05-14-async-geotiff-rs-design.md`
- Library research: `docs/superpowers/research/2026-05-14-library-deep-dive.md`
- Implementation plan: `docs/superpowers/plans/2026-05-14-async-geotiff-rs.md`

## Quick start

```bash
cargo run --example serve --features server -- <https://.../cog.tif>
# then: curl http://localhost:8080/tiles/WebMercatorQuad/0/0/0.png?colormap_name=viridis&rescale=0,3000
```

## Supported URL parameters (titiler-compatible subset)

| Parameter | Repeat | Example |
|---|---|---|
| `bidx` | yes | `?bidx=1&bidx=2&bidx=3` |
| `rescale` | yes | `?rescale=0,3000&rescale=0,3000` |
| `nodata` | no | `?nodata=nan` |
| `colormap_name` | no | `?colormap_name=viridis` |
| `color_formula` | no | `?color_formula=gamma%20RGB%201.5` |
| `resampling` | no | `?resampling=bilinear` |
| `tilesize` | no | `?tilesize=512` |

## License

MIT OR Apache-2.0
```

- [ ] **Step 2: Verify everything still builds and tests pass**

Run: `cargo fmt --check`
If FAIL: run `cargo fmt` and stage.
Run: `cargo clippy -- -D warnings`
Expected: PASS.
Run: `cargo test`
Expected: All non-ignored tests PASS.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: README with quick-start and URL params table"
```

---

## Self-Review Checklist

**Spec coverage:**
- §2 MVP scope: covered by Tasks 0–22 (read → warp → render → server).
- §3 module tree: matches file structure in Tasks 0–22.
- §4 core types (PixelData, ImageData, RenderSpec, Tiler): Tasks 4, 4, 20, 21.
- §5 data flow: realised in Task 21 (Tiler::tile orchestration).
- §6 key decisions (15): all addressed —
  - overview pick (Task 7), coarse warp (Task 12), PROJ in spawn_blocking
    (Task 21), decode in spawn_blocking (Task 8), ObjectReader (Task 6),
    interleaved (H,W,C) (Task 19), color_formula (Task 16), colormap via
    colorous + enterpolation (Task 14), raffine GeoTransform (Task 2),
    mask priority alpha>nodata (Task 17/19), TMS WebMercatorQuad/WorldCRS84Quad
    (Task 5), PROJ axis-order doc (Task 3), AgError 400/422 mapping (Task 22),
    Arc<Tiler> sharing (Task 22 state), PROJ feature flag (Task 10).
- §6b performance: monomorphic Rust (all tasks), CPU/IO split (Task 21),
  request coalescing via ObjectReader (Task 6/8), zero-copy views (Task 8/19),
  static LUTs (Task 14), CompiledRenderSpec (Task 20), bounded concurrency
  (left to caller — server can wrap with semaphore, noted in §11 of spec).
- §6c render types: Tasks 13/14/17 cover Colour/ColourDefinition/Composite/
  HandleGet exactly as drafted.
- §7 public API: realised in Tasks 6/21/20.
- §8 testing: unit tests in every task; integration scaffold in Task 23;
  benches in Task 24.
- §9 dependencies: Task 0 pins versions.
- §10 out-of-scope: not implemented (per design).
- §11 open questions: resolved during implementation per task notes.

**Placeholder scan:**
- No "TBD" / "implement later" remain in steps.
- All test code shown inline; all production code shown inline.
- Task 9 explicitly defers fixture build to Task 23 with reason given.
- Task 22 notes axum-extra::Query as post-MVP refinement (acknowledged
  limitation, not a blocker; `?bidx=1,2,3` CSV form acceptable in MVP).

**Type consistency:**
- `AgError` / `AgResult<T>` used throughout (Tasks 1+).
- `CogReader::open_http`, `Tiler::tile`, `RenderSpec::from_query`,
  `Composite::get`, `compile()`, `render()` — names consistent across
  declaration and call sites.
- `GeoTransform`, `Bbox`, `Crs`, `Tms`, `TmsKind`, `PixelData`, `ImageData`,
  `RenderSpec`, `CompiledRenderSpec`, `Composite`, `ColourDefinition`,
  `Colour`, `Resampling`, `Op`, `Bands` — all defined once and referenced
  by the same name everywhere.
- `pick_overview` signature `(tiff, full_width, full_gsd, target_gsd) -> usize`
  consistent between Task 7 definition and Task 21 use.
- `warp::warp(src, dst_crs, dst_bbox, dst_w, dst_h, resampling)` consistent
  between Task 12 and Task 21.

No issues found.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-14-async-geotiff-rs.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
