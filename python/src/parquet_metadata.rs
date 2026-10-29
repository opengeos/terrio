//! Thin Python metadata aggregation binding.
use pyo3::prelude::*;

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(parquet_create_metadata, m)?)
}
/// Create a metadata-only Parquet file from explicit partition paths.
#[pyfunction]
#[pyo3(signature = (inputs, output, *, overwrite=false))]
fn parquet_create_metadata<'py>(
    py: Python<'py>,
    inputs: Vec<std::path::PathBuf>,
    output: std::path::PathBuf,
    overwrite: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let path = |p: std::path::PathBuf| p.into_os_string().into_string().map_err(|_| crate::invalid("Parquet paths must be UTF-8"));
    let inputs = inputs.into_iter().map(path).collect::<PyResult<Vec<_>>>()?;
    let output = path(output)?;
    let report = crate::with_warnings(py, || {
        terrio::parquet_metadata::create(&inputs, &output, &terrio::parquet_metadata::MetadataOptions { overwrite })
    })?;
    let text = serde_json::to_string(&report).map_err(|e| crate::invalid(e.to_string()))?;
    py.import("json")?.call_method1("loads", (text,))
}
