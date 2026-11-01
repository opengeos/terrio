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

    /// This error, naming `path` when it is an I/O error that `path` itself causes: a local file
    /// that does not exist (an error of kind `NotFound`) or a directory (an I/O error of any kind),
    /// which the operating system's messages ("No such file or directory (os error 2)", "Is a
    /// directory (os error 21)") do not name. Other errors are returned as they are: those of
    /// another file a dataset refers to (a VRT's source), those whose message names `path` already,
    /// and those of archive members and remote datasets, whose URL may hold a signature. The
    /// variant and the I/O error's kind, which picks the Python exception, are kept; the OS error
    /// code is not (`raw_os_error` gives `None` for an error named here), as an I/O error with a
    /// message of its own has none.
    pub(crate) fn naming_local_file(self, path: &str) -> Error {
        let Error::Io(e) = self else { return self };
        let caused_by_path = match crate::vfs::Location::parse(path) {
            crate::vfs::Location::Local(file) if e.kind() == std::io::ErrorKind::NotFound => !file.exists(),
            crate::vfs::Location::Local(file) => file.is_dir(),
            _ => false,
        };
        // Named already: in quotes, as here and in other messages of terrio.
        if !caused_by_path || e.to_string().contains(&format!("'{path}'")) {
            return Error::Io(e);
        }
        Error::Io(std::io::Error::new(e.kind(), format!("'{path}': {e}")))
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

#[cfg(test)]
mod tests {
    use super::Error;
    use std::io::ErrorKind;

    /// An I/O error about an existing file that is not a directory (it cannot be read), or about a
    /// URL, which may hold a signature, keeps its message. The errors named, and those named
    /// already, are tested through `open_raster` and `validate_cog`
    /// (`dataset::tests::local_open_errors_name_the_path`).
    #[test]
    fn io_errors_the_path_does_not_cause_keep_their_message() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.tif");
        std::fs::write(&file, b"x").unwrap();
        let url = "https://example.com/missing.tif?sig=secret";
        for (path, kind) in [(file.to_str().unwrap(), ErrorKind::PermissionDenied), (url, ErrorKind::NotFound)] {
            let error = Error::Io(std::io::Error::new(kind, "the operating system's message")).naming_local_file(path);
            assert!(
                matches!(&error, Error::Io(e) if e.kind() == kind && e.to_string() == "the operating system's message"),
                "{path}: {error:?}"
            );
        }
    }

    /// A relative path that is also a word of the operating system's message ("No" in "No such
    /// file or directory") is named: only the path in quotes counts as named already.
    #[test]
    fn a_path_that_is_a_word_of_the_message_is_named() {
        let error = Error::Io(std::io::Error::from_raw_os_error(2)).naming_local_file("No");
        assert!(matches!(&error, Error::Io(e) if e.kind() == ErrorKind::NotFound && e.to_string().starts_with("'No': ")), "{error:?}");
    }
}
