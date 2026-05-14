//! `Projector` — thin wrapper around `proj::Proj`.
//!
//! `Proj` is `!Send + !Sync` (FFI handle). Must live on a single thread —
//! typically inside `tokio::task::spawn_blocking`.

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
