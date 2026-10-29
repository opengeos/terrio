//! Dataset file-set operations; all mutation and safety checks live in the core crate.
use crate::invalid;
use pyo3::prelude::*;
use terrio::dataset_files::{self as files, FileOptions};

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(dataset_files, m)?)?;
    m.add_function(wrap_pyfunction!(dataset_copy, m)?)?;
    m.add_function(wrap_pyfunction!(dataset_rename, m)?)?;
    m.add_function(wrap_pyfunction!(dataset_delete, m)?)?;
    Ok(())
}
fn path(value: &Bound<'_, PyAny>) -> PyResult<String> {
    value
        .extract::<std::path::PathBuf>()
        .map_err(|_| invalid("dataset source and destination must be paths"))?
        .into_os_string()
        .into_string()
        .map_err(|_| invalid("dataset paths must be UTF-8"))
}
fn json<'py>(py: Python<'py>, value: &impl serde::Serialize) -> PyResult<Bound<'py, PyAny>> {
    let text = serde_json::to_string(value).map_err(|e| invalid(e.to_string()))?;
    py.import("json")?.call_method1("loads", (text,))
}
/// Returns the driver, owned files and directory flag. VRT sources are referenced, not owned.
#[pyfunction]
#[pyo3(signature = (source, *, format=None))]
fn dataset_files<'py>(py: Python<'py>, source: &Bound<'_, PyAny>, format: Option<&str>) -> PyResult<Bound<'py, PyAny>> {
    let source = path(source)?;
    let report = crate::with_warnings(py, || files::file_list(&source, format))?;
    json(py, &report)
}
/// Copies encoded dataset files and companions without converting their format.
#[pyfunction]
#[pyo3(signature = (source, destination, *, format=None, overwrite=false, allow_external_references=false))]
fn dataset_copy<'py>(
    py: Python<'py>,
    source: &Bound<'_, PyAny>,
    destination: &Bound<'_, PyAny>,
    format: Option<String>,
    overwrite: bool,
    allow_external_references: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let (source, destination) = (path(source)?, path(destination)?);
    let report =
        crate::with_warnings(py, || files::copy(&source, &destination, &FileOptions { format, overwrite, allow_external_references }))?;
    json(py, &report)
}
/// Renames a dataset and its companion files, guarding source aliases and destination collisions.
#[pyfunction]
#[pyo3(signature = (source, destination, *, format=None, overwrite=false, allow_external_references=false))]
fn dataset_rename<'py>(
    py: Python<'py>,
    source: &Bound<'_, PyAny>,
    destination: &Bound<'_, PyAny>,
    format: Option<String>,
    overwrite: bool,
    allow_external_references: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let (source, destination) = (path(source)?, path(destination)?);
    let report =
        crate::with_warnings(py, || files::rename(&source, &destination, &FileOptions { format, overwrite, allow_external_references }))?;
    json(py, &report)
}
/// Deletes owned files of one path or an iterable of paths, returning a list of reports.
/// All inputs are inventoried before mutation; VRT/catalog source assets are retained.
#[pyfunction]
#[pyo3(signature = (sources, *, format=None))]
fn dataset_delete<'py>(py: Python<'py>, sources: &Bound<'_, PyAny>, format: Option<&str>) -> PyResult<Bound<'py, PyAny>> {
    let sources = if let Ok(path) = sources.extract::<std::path::PathBuf>() {
        vec![path.into_os_string().into_string().map_err(|_| invalid("dataset paths must be UTF-8"))?]
    } else {
        sources
            .try_iter()
            .map_err(|_| invalid("sources must be a path or an iterable of paths"))?
            .map(|source| path(&source?))
            .collect::<PyResult<Vec<_>>>()?
    };
    let reports = crate::with_warnings(py, || files::delete_many(&sources, format))?;
    json(py, &reports)
}
