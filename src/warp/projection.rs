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

#[cfg(feature = "proj")]
use std::{cell::RefCell, collections::HashMap, rc::Rc};

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
        self.inner.convert_array(pts).map(|_| ()).map_err(|e| {
            let msg = e.to_string();
            let lower = msg.to_ascii_lowercase();
            // PROJ reports "Point outside of projection domain" (and
            // similar latitude/longitude-range messages) for coords a
            // tile may legitimately cover near the CRS edge. Tag these
            // so the tiler can blank the tile instead of 500-ing.
            if lower.contains("outside")
                || lower.contains("domain")
                || lower.contains("range")
                || lower.contains("invalid coordinate")
            {
                AgError::ProjectionDomain(msg)
            } else {
                AgError::Warp(format!("convert: {msg}"))
            }
        })
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
    static PROJ_CACHE: RefCell<HashMap<(Crs, Crs), Rc<Projector>>> = RefCell::new(HashMap::new());
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
    let key = (src.clone(), dst.clone());
    // 1) Materialise/insert under a short mutable borrow.
    PROJ_CACHE.with(|cell| -> AgResult<()> {
        if !cell.borrow().contains_key(&key) {
            let p = Projector::new(src, dst)?;
            cell.borrow_mut().insert(key.clone(), Rc::new(p));
        }
        Ok(())
    })?;
    // 2) Clone the Rc and release the borrow before calling `f` — `f` is
    //    free to re-enter `with_cached` for any other key without
    //    triggering RefCell BorrowMutError.
    let proj: Rc<Projector> = PROJ_CACHE
        .with(|cell| cell.borrow().get(&key).cloned())
        .expect("just inserted if missing");
    f(&proj)
}

/// Convert a batch of points from `src` to `dst` in place. For the very
/// common `EPSG:4326 ↔ EPSG:3857` pair, dispatches to a closed-form formula
/// (`crate::warp::mercator`) and skips PROJ entirely — saves ~150-300 µs per
/// cross-CRS XYZ tile on a 33×33 control grid. All other CRS pairs go
/// through `with_cached` → PROJ.
pub fn convert_batch(src: &Crs, dst: &Crs, pts: &mut [(f64, f64)]) -> AgResult<()> {
    // Identity: same CRS is a no-op. Lets the generic grid path serve
    // same-CRS boundary tiles (where warp_affine would stretch a clamped
    // crop) without paying a PROJ pipeline build.
    if src == dst {
        return Ok(());
    }
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

/// Reproject a boundary with 64 segments per edge. This samples curved
/// projection edges rather than assuming their extrema occur at corners.
/// Longitude discontinuities produce a broad envelope rather than a wrapped
/// bbox, so callers using a single rectangle do not discard either side.
pub(crate) fn reproject_boundary(
    corners: [(f64, f64); 4],
    from: &Crs,
    to: &Crs,
) -> AgResult<crate::bounds::Bbox> {
    let mut points = Vec::with_capacity(256);
    for edge in 0..4 {
        let a = corners[edge];
        let b = corners[(edge + 1) % 4];
        for step in 0..64 {
            let t = step as f64 / 64.0;
            points.push((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t));
        }
    }
    convert_batch_partial(from, to, &mut points)?;
    let mut bounds = crate::bounds::Bbox::new(
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for (x, y) in points
        .into_iter()
        .filter(|(x, y)| x.is_finite() && y.is_finite())
    {
        bounds.xmin = bounds.xmin.min(x);
        bounds.ymin = bounds.ymin.min(y);
        bounds.xmax = bounds.xmax.max(x);
        bounds.ymax = bounds.ymax.max(y);
    }
    if !bounds.xmin.is_finite() {
        return Err(AgError::ProjectionDomain(
            "boundary has no projectable points".into(),
        ));
    }
    Ok(bounds)
}

/// Preserve projectable points when only part of a grid is outside the CRS
/// domain. Invalid samples are NaN; configuration/initialization errors pass
/// through unchanged. Retry original inputs because a failed batch may have
/// partially mutated its output.
pub(crate) fn convert_batch_partial(
    from: &Crs,
    to: &Crs,
    points: &mut [(f64, f64)],
) -> AgResult<()> {
    let original = points.to_vec();
    match convert_batch(from, to, points) {
        Ok(()) => Ok(()),
        Err(AgError::ProjectionDomain(_)) => {
            for (output, input) in points.iter_mut().zip(original) {
                *output = input;
                match convert_batch(from, to, std::slice::from_mut(output)) {
                    Ok(()) => {}
                    Err(AgError::ProjectionDomain(_)) => *output = (f64::NAN, f64::NAN),
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
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
    fn polar_boundary_includes_extremum_between_corners() {
        let bbox = reproject_boundary(
            [(-90.0, 60.0), (0.0, 60.0), (0.0, 80.0), (-90.0, 80.0)],
            &Crs::epsg(4326),
            &Crs::epsg(3413),
        )
        .unwrap();
        assert!(bbox.ymin < -3_323_160.0, "{}", bbox.ymin);
    }

    #[test]
    fn boundary_retains_partly_projectable_coverage() {
        let bbox = reproject_boundary(
            [(-10.0, 80.0), (10.0, 80.0), (10.0, 100.0), (-10.0, 100.0)],
            &Crs::epsg(4326),
            &Crs::epsg(3413),
        )
        .unwrap();
        assert!(bbox.xmin.is_finite() && bbox.xmax > bbox.xmin);
    }

    #[test]
    fn lonlat_to_webmercator_origin() {
        let p = Projector::new(&Crs::epsg(4326), &Crs::epsg(3857)).unwrap();
        let mut pts = [(0.0_f64, 0.0_f64)];
        p.convert(&mut pts).unwrap();
        assert_relative_eq!(pts[0].0, 0.0, epsilon = 1e-3);
        assert_relative_eq!(pts[0].1, 0.0, epsilon = 1e-3);
    }
}
