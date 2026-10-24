use thiserror::Error;

/// Result type returned by the crate's fallible operations.
pub type AgResult<T> = Result<T, AgError>;

/// Reader, rendering and request-control failures.
///
/// Request admission failures are returned without waiting for capacity.
/// Cancellation and deadlines stop cooperative work; they do not forcibly stop
/// CPU closures that are already running.
#[derive(Debug, Error)]
pub enum AgError {
    /// An allocation, source shape or work request exceeds a configured limit.
    #[error("work resource limit: {0}")]
    ResourceLimit(String),

    /// The request limiter has no free admission slot.
    #[error("request admission limit reached")]
    Overloaded,

    /// The request's configured or per-call deadline expired.
    #[error("request deadline exceeded")]
    DeadlineExceeded,

    /// The caller explicitly cancelled the request.
    #[error("request cancelled")]
    Cancelled,

    /// A query, style document or band selection is invalid.
    #[error("invalid query parameter `{name}`: {msg}")]
    Query {
        /// Parameter name or document path identifying the invalid input.
        name: String,
        /// Explanation of the validation failure.
        msg: String,
    },

    /// The requested tile matrix set name is not supported.
    #[error("unknown TMS: {0}")]
    UnknownTms(String),

    /// The requested built-in color map name is not supported.
    #[error("unknown colormap: {0}")]
    UnknownColormap(String),

    /// A tile coordinate is outside its matrix or supported zoom range.
    #[error("out-of-bounds tile: z={z} x={x} y={y}")]
    TileOutOfBounds {
        /// Requested zoom level.
        z: u8,
        /// Requested tile column.
        x: u32,
        /// Requested tile row.
        y: u32,
    },

    /// Unsupported pixel data, channel layout or unavailable output format.
    #[error("unsupported pixel format: {0}")]
    UnsupportedPixel(String),

    /// Required georeferencing metadata is absent.
    #[error("missing geo metadata: {0}")]
    MissingGeo(&'static str),

    /// A color formula could not be parsed or applied.
    #[error("color formula: {0}")]
    ColorFormula(String),

    /// Reprojection setup or raster warping failed.
    #[error("warp: {0}")]
    Warp(String),

    /// A coordinate fell outside the source/target projection's valid
    /// domain. Not a failure: the tiler treats this as "nothing here"
    /// and returns a transparent tile (mirrors rio-tiler/titiler).
    #[error("projection domain: {0}")]
    ProjectionDomain(String),

    /// An underlying I/O operation failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// TIFF metadata, pixel decoding, image encoding or a source operation failed.
    #[error("async-tiff: {0}")]
    Tiff(String),

    /// Mosaic discovery, asset access or composition failed.
    #[error("mosaic: {0}")]
    Mosaic(String),

    /// An image encoder failed.
    #[error("encode: {0}")]
    Encode(#[from] image::ImageError),
}

impl AgError {
    /// Construct a validation error for a parameter name or document path.
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
