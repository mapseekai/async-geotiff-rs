use crate::bounds::Bbox;
use crate::crs::Crs;
use crate::error::{AgError, AgResult};
use std::str::FromStr;
use tile_grid::{tms as registry, Tms as InnerTms, Xyz};

/// Supported OGC TileMatrixSet identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileMatrixSet {
    WebMercatorQuad,
    WorldCrs84Quad,
}

impl TileMatrixSet {
    pub fn name(self) -> &'static str {
        match self {
            Self::WebMercatorQuad => "WebMercatorQuad",
            Self::WorldCrs84Quad => "WorldCRS84Quad",
        }
    }

    pub fn parse_name(s: &str) -> AgResult<Self> {
        match s {
            "WebMercatorQuad" | "webmercatorquad" => Ok(Self::WebMercatorQuad),
            "WorldCRS84Quad" | "worldcrs84quad" => Ok(Self::WorldCrs84Quad),
            other => Err(AgError::UnknownTms(other.into())),
        }
    }

    pub fn crs(self) -> Crs {
        match self {
            Self::WebMercatorQuad => Crs::epsg(3857),
            Self::WorldCrs84Quad => Crs::epsg(4326),
        }
    }
}

impl FromStr for TileMatrixSet {
    type Err = AgError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_name(s)
    }
}

/// One XYZ-slippy tile address within a [`TileMatrixSet`].
///
/// Groups the four values that previously travelled as positional parameters
/// (`tile_matrix_set, z, x, y`) through every tile-rendering signature.
/// Construct with a struct literal so `x` and `y` are named at the call site
/// and cannot be silently swapped:
///
/// ```
/// # use async_geotiff::tms::{TileCoord, TileMatrixSet};
/// let coord = TileCoord { tms: TileMatrixSet::WebMercatorQuad, z: 12, x: 3412, y: 1670 };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileCoord {
    pub tms: TileMatrixSet,
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

pub struct Tms {
    tile_matrix_set: TileMatrixSet,
    inner: InnerTms,
}

impl Tms {
    pub fn new(tile_matrix_set: TileMatrixSet) -> AgResult<Self> {
        let inner = registry()
            .lookup(tile_matrix_set.name())
            .map_err(|e| AgError::UnknownTms(format!("{}: {}", tile_matrix_set.name(), e)))?;
        Ok(Self {
            tile_matrix_set,
            inner,
        })
    }

    /// A process-lifetime cached `Tms` for `tile_matrix_set`. [`Self::new`]
    /// clones and re-sorts the whole matrix set and builds proj transformers;
    /// the tile pipeline resolves a `Tms` on every tile (and every mosaic
    /// asset), so cache one instance per variant. `Tms` is `Send + Sync`.
    pub fn cached(tile_matrix_set: TileMatrixSet) -> AgResult<&'static Tms> {
        use std::sync::OnceLock;
        fn slot(v: TileMatrixSet) -> &'static OnceLock<Tms> {
            static WEB_MERCATOR: OnceLock<Tms> = OnceLock::new();
            static WORLD_CRS84: OnceLock<Tms> = OnceLock::new();
            match v {
                TileMatrixSet::WebMercatorQuad => &WEB_MERCATOR,
                TileMatrixSet::WorldCrs84Quad => &WORLD_CRS84,
            }
        }
        let cell = slot(tile_matrix_set);
        if let Some(t) = cell.get() {
            return Ok(t);
        }
        // Build outside the cell (Tms::new is fallible); a concurrent racer's
        // instance is simply dropped by `get_or_init`.
        let built = Tms::new(tile_matrix_set)?;
        Ok(cell.get_or_init(|| built))
    }

    pub fn tile_matrix_set(&self) -> TileMatrixSet {
        self.tile_matrix_set
    }

    pub fn crs(&self) -> Crs {
        self.tile_matrix_set.crs()
    }

    /// Bbox of (z, x, y) in the TMS CRS. `y` is XYZ-slippy convention.
    pub fn tile_bbox(&self, z: u8, x: u32, y: u32) -> AgResult<Bbox> {
        let y_tms = self.slippy_to_tms_y(z, y)?;
        let bb = self.inner.xy_bounds(&Xyz::new(x.into(), y_tms.into(), z));
        Ok(Bbox::new(bb.left, bb.bottom, bb.right, bb.top))
    }

    /// CRS-unit resolution at zoom `z` (units per pixel of the matrix).
    pub fn resolution(&self, z: u8) -> AgResult<f64> {
        self.inner
            .resolution_z(z)
            .ok_or(AgError::TileOutOfBounds { z, x: 0, y: 0 })
    }

    fn slippy_to_tms_y(&self, z: u8, y: u32) -> AgResult<u32> {
        // WebMercatorQuad and WorldCRS84Quad are NW-origin; no flip.
        // Helper kept for future SW-origin TMSs.
        let matrix = self
            .inner
            .matrix_z(z)
            .ok_or(AgError::TileOutOfBounds { z, x: 0, y })?;
        let height: u64 = matrix.matrix_height.into();
        if u64::from(y) >= height {
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
        assert!("foo".parse::<TileMatrixSet>().is_err());
    }

    #[test]
    fn webmercatorquad_tile_0_0_0_is_full_world() {
        let tms = Tms::new(TileMatrixSet::WebMercatorQuad).unwrap();
        let bb = tms.tile_bbox(0, 0, 0).unwrap();
        assert_relative_eq!(bb.xmin, -20037508.342789244, epsilon = 1e-3);
        assert_relative_eq!(bb.xmax, 20037508.342789244, epsilon = 1e-3);
    }

    #[test]
    fn out_of_range_y_errors() {
        let tms = Tms::new(TileMatrixSet::WebMercatorQuad).unwrap();
        assert!(tms.tile_bbox(0, 0, 1).is_err());
    }

    #[test]
    fn resolution_decreases_with_zoom() {
        let tms = Tms::new(TileMatrixSet::WebMercatorQuad).unwrap();
        let r0 = tms.resolution(0).unwrap();
        let r10 = tms.resolution(10).unwrap();
        assert!(r0 > r10);
    }
}
