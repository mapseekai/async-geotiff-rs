# maptile Integration Gaps — 0.0.3 (Quick-Win Tier) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close three of maptile's HTTP-400 integration gaps — `ObjectReader`/`object_store` re-export (#6), Terrain colormap (#3), and inline JSON colormap (#2) — and release them as `async-geotiff` 0.0.3.

**Architecture:** Three independent, additive changes. #6 widens the public API surface (re-export crates already used internally, add a convenience constructor). #3 adds one builtin colormap LUT. #2 wires the *already-existing* `ColourDefinition` discrete/gradient machinery to a new `colormap` field on `TileStyle` via an explicit JSON-shape router. No change to the render hot path.

**Tech Stack:** Rust, `async-tiff 0.3`, `object_store 0.13`, `serde_json`, `tokio` (tests). Source of truth spec: `docs/superpowers/specs/2026-05-30-maptile-integration-gaps-design.md`.

**Conventions in this repo (follow them):**
- Tests live in `#[cfg(test)] mod tests` at the bottom of the same source file (not a `tests/` dir).
- Local fixture COGs live in `data/`; open them with `LocalFileSystem::new_with_prefix(".")` + `Path::from("data/<file>")`. Pattern: `src/io/stats.rs:398-401`.
- Errors use `AgError::query(field, msg)` (→ HTTP 400) and `AgError::UnknownColormap(name)`.
- Run a single test: `cargo test <test_name> -- --exact --nocapture`. Run all: `cargo test`.

---

## File Structure

| File | Change | Responsibility |
|------|--------|----------------|
| `src/io/mod.rs` | Modify | Re-export `ObjectReader` and `object_store` |
| `src/io/cog.rs` | Modify | Add `open_object_store*` convenience constructors |
| `src/render/colormap.rs` | Modify | Add `"terrain"` builtin LUT + test |
| `src/render/composite.rs` | (read only) | Existing `ColourDefinition` variants reused |
| `src/style/colormap_param.rs` | Create | `parse_colormap_param`: JSON-shape → `ColourDefinition` |
| `src/style/mod.rs` | Modify | `mod colormap_param;` |
| `src/style/params.rs` | Modify | `colormap` field, parse arm, compile wiring, `#[non_exhaustive]` |
| `Cargo.toml` | Modify | Version → 0.0.3 |
| `CHANGELOG.md` | Modify | Document the three changes |

---

## Task 1: Re-export `ObjectReader` + `object_store`, add `open_object_store` (#6)

**Files:**
- Modify: `src/io/mod.rs:1-8`
- Modify: `src/io/cog.rs` (add constructors after `open_with_reader_and_config`, ~line 184)

- [ ] **Step 1: Write the failing test**

Add to the bottom of `src/io/cog.rs`. If a `#[cfg(test)] mod tests { ... }` block already exists there, add the function inside it; otherwise create the block. Use the small RGB fixture already in the repo.

```rust
#[cfg(test)]
mod open_object_store_tests {
    use crate::io::{CogReader, ObjectReader};
    use object_store::local::LocalFileSystem;
    use std::sync::Arc;

    #[tokio::test]
    async fn open_object_store_matches_open_with_reader() {
        let fs = Arc::new(LocalFileSystem::new_with_prefix(".").unwrap());
        let key = "data/cog_uint8_rgb_nodata.tif";

        // New convenience path.
        let via_helper = CogReader::open_object_store(fs.clone(), key)
            .await
            .expect("open_object_store");

        // Old explicit path (must still produce an equivalent reader).
        let reader = ObjectReader::new(fs, object_store::path::Path::from(key));
        let via_reader = CogReader::open_with_reader(reader)
            .await
            .expect("open_with_reader");

        assert_eq!(via_helper.transform(), via_reader.transform());
        assert_eq!(via_helper.src_crs(), via_reader.src_crs());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test open_object_store_matches_open_with_reader -- --exact`
