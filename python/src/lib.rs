//! Python bindings for terrio: the `terrio._terrio` extension module, wrapped by the `terrio`
//! Python package.

mod analysis;
mod arrow;
#[cfg(not(target_os = "emscripten"))]
mod cli;
mod cog_writer;
mod config;
mod dataset_files;
mod focal;
mod gpkg_maintenance;
mod grid;
mod inspection;
mod manage;
mod multidim;
mod ops;
mod palette;
mod parquet_metadata;
mod pipeline;
mod pixel;
mod pointcloud;
mod pyogrio;
mod query;
mod raster;
mod raster_tile;
mod rio;
mod stream;
mod terrain;
mod tile_index;
mod vector;
mod vector_dataset;
mod vector_geom;
mod vsi;

use pyo3::prelude::*;
use pyo3::types::PyType;

/// Converts a terrio error into the matching exception of `terrio.errors`.
pub(crate) fn to_py(e: terrio::Error) -> PyErr {
    let class = match &e {
        terrio::Error::Io(io) if io.get_ref().is_some_and(|inner| inner.is::<Panicked>()) => "InternalError",
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

/// A Rust panic caught before it reached Python (see [`Detach`]).
#[derive(Debug)]
struct Panicked(String);

impl std::fmt::Display for Panicked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rust panic in terrio: {}", self.0)
    }
}

impl std::error::Error for Panicked {}

/// The terrio error of a caught panic, `InternalError` in Python ([`to_py`]).
fn panicked(payload: Box<dyn std::any::Any + Send>) -> terrio::Error {
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".to_string());
    terrio::Error::Io(std::io::Error::other(Panicked(message)))
}

/// `Python::detach`, without letting a Rust panic reach Python as PyO3's `PanicException`, a
/// `BaseException` that `except Exception` does not catch: the panic becomes
/// `terrio.errors.InternalError`, an `Exception` whose message holds "panic" and the panic's
/// text (in `terrio.rio`, the cause of a `RasterioIOError`). The process stays usable, as it did
/// after PyO3's own catch (measured): only the exception's class changes. Every binding releases
/// the GIL through this trait (a test checks the sources).
pub(crate) trait Detach {
    /// For work returning a terrio result: a panic becomes its error.
    fn detach_result<T: Send, F: Send + FnOnce() -> terrio::Result<T>>(self, f: F) -> terrio::Result<T>;
    /// For other work: a panic becomes an `InternalError`.
    fn detach_value<T: Send, F: Send + FnOnce() -> T>(self, f: F) -> PyResult<T>;
}

impl Detach for Python<'_> {
    fn detach_result<T: Send, F: Send + FnOnce() -> terrio::Result<T>>(self, f: F) -> terrio::Result<T> {
        let result = self.detach(move || std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))).unwrap_or_else(|p| Err(panicked(p)));
        pass_on_http_retries(self);
        pass_on_nodata_warnings(self);
        result
    }

    fn detach_value<T: Send, F: Send + FnOnce() -> T>(self, f: F) -> PyResult<T> {
        let result = self.detach(move || std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))).map_err(|p| to_py(panicked(p)));
        pass_on_http_retries(self);
        pass_on_nodata_warnings(self);
        result
    }
}

/// The callable HTTP retry warnings are passed to (see [`_set_http_retry_logger`]).
static HTTP_RETRY_LOGGER: std::sync::Mutex<Option<Py<PyAny>>> = std::sync::Mutex::new(None);

/// Sets the callable that gets, after each call that released the GIL, one warning (a `str`) per
/// HTTP request retried meanwhile, worded as GDAL's (`HTTP error code: 0 - URL. Retrying again in
/// 0.5 secs`, see `terrio::http::record_retry_warnings`); `None` stops recording them.
/// `terrio.compat` logs them as rasterio logs GDAL's.
#[pyfunction]
fn _set_http_retry_logger(callback: Option<Py<PyAny>>) {
    let on = callback.is_some();
    // The previous callable is released last, after the lock (releasing it may run Python code).
    let _previous = std::mem::replace(&mut *HTTP_RETRY_LOGGER.lock().unwrap_or_else(std::sync::PoisonError::into_inner), callback);
    terrio::http::record_retry_warnings(on);
}

/// Passes the HTTP retry warnings recorded so far to the callable [`_set_http_retry_logger`] set,
/// called without the lock held; its errors are ignored, as rasterio's GDAL error handler cannot
/// raise them either.
fn pass_on_http_retries(py: Python<'_>) {
    pass_on(py, terrio::http::take_retry_warnings(), &HTTP_RETRY_LOGGER);
}

