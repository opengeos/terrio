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
    /// A dataset was found but its contents are not valid.
    #[error("{0}")]
    InvalidData(String),
}

impl From<wbprojection::ProjectionError> for Error {
    fn from(e: wbprojection::ProjectionError) -> Self {
        Error::Crs(e.to_string())
    }
}

impl From<zip::result::ZipError> for Error {
    fn from(e: zip::result::ZipError) -> Self {
        Error::InvalidData(format!("zip: {e}"))
    }
}

/// A result with a terrio [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
