//! Thin GeoPackage repacking bindings.
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(gpkg_repack, m)?)?;
    m.add_function(wrap_pyfunction!(gpkg_repack_bytes, m)?)
}
/// Repack a native or /vsimem/ GeoPackage in place, returning physical storage counts.
#[pyfunction]
fn gpkg_repack<'py>(py: Python<'py>, path: std::path::PathBuf) -> PyResult<Bound<'py, PyAny>> {
    let report = crate::with_warnings(py, || terrio::gpkg_maintenance::repack(&path))?;
    let result = PyDict::new(py);
    result.set_item("bytes_before", report.bytes_before)?;
    result.set_item("bytes_after", report.bytes_after)?;
    result.set_item("page_size", report.page_size)?;
    result.set_item("pages_before", report.pages_before)?;
    result.set_item("pages_after", report.pages_after)?;
    Ok(result.into_any())
}
/// Return a compacted copy of a self-contained GeoPackage image.
#[pyfunction]
fn gpkg_repack_bytes<'py>(py: Python<'py>, data: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
    let output = crate::with_warnings(py, || terrio::gpkg_maintenance::repack_bytes(data))?;
    Ok(PyBytes::new(py, &output))
}
