//! Thin GeoPackage repacking bindings.
use pyo3::prelude::*;
use pyo3::types::PyBytes;
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(gpkg_repack, m)?)?;
    m.add_function(wrap_pyfunction!(gpkg_repack_bytes, m)?)?;
    m.add_function(wrap_pyfunction!(validate_gpkg, m)?)
}
/// Repack a native or /vsimem/ GeoPackage in place, returning physical storage counts.
#[pyfunction]
fn gpkg_repack<'py>(py: Python<'py>, path: std::path::PathBuf) -> PyResult<Bound<'py, PyAny>> {
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

/// Validate a closed/checkpointed native or /vsimem/ path, or uploaded bytes.
/// Returns valid/complete flags, version, requirement errors, warnings and unsupported_checks.
#[pyfunction]
#[pyo3(signature=(path,*,full_check=false,strict=false,unicode_case=false))]
fn validate_gpkg<'py>(
    py: Python<'py>,
    path: &Bound<'py, PyAny>,
    full_check: bool,
    strict: bool,
    unicode_case: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let options = terrio::GpkgValidationOptions { full_check, strict, unicode_case };
    let report = if let Ok(bytes) = path.cast::<PyBytes>() {
        let data = bytes.as_bytes();
        crate::with_warnings(py, || terrio::validate_gpkg_bytes(data, &options))?
    } else {
        let path: std::path::PathBuf = path.extract()?;
        crate::with_warnings(py, || terrio::validate_gpkg(&path, &options))?
    };
    let text = serde_json::to_string(&report).map_err(|e| crate::invalid(e.to_string()))?;
    py.import("json")?.call_method1("loads", (text,))
}
