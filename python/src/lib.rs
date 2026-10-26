//! Python bindings for terrio: the `terrio._terrio` extension module, wrapped by the `terrio`
//! Python package.

mod arrow;
#[cfg(not(target_os = "emscripten"))]
mod cli;
mod focal;
mod multidim;
mod ops;
mod pointcloud;
mod raster;
mod rio;
mod stream;
mod terrain;
mod vector;
mod vector_geom;

use pyo3::prelude::*;
use pyo3::types::PyType;

/// Converts a terrio error into the matching exception of `terrio.errors`.
pub(crate) fn to_py(e: terrio::Error) -> PyErr {
    let class = match &e {
        terrio::Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound => "NotFoundError",
        terrio::Error::Io(_) | terrio::Error::Http(_) => "TerrioIOError",
        terrio::Error::Crs(_) => "CRSError",
        terrio::Error::InvalidArgument(_) => "InvalidArgumentError",
        terrio::Error::Unsupported(_) => "UnsupportedError",
        terrio::Error::Raster(_) | terrio::Error::Vector(_) | terrio::Error::InvalidData(_) => "InvalidDataError",
        _ => "TerrioError",
    };
    let message = e.to_string();
    Python::attach(|py| match py.import("terrio.errors").and_then(|m| m.getattr(class)).and_then(|t| Ok(t.cast_into::<PyType>()?)) {
        Ok(t) => PyErr::from_type(t, message),
        Err(err) => err,
    })
}

/// Converts terrio results into Python results.
pub(crate) trait PyResultExt<T> {
    /// The result, with a terrio error turned into a Python exception.
    fn py(self) -> PyResult<T>;
}

impl<T> PyResultExt<T> for terrio::Result<T> {
    fn py(self) -> PyResult<T> {
        self.map_err(to_py)
    }
}

/// An `InvalidArgumentError` with a message.
pub(crate) fn invalid(message: impl Into<String>) -> PyErr {
    to_py(terrio::Error::InvalidArgument(message.into()))
}

/// Runs `f` without the GIL and issues the warnings it records (see `terrio::warnings`) as Python
/// `UserWarning`s, whether it succeeds or fails. Warnings left by earlier calls are dropped first,
/// so none is attributed to this call; `f`'s own error is preferred to a warning turned into one.
pub(crate) fn with_warnings<T: Send>(py: Python<'_>, f: impl FnOnce() -> terrio::Result<T> + Send) -> PyResult<T> {
    let _ = terrio::warnings::take();
    let result = py.detach(f).py();
    let mut warned = Ok(());
    for w in terrio::warnings::take() {
        let message = std::ffi::CString::new(w.replace('\0', " ")).expect("no NUL bytes left");
        warned = warned.and(PyErr::warn(py, &py.get_type::<pyo3::exceptions::PyUserWarning>(), &message, 2));
    }
    let value = result?;
    warned.map(|()| value)
}

/// A CRS as Python sees it: `EPSG:xxxx` (or `ESRI:xxxx`) when the code is known, otherwise the
/// WKT, or `None`.
pub(crate) fn crs_string(crs: &terrio::crs::SpatialRef) -> Option<String> {
    crs.authority_code().or_else(|| crs.to_wkt())
}

#[pymodule]
fn _terrio(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", terrio::VERSION)?;
    m.add_class::<raster::PyRaster>()?;
    m.add_class::<raster::PyGeoTiffWriter>()?;
    m.add_class::<vector::PyLayer>()?;
    m.add_class::<vector::PyVectorDataset>()?;
    m.add_class::<arrow::PyArrowTable>()?;
    m.add_function(wrap_pyfunction!(arrow::from_arrow, m)?)?;
    ops::register(m)?;
    multidim::register(m)?;
    terrain::register(m)?;
    focal::register(m)?;
    pointcloud::register(m)?;
    rio::register(m)?;
    stream::register(m)?;
    vector_geom::register(m)?;
    #[cfg(not(target_os = "emscripten"))]
    cli::register(m)?;
    Ok(())
}
