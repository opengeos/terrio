//! Thin bindings for dataset header identification.
use crate::invalid;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
#[cfg(not(target_family = "wasm"))]
use terrio::inspection::IdentifyOptions;
use terrio::inspection::IdentifyRecord;

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(identify, module)?)?;
    Ok(())
}
/// Identifies files from bounded headers without decoding pixel/feature payloads.
/// Accepts one path, a sequence of paths, or uploaded bytes with a filename hint.
/// Recursive scans apply to local directories and stop at recognized datasets;
/// force_recursive also descends inside those datasets. deep permits existing
/// readers as a fallback and can materialize formats without lazy metadata.
/// report_failures includes unknown inputs and I/O failures; otherwise they are omitted.
#[pyfunction]
#[pyo3(signature=(sources, *, filename=None, recursive=false, force_recursive=false, deep=false, report_failures=false))]
#[allow(clippy::too_many_arguments)]
fn identify<'py>(
    py: Python<'py>,
    sources: &Bound<'py, PyAny>,
    filename: Option<&str>,
    recursive: bool,
    force_recursive: bool,
    deep: bool,
    report_failures: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let records = if let Ok(bytes) = sources.cast::<PyBytes>() {
        if recursive || force_recursive || deep {
            return Err(invalid("recursive/deep identification requires native paths"));
        }
        let name = filename.unwrap_or("data");
        IdentifyRecord::from_bytes(bytes.as_bytes(), name, report_failures).into_iter().collect::<Vec<_>>()
    } else {
        if filename.is_some() {
            return Err(invalid("filename applies only to uploaded bytes"));
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let utf8 = |path: std::path::PathBuf| path.into_os_string().into_string().map_err(|_| invalid("identify paths must be UTF-8"));
            let paths = if let Ok(path) = sources.extract::<std::path::PathBuf>() {
                vec![utf8(path)?]
            } else {
                let iterator = sources.try_iter().map_err(|_| invalid("sources must be a path, path sequence or bytes"))?;
                let mut paths = Vec::new();
                for item in iterator {
                    let path = item?.extract::<std::path::PathBuf>().map_err(|_| invalid("identify sources must be paths"))?;
                    paths.push(utf8(path)?);
                }
                paths
            };
            crate::with_warnings(py, || {
                Ok(terrio::inspection::identify_many(&paths, &IdentifyOptions { recursive, force_recursive, deep, report_failures }))
            })?
        }
        #[cfg(target_family = "wasm")]
        {
            return Err(invalid("this build identifies uploaded bytes only"));
        }
    };
    let json = serde_json::to_string(&records).map_err(|error| invalid(error.to_string()))?;
    py.import("json")?.call_method1("loads", (json,))
}