/// The callable the warper's nodata warnings are passed to (see [`_set_warp_nodata_logger`]).
static WARP_NODATA_LOGGER: std::sync::Mutex<Option<Py<PyAny>>> = std::sync::Mutex::new(None);

/// Sets the callable that gets, after each call that released the GIL, the warning (a `str`)
/// GDAL's warper gives for each kernel run that moved warped values off the output nodata value
/// meanwhile, worded as GDAL's (`Value 255 in the source dataset has been changed to 254 in the
/// destination dataset to avoid being treated as NoData. ...`, see
/// `terrio::ops::warp::record_nodata_warnings`); `None` stops recording them. `terrio.compat` logs
/// them as rasterio logs GDAL's.
#[pyfunction]
fn _set_warp_nodata_logger(callback: Option<Py<PyAny>>) {
    let on = callback.is_some();
    // The previous callable is released last, after the lock (releasing it may run Python code).
    let _previous = std::mem::replace(&mut *WARP_NODATA_LOGGER.lock().unwrap_or_else(std::sync::PoisonError::into_inner), callback);
    terrio::ops::warp::record_nodata_warnings(on);
}

/// Passes the warper's nodata warnings recorded so far to the callable
/// [`_set_warp_nodata_logger`] set, as [`pass_on_http_retries`] does.
fn pass_on_nodata_warnings(py: Python<'_>) {
    pass_on(py, terrio::ops::warp::take_nodata_warnings(), &WARP_NODATA_LOGGER);
}

/// Passes `warnings` to the callable in `logger`, if any, one call each, without the lock held.
fn pass_on(py: Python<'_>, warnings: Vec<String>, logger: &std::sync::Mutex<Option<Py<PyAny>>>) {
    if warnings.is_empty() {
        return;
    }
    let logger = logger.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().map(|c| c.clone_ref(py));
    if let Some(logger) = logger {
        for warning in warnings {
            let _ = logger.bind(py).call1((warning,));
        }
    }
}

/// The bytes of memory the process can use, as GDAL counts them (`CPLGetUsablePhysicalRAM`: the
/// physical memory, less under a cgroup's memory limit or an address-space limit), which reads
/// refuse windows past; `None` where it cannot be read.
#[pyfunction]
fn usable_memory() -> Option<u64> {
    terrio::usable_memory()
}

/// Panics on purpose, for the tests of [`Detach`]: `terrio.errors.InternalError`.
#[pyfunction]
fn _panic_for_tests(py: Python<'_>, message: String) -> PyResult<()> {
    py.detach_result(move || -> terrio::Result<()> { panic!("{message}") }).py()
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
    let result = py.detach_result(f).py();
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
    m.add_class::<cog_writer::PyCogWriter>()?;
    m.add_class::<vector::PyLayer>()?;
    m.add_class::<vector::PyVectorDataset>()?;
    m.add_class::<arrow::PyArrowTable>()?;
    m.add_function(wrap_pyfunction!(arrow::from_arrow, m)?)?;
    m.add_function(wrap_pyfunction!(_panic_for_tests, m)?)?;
    m.add_function(wrap_pyfunction!(usable_memory, m)?)?;
    m.add_function(wrap_pyfunction!(_set_http_retry_logger, m)?)?;
    m.add_function(wrap_pyfunction!(_set_warp_nodata_logger, m)?)?;
    ops::register(m)?;
    analysis::register(m)?;
    config::register(m)?;
    manage::register(m)?;
    dataset_files::register(m)?;
    parquet_metadata::register(m)?;
    inspection::register(m)?;
    grid::register(m)?;
    gpkg_maintenance::register(m)?;
    multidim::register(m)?;
    terrain::register(m)?;
    tile_index::register(m)?;
    raster_tile::register(m)?;
    query::register(m)?;
    pixel::register(m)?;
    pipeline::register(m)?;
    palette::register(m)?;
    focal::register(m)?;
    pointcloud::register(m)?;
    pyogrio::register(m)?;
    rio::register(m)?;
    stream::register(m)?;
    vector_geom::register(m)?;
    vsi::register(m)?;
    vector_dataset::register(m)?;
    #[cfg(not(target_os = "emscripten"))]
    cli::register(m)?;
    Ok(())
}
