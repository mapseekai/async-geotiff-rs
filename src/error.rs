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
        Self::Query {
            name: name.into(),
            msg: msg.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_helper_builds_message() {
        let e = AgError::query("bidx", "not an integer");
        assert_eq!(
            format!("{}", e),
            "invalid query parameter `bidx`: not an integer"
        );
    }

    #[test]
    fn unknown_tms_displays() {
        let e = AgError::UnknownTms("LolMercator".into());
        assert_eq!(format!("{}", e), "unknown TMS: LolMercator");
    }
}