Expected: FAIL — `no function or associated item named open_object_store` and `unresolved import crate::io::ObjectReader`.

- [ ] **Step 3: Add the re-exports**

Edit `src/io/mod.rs` to (final state):

```rust
mod cache;
pub mod cog;
mod decode;
pub mod stats;
pub mod window;

pub use cache::CacheConfig;
pub use cog::{CogReader, ReaderConfig};

// Re-exported so crate consumers can name the reader type and wire their own
// object_store WITHOUT taking a direct, version-locked dependency on async-tiff
// or object_store. `object_store::ClientOptions` is already in our public API
// (see `CogReader::open_http_with_options`), so this only formalises an existing
// requirement.
pub use async_tiff::reader::ObjectReader;
pub use object_store;
```

- [ ] **Step 4: Add the convenience constructors**

In `src/io/cog.rs`, immediately after the `open_with_reader_and_config` method (after its closing `}` near line 184, still inside `impl CogReader`), add:

```rust
    /// Open a COG from an existing object store and key, building the
    /// `ObjectReader` internally. Lets callers wire any `object_store`
    /// backend (S3/MinIO, GCS, local, …) without naming `ObjectReader`.
    pub async fn open_object_store(
        store: Arc<dyn ObjectStore>,
        key: &str,
    ) -> AgResult<Self> {
        Self::open_object_store_with_config(store, key, ReaderConfig::default()).await
    }

    /// Like [`open_object_store`] but lets the caller size the decoded-window
    /// cache.
    pub async fn open_object_store_with_cache(
        store: Arc<dyn ObjectStore>,
        key: &str,
        cache: CacheConfig,
    ) -> AgResult<Self> {
        Self::open_object_store_with_config(
            store,
            key,
            ReaderConfig {
                cache,
                ..ReaderConfig::default()
            },
        )
        .await
    }

    /// Like [`open_object_store`] but lets callers configure cache capacity
    /// and CPU decode concurrency.
    pub async fn open_object_store_with_config(
        store: Arc<dyn ObjectStore>,
        key: &str,
        config: ReaderConfig,
    ) -> AgResult<Self> {
        let reader = ObjectReader::new(store, Path::from(key));
        Self::open_with_reader_and_config(reader, config).await
    }
```

