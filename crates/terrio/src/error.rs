//! Errors returned by terrio.

/// A terrio error.
///
/// New variants may be added in minor releases: match with a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A file system or network I/O error.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// An error from a raster format driver.
    #[error("{0}")]
    Raster(#[from] wbraster::RasterError),
    /// An error from a vector format driver.
    #[error("{0}")]
    Vector(#[from] wbvector::GeoError),
    /// A coordinate reference system could not be parsed or used.
    #[error("coordinate reference system: {0}")]
    Crs(String),
    /// An HTTP request failed.
    #[error("HTTP: {0}")]
    Http(String),
    /// The dataset's format is not recognized, or the operation is not supported for it.
    #[error("{0}")]
    Unsupported(String),
    /// An argument or option is invalid.
    #[error("{0}")]
    InvalidArgument(String),
    /// A decoded payload exceeded a decoded-byte limit: a PMTiles directory, metadata or tile
    /// (`PmTilesReadLimits`), a GeoTIFF chunk or tag value (`TiffReadLimits`), or a
    /// bounded VSI file read (after archive decompression).
    #[error("{resource} exceeds decoded-byte limit {limit}")]
    DecodedLimitExceeded {
        /// The payload kind (directory, metadata, tile, TIFF chunk/tag, or VSI file).
        resource: &'static str,
        /// The maximum allowed decoded bytes.
        limit: usize,
    },
    /// A dataset was found but its contents are not valid.
    #[error("{0}")]
    InvalidData(String),
}

impl Error {
    /// The error of a driver this build leaves out: terrio was built without `feature`.
    #[allow(dead_code)]
    pub(crate) fn disabled(feature: &str) -> Error {
        Error::Unsupported(format!("built without the `{feature}` feature"))
    }
}

impl From<wbprojection::ProjectionError> for Error {
    fn from(e: wbprojection::ProjectionError) -> Self {
        Error::Crs(e.to_string())
    }
}

impl From<terrio_crs::Error> for Error {
    fn from(error: terrio_crs::Error) -> Self {
        match error {
            terrio_crs::Error::Crs(message) | terrio_crs::Error::MissingGrid(message) | terrio_crs::Error::NoOperation(message) => {
                Self::Crs(message)
            }
            terrio_crs::Error::InvalidArgument(message) => Self::InvalidArgument(message),
            terrio_crs::Error::Io(error) => Self::Io(error),
            other => Self::Crs(other.to_string()),
        }
    }
}

impl From<zip::result::ZipError> for Error {
    fn from(e: zip::result::ZipError) -> Self {
        Error::InvalidData(format!("zip: {e}"))
    }
}

/// A result with a terrio [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
