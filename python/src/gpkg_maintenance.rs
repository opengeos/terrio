//! Thin GeoPackage repacking bindings.
use pyo3::prelude::*;
use pyo3::types::PyBytes;
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(gpkg_repack, m)?)?;
    m.add_function(wrap_pyfunction!(gpkg_repack_bytes, m)?)
}
/// Repack a native or /vsimem/ GeoPackage in place, returning physical storage counts.
#[pyfunction]
fn gpkg_repack<'py>(py: Python<'py>, path: std::path::PathBuf) -> PyResult<Bound<'py, PyAny>> {
    let path = path.into_os_string().into_string().map_err(|_| crate::invalid("GeoPackage paths must be UTF-8"))?;
    let report = crate::with_warnings(py, || terrio::gpkg_maintenance::repack(&path))?;
    let text = serde_json::to_string(&report).map_err(|e| crate::invalid(e.to_string()))?;
    py.import("json")?.call_method1("loads", (text,))
}
/// Return a compacted copy of a self-contained GeoPackage image.
#[pyfunction]
fn gpkg_repack_bytes<'py>(py: Python<'py>, data: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
    let output = crate::with_warnings(py, || terrio::gpkg_maintenance::repack_bytes(data))?;
    Ok(PyBytes::new(py, &output))
}
