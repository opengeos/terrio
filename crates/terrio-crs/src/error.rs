//! Errors returned by the standalone CRS engine.

/// A CRS definition, option or I/O failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A coordinate reference system could not be parsed or used.
    #[error("coordinate reference system: {0}")]
    Crs(String),
    /// An invalid coordinate-operation option.
    #[error("{0}")]
    InvalidArgument(String),
    /// A file-system or grid transport error.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl From<wbprojection::ProjectionError> for Error {
    fn from(error: wbprojection::ProjectionError) -> Self {
        Self::Crs(error.to_string())
    }
}

/// A result returned by the standalone CRS engine.
pub type Result<T> = std::result::Result<T, Error>;
