//! Errors returned by the standalone CRS engine.

/// A CRS definition, option or I/O failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A coordinate reference system could not be parsed or used.
    #[error("coordinate reference system: {0}")]
    Crs(String),
    /// The best coordinate operation is unavailable for this point, with
    /// [`crate::TransformOptions::only_best`] (GDAL's `ONLY_BEST=YES`). This includes missing
    /// grids and points outside an operation's coverage; inspect the message for the reason. Also
    /// a missing velocity grid of a point motion between coordinate epochs, whatever the options.
    #[error("coordinate reference system: {0}")]
    MissingGrid(String),
    /// No coordinate operation meets the options: a ballpark one would be needed, with
    /// [`crate::TransformOptions::allow_ballpark`] off (GDAL's `ALLOW_BALLPARK=NO`).
    #[error("coordinate reference system: {0}")]
    NoOperation(String),
    /// An invalid coordinate-operation option.
    #[error("{0}")]
    InvalidArgument(String),
    /// A file-system or grid transport error.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

#[cfg(feature = "wbprojection-fallback")]
impl From<wbprojection::ProjectionError> for Error {
    fn from(error: wbprojection::ProjectionError) -> Self {
        Self::Crs(error.to_string())
    }
}

/// A result returned by the standalone CRS engine.
pub type Result<T> = std::result::Result<T, Error>;
