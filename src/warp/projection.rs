//! `Projector` — thin wrapper around `proj::Proj`.
//!
//! `Proj` is `!Send + !Sync` (FFI handle). Must live on a single thread —
//! typically inside `tokio::task::spawn_blocking`.
//!
//! Each `Proj::new_known_crs` call materialises a fresh PROJ context (parses
//! the proj-string, opens the grid database, etc.) — 5-30 ms on the first
//! hit, non-trivial on subsequent ones. For a tile server every cross-CRS
//! tile would pay that cost twice (bbox reprojection + warp grid).
//!
//! `with_cached` keeps a `thread_local!` map of `(src, dst) → Projector`
//! and reuses entries within the same blocking-pool worker thread. Tokio's
//! blocking pool reuses threads up to `max_blocking_threads`, so cache hit
//! rate is high after warm-up. This is the Rust-side analogue of the
//! async-geotiff guidance to "do PROJ init once, reuse the handle".

use std::cell::RefCell;
use std::collections::HashMap;

use crate::crs::Crs;
use crate::error::{AgError, AgResult};

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
        Ok(Self {
            inner,
            src: src.clone(),
            dst: dst.clone(),
        })
    }

    /// Convert in place a batch of `(x, y)` points.
    pub fn convert(&self, pts: &mut [(f64, f64)]) -> AgResult<()> {
        self.inner
            .convert_array(pts)
            .map(|_| ())
            .map_err(|e| AgError::Warp(format!("convert: {e}")))
    }

    pub fn src(&self) -> &Crs {
        &self.src
    }
    pub fn dst(&self) -> &Crs {
        &self.dst
    }
}

#[cfg(feature = "proj")]
thread_local! {
    /// Per-blocking-thread cache of `(src, dst) → Projector`. `Proj` is
    /// `!Send`, so we cannot share across threads — this is the only
    /// place a Projector lives.
    static PROJ_CACHE: RefCell<HashMap<(Crs, Crs), Projector>> = RefCell::new(HashMap::new());
}

#[cfg(feature = "proj")]
/// Run `f` with a (possibly-cached) `Projector` for `(src, dst)`.
///
/// MUST be called inside `tokio::task::spawn_blocking` (or any synchronous
/// thread context) — `Proj` is `!Send`, so callers cannot hold the
/// projector across `.await` points.
pub fn with_cached<R>(
    src: &Crs,
    dst: &Crs,
    f: impl FnOnce(&Projector) -> AgResult<R>,
) -> AgResult<R> {
    PROJ_CACHE.with(|cell| {
        let key = (src.clone(), dst.clone());
        // Build on miss outside the borrow scope to keep RefCell usage tight.
        let need_build = !cell.borrow().contains_key(&key);
        if need_build {
            let p = Projector::new(src, dst)?;
            cell.borrow_mut().insert(key.clone(), p);
        }
        let map = cell.borrow();
        let proj = map
            .get(&key)
            .expect("just inserted if missing");
        f(proj)
    })
}

/// Convert a batch of points from `src` to `dst` in place. For the very
/// common `EPSG:4326 ↔ EPSG:3857` pair, dispatches to a closed-form formula
/// (`crate::warp::mercator`) and skips PROJ entirely — saves ~150-300 µs per
/// cross-CRS XYZ tile on a 33×33 control grid. All other CRS pairs go
/// through `with_cached` → PROJ.
pub fn convert_batch(src: &Crs, dst: &Crs, pts: &mut [(f64, f64)]) -> AgResult<()> {
    if is_epsg(src, 4326) && is_epsg(dst, 3857) {
        crate::warp::mercator::lonlat_to_mercator(pts);
        return Ok(());
    }
    if is_epsg(src, 3857) && is_epsg(dst, 4326) {
        crate::warp::mercator::mercator_to_lonlat(pts);
        return Ok(());
    }
    #[cfg(feature = "proj")]
    {
        with_cached(src, dst, |p| p.convert(pts))
    }
    #[cfg(not(feature = "proj"))]
    {
        let _ = (src, dst, pts);
        Err(AgError::Warp(
            "non-Mercator CRS conversion requested but crate built without `proj`".into(),
        ))
    }
}

/// True when `crs` names EPSG:<code> in any of its common spellings.
/// Zero-allocation: parses the digits instead of formatting three Strings
/// — `convert_batch` calls this twice per tile on the hot path.
fn is_epsg(crs: &Crs, code: u32) -> bool {
    let s = crs.as_str();
    let digits = s
        .strip_prefix("EPSG:")
        .or_else(|| s.strip_prefix("epsg:"))
        .unwrap_or(s);
    digits.parse::<u32>().map(|c| c == code).unwrap_or(false)
}

#[cfg(not(feature = "proj"))]
pub fn with_cached<R>(
    _src: &Crs,
    _dst: &Crs,
    _f: impl FnOnce(&Projector) -> AgResult<R>,
) -> AgResult<R> {
    Err(AgError::Warp("crate built without `proj` feature".into()))
}

#[cfg(not(feature = "proj"))]
pub struct Projector {
    _private: (),
}

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
    use approx::assert_relative_eq;

    #[test]
    fn lonlat_to_webmercator_origin() {
        let p = Projector::new(&Crs::epsg(4326), &Crs::epsg(3857)).unwrap();
        let mut pts = [(0.0_f64, 0.0_f64)];
        p.convert(&mut pts).unwrap();
        assert_relative_eq!(pts[0].0, 0.0, epsilon = 1e-3);
        assert_relative_eq!(pts[0].1, 0.0, epsilon = 1e-3);
    }
}
