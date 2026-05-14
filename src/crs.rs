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