`ObjectStore`, `Path`, `Arc`, `ReaderConfig`, `CacheConfig` are already imported at the top of `cog.rs` (lines 3-15) — no new `use` needed.

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test open_object_store_matches_open_with_reader -- --exact`
Expected: PASS.

- [ ] **Step 6: Verify the re-export compiles as public API**

Run: `cargo build`
Expected: builds clean. (Confirms `pub use object_store;` and `pub use async_tiff::reader::ObjectReader;` resolve.)

- [ ] **Step 7: Commit**

```bash
git add src/io/mod.rs src/io/cog.rs
git commit -m "feat(io): re-export ObjectReader/object_store and add open_object_store"
```

---

## Task 2: Add the Terrain builtin colormap (#3)

**Files:**
- Modify: `src/render/colormap.rs:9-31` (add a non-colorous builtin path) and its test module.

Background: `builtin_lut` currently only resolves the seven colorous gradients (`colormap.rs:9-17`). Terrain is not in colorous. It has well-known matplotlib stops; build it with the existing `lut_from_stops` (`colormap.rs:34`).

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `src/render/colormap.rs`:

```rust
    #[test]
    fn terrain_low_is_blue_high_is_white() {
        let lut = builtin_lut("terrain").unwrap();
        let [r0, g0, b0, a0] = lut[0];
        // Low end is deep blue-ish: blue channel dominant, opaque.
        assert!(b0 > 120 && b0 > r0 && b0 > g0, "low={:?}", [r0, g0, b0]);
        assert_eq!(a0, 255);
        // High end is near-white.
        let [r255, g255, b255, _] = lut[255];
        assert!(r255 > 240 && g255 > 240 && b255 > 240, "high={:?}", [r255, g255, b255]);
    }

    #[test]
    fn terrain_is_case_insensitive() {
        assert!(builtin_lut("Terrain").is_ok());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test terrain_low_is_blue_high_is_white -- --exact`
Expected: FAIL — `builtin_lut("terrain")` returns `Err(UnknownColormap)`, so `.unwrap()` panics.

- [ ] **Step 3: Implement the terrain branch**

In `src/render/colormap.rs`, replace the body of `builtin_lut` (lines 19-31) with a version that checks the non-colorous builtins first:

```rust
/// Matplotlib `terrain` stops: deep blue → cyan → green → tan → brown → white.
/// Not part of the `colorous` set, so built from explicit stops.
const TERRAIN_STOPS: &[(f64, [f64; 4])] = &[
    (0.00, [0.2, 0.2, 0.6, 1.0]),
    (0.15, [0.0, 0.6, 1.0, 1.0]),
    (0.25, [0.0, 0.8, 0.4, 1.0]),
    (0.50, [1.0, 1.0, 0.6, 1.0]),
    (0.75, [0.5, 0.36, 0.33, 1.0]),
    (1.00, [1.0, 1.0, 1.0, 1.0]),
];

pub fn builtin_lut(name: &str) -> AgResult<Lut> {
    if name.eq_ignore_ascii_case("terrain") {
        let stops: Vec<(f64, Colour)> = TERRAIN_STOPS
            .iter()
            .map(|(p, c)| (*p, Colour::Seq(*c)))
            .collect();
        return lut_from_stops(&stops);
    }
    let g = BUILTINS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, g)| *g)
        .ok_or_else(|| AgError::UnknownColormap(name.into()))?;
    let mut lut = [[0u8; 4]; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        let c = g.eval_rational(i, 255);
        *slot = [c.r, c.g, c.b, 255];
    }
    Ok(lut)
}
```

`Colour` is already imported (`colormap.rs:4`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test terrain -- --nocapture`
Expected: both `terrain_*` tests PASS, and the existing `unknown_colormap_errors` / `viridis_*` tests still PASS.

- [ ] **Step 5: Commit**

```bash
git add src/render/colormap.rs
git commit -m "feat(render): add terrain builtin colormap"
```

---

## Task 3: Inline JSON colormap on `TileStyle` (#2)

This is mostly wiring — `ColourDefinition::Discrete` / `ColoursAndBreaks` and `Composite::from_definition` already exist (`src/render/composite.rs:17-23, 106`). We add a JSON-shape router and a `colormap` field.

**JSON contract (titiler-aligned):**
- Discrete (JSON object): `{"1":[255,0,0,255], "2":[0,255,0,255]}` → `ColourDefinition::Discrete`
- Gradient (JSON array of `[value, [r,g,b,a]]`): `[[0.0,[255,0,0,255]],[1.0,[0,0,255,255]]]` → `ColourDefinition::ColoursAndBreaks`

`colormap` and `colormap_name` are mutually exclusive.

### Task 3a: The JSON-shape router

**Files:**
- Create: `src/style/colormap_param.rs`
- Modify: `src/style/mod.rs`

- [ ] **Step 1: Register the module**

In `src/style/mod.rs`, add the line (alongside the other `mod`/`pub mod` declarations — match the existing style of that file):

```rust
mod colormap_param;
```

- [ ] **Step 2: Write the failing test (in the new file)**

Create `src/style/colormap_param.rs` with ONLY the test module first, so it fails to compile against the not-yet-written function:

```rust
//! Parse titiler-style `colormap=` JSON into the library's `ColourDefinition`.
//!
//! Routing is by JSON shape, NOT by serde's untagged ordering (which is
//! first-variant-wins and fragile): a leading `{` is a discrete value→colour
//! map; a leading `[` is a list of `[value, colour]` gradient breaks.

use crate::error::{AgError, AgResult};
use crate::render::colour::Colour;
use crate::render::composite::ColourDefinition;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_discrete_object() {
        let def = parse_colormap_param(r#"{"1":[255,0,0,255],"2":[0,255,0,255]}"#).unwrap();
        match def {
            ColourDefinition::Discrete(pairs) => {
                assert!(pairs.iter().any(|(k, _)| *k == 1));
                assert!(pairs.iter().any(|(k, _)| *k == 2));
                assert_eq!(pairs.len(), 2);
            }
            other => panic!("expected Discrete, got {other:?}"),
        }
    }

    #[test]
    fn parses_gradient_array() {
        let def =
            parse_colormap_param(r#"[[0.0,[255,0,0,255]],[1.0,[0,0,255,255]]]"#).unwrap();
        match def {
            ColourDefinition::ColoursAndBreaks(breaks) => {
                assert_eq!(breaks.len(), 2);
                assert_eq!(breaks[0].0, 0.0);
                assert_eq!(breaks[1].0, 1.0);
            }
            other => panic!("expected ColoursAndBreaks, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_query_error() {
        let err = parse_colormap_param("{not json").unwrap_err();
        assert!(matches!(err, AgError::Query { .. }));
    }

    #[test]
    fn non_object_non_array_is_query_error() {
        let err = parse_colormap_param("\"viridis\"").unwrap_err();
        assert!(matches!(err, AgError::Query { .. }));
    }

    #[test]
    fn discrete_non_integer_key_is_query_error() {
        let err = parse_colormap_param(r#"{"abc":[1,2,3,4]}"#).unwrap_err();
        assert!(matches!(err, AgError::Query { .. }));
    }
}
```

NOTE: `AgError::query(name, msg)` constructs the variant `AgError::Query { name, msg }` (confirmed in `src/error.rs:8,51`), so `matches!(err, AgError::Query { .. })` is correct as written.

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib colormap_param -- --nocapture`
Expected: FAIL to COMPILE — `cannot find function parse_colormap_param`.

- [ ] **Step 4: Implement the router**

Add above the `#[cfg(test)]` block in `src/style/colormap_param.rs`:

```rust
/// Route titiler `colormap=` JSON to a `ColourDefinition` by top-level shape.
pub fn parse_colormap_param(s: &str) -> AgResult<ColourDefinition> {
    let head = s.trim_start().chars().next();
    match head {
        Some('{') => {
            // Discrete: { "<int>": [r,g,b,a], ... }
            let map: std::collections::BTreeMap<String, Colour> = serde_json::from_str(s)
                .map_err(|e| AgError::query("colormap", format!("discrete colormap: {e}")))?;
            let mut pairs = Vec::with_capacity(map.len());
            for (k, c) in map {
                let key: i64 = k
                    .parse()
                    .map_err(|_| AgError::query("colormap", format!("key {k:?} is not an integer")))?;
                pairs.push((key, c));
            }
            Ok(ColourDefinition::Discrete(pairs))
        }
        Some('[') => {
            // Gradient: [ [value, [r,g,b,a]], ... ]
            let breaks: Vec<(f64, Colour)> = serde_json::from_str(s)
                .map_err(|e| AgError::query("colormap", format!("gradient colormap: {e}")))?;
            Ok(ColourDefinition::ColoursAndBreaks(breaks))
        }
        _ => Err(AgError::query(
            "colormap",
            "expected a JSON object (discrete) or array (gradient)",
        )),
    }
}
```

`ColourDefinition`, `Colour`, and the `AgError` imports are already at the top of the file (Step 2). `Colour` deserializes from a `[r,g,b,a]` array (see `src/render/colour.rs:55`), so `BTreeMap<String, Colour>` and `Vec<(f64, Colour)>` both work directly.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib colormap_param -- --nocapture`
Expected: all five tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src/style/mod.rs src/style/colormap_param.rs
git commit -m "feat(style): add titiler colormap JSON router"
```

### Task 3b: Wire `colormap` into `TileStyle` + compile

**Files:**
- Modify: `src/style/params.rs:12-43` (struct + Default), `:96-139` (apply_param), `:162-221` (compile)

- [ ] **Step 1: Write the failing test**

Add to `src/style/params.rs` `#[cfg(test)] mod tests` (create the block at the bottom if absent):

```rust
    #[test]
    fn colormap_discrete_compiles_to_discrete_composite() {
        use crate::render::composite::CompositeKind;
        let mut s = TileStyle::default();
        s.colormap = Some(r#"{"1":[255,0,0,255]}"#.to_string());
        let compiled = compile(&s).unwrap();
        assert_eq!(compiled.composite.kind, CompositeKind::Discrete);
    }

    #[test]
    fn colormap_and_colormap_name_are_mutually_exclusive() {
        let mut s = TileStyle::default();
        s.colormap = Some(r#"{"1":[255,0,0,255]}"#.to_string());
        s.colormap_name = Some("viridis".to_string());
        assert!(compile(&s).is_err());
    }

    #[test]
    fn colormap_param_is_parsed_from_query() {
        let s = TileStyle::from_query("colormap=%7B%221%22%3A%5B255%2C0%2C0%2C255%5D%7D").unwrap();
        assert_eq!(s.colormap.as_deref(), Some(r#"{"1":[255,0,0,255]}"#));
    }
```

(The query string in the third test is URL-encoded `{"1":[255,0,0,255]}`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib colormap_discrete_compiles_to_discrete_composite -- --exact`
Expected: FAIL to COMPILE — `TileStyle` has no field `colormap`.

- [ ] **Step 3: Add the field + Default + `#[non_exhaustive]`**

In `src/style/params.rs`, add `#[non_exhaustive]` to the struct and the `colormap` field (place it right after `colormap_name`):

```rust
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TileStyle {
    pub bidx: Vec<u32>, // 1-indexed as given in URL
    pub rescale: Vec<(f64, f64)>,
    pub stretch_kind: Option<String>,
    pub stretch_pc: Option<(f64, f64)>,
    pub stretch_sigma: Option<f64>,
    pub nodata: Option<f64>,
    pub colormap_name: Option<String>,
    /// Inline titiler-style `colormap=` JSON (discrete object or gradient array).
    /// Mutually exclusive with `colormap_name`.
    pub colormap: Option<String>,
    pub color_formula: Option<String>,
    pub resampling: Resampling,
    pub reproject: Resampling,
    pub tilesize: u32,
}
```

In the `Default` impl, add `colormap: None,` right after `colormap_name: None,`:

```rust
            colormap_name: None,
            colormap: None,
```

- [ ] **Step 4: Add the parse arm**

In `apply_param` (`params.rs:96`), add a `"colormap"` arm right after the `"colormap_name"` arm (line 111):

```rust
            "colormap_name" => self.colormap_name = Some(v.to_owned()),
            "colormap" => self.colormap = Some(v.to_owned()),
```

- [ ] **Step 5: Wire `colormap` into `compile`**

In `compile` (`params.rs:162`), add the mutual-exclusion guard near the top (right after `validate_tilesize(...)?;`):

```rust
    if style.colormap.is_some() && style.colormap_name.is_some() {
        return Err(AgError::query(
            "colormap",
            "colormap and colormap_name are mutually exclusive",
        ));
    }
```

Update the `auto_rgb` computation (lines 168-171) to also exclude `colormap`:

```rust
    let auto_rgb = style.bidx.is_empty()
        && style.rescale.is_empty()
        && style.colormap_name.is_none()
        && style.colormap.is_none()
        && style.color_formula.is_none();
```

Add the inline-colormap branch as the FIRST arm of the `composite` selection (before the `colormap_name` branch at line 173). Change `let composite = if let Some(name) = &style.colormap_name {` into a preceding `if let`:

```rust
    let composite = if let Some(cm) = &style.colormap {
        // Inline JSON colormap → reuse the existing ColourDefinition machinery.
        // Optional single rescale range supplies vmin/vmax for the gradient case.
        let def = crate::style::colormap_param::parse_colormap_param(cm)?;
        if style.rescale.len() > 1 {
            return Err(AgError::query(
                "rescale",
                "colormap accepts at most one rescale range",
            ));
        }
        Composite::from_definition(
            &def,
            style.rescale.first().map(|r| vec![r.0]),
            style.rescale.first().map(|r| vec![r.1]),
        )?
    } else if let Some(name) = &style.colormap_name {
        // ... existing colormap_name branch unchanged ...
```

Leave the rest of the `if/else` chain (colormap_name → rgb → greys) exactly as-is.

Make the router function visible to `params.rs`: in `src/style/mod.rs`, the `mod colormap_param;` from Task 3a is in the same `style` module, so `crate::style::colormap_param::parse_colormap_param` resolves. (The function is already `pub`.)

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib params -- --nocapture` then `cargo test --lib colormap -- --nocapture`
Expected: the three new `colormap_*` tests PASS; all pre-existing `params`/`compile` tests still PASS.

- [ ] **Step 7: Full build + test sweep**

Run: `cargo build && cargo test`
Expected: clean build, whole suite green. If any *other* crate-internal `TileStyle { ... }` struct literal now warns/breaks from `#[non_exhaustive]`, that is expected only for *external* crates — internal literals are unaffected; if an internal literal fails for an unrelated reason, fix it to include `colormap: None`.

- [ ] **Step 8: Commit**

```bash
git add src/style/params.rs
git commit -m "feat(style): accept inline colormap JSON on TileStyle"
```

---

## Task 4: Release 0.0.3

**Files:**
- Modify: `Cargo.toml` (version field)
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Check current version**

Run: `grep '^version' Cargo.toml`
Expected: shows the current version (e.g. `version = "0.0.2"`).

- [ ] **Step 2: Bump to 0.0.3**

Edit `Cargo.toml`, set:

```toml
version = "0.0.3"
```

- [ ] **Step 3: Update CHANGELOG**

Open `CHANGELOG.md`. Move the relevant items out of `Unreleased` (or add a new section) under a `## [0.0.3]` heading. Add these bullets (match the file's existing bullet style):

```markdown
## [0.0.3]

### Added
- Re-export `ObjectReader` and the `object_store` crate from `async_geotiff::io`, and add `CogReader::open_object_store{,_with_cache,_with_config}` so consumers can open a COG from an existing `Arc<dyn ObjectStore>` without depending on `async-tiff` directly (#6).
- `terrain` builtin colormap (matplotlib stops), in addition to the colorous set (#3).
- Inline `colormap=` JSON on `TileStyle` — discrete (`{"1":[r,g,b,a]}`) and gradient (`[[v,[r,g,b,a]]]`) maps, mutually exclusive with `colormap_name` (#2).

### Changed
- `TileStyle` is now `#[non_exhaustive]`; construct it with `..Default::default()`.
```

- [ ] **Step 4: Verify it builds and the version is set**

Run: `cargo build && grep '^version' Cargo.toml`
Expected: clean build; `version = "0.0.3"`.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml CHANGELOG.md
git commit -m "chore(release): 0.0.3 — object_store re-export, terrain, inline colormap"
```

---

## Final verification

- [ ] Run the full suite: `cargo test`
- [ ] Run clippy (repo keeps it clean): `cargo clippy --all-targets -- -D warnings`
- [ ] Confirm the public surface: `cargo doc --no-deps` builds without warnings about the new items.

## Notes for the engineer

- **Fixture dependency:** Task 1's test reads `data/cog_uint8_rgb_nodata.tif` (confirmed present, 2.2 KB). The test only compares the two open paths to each other, so any readable COG in `data/` would also work if it ever moves.
- **Out of scope for 0.0.3** (later plans): `unscale` (#4), `mode` resampling (#5), band-math `expression`/`Index` (#1). See the spec's phasing table.
```