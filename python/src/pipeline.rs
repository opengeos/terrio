//! Thin bindings for the shared linear pipeline engine.
use crate::raster::PyRaster;
use crate::vector::PyVectorDataset;
use crate::{Detach, PyResultExt, invalid};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use std::collections::BTreeMap;
use terrio::ops::pipeline::{Pipeline, PipelineData, PipelineInput, PipelineResult, ReadRequest};

/// The final dataset and deferred named outputs of a pipeline.
#[pyclass(module = "terrio", name = "PipelineResult")]
pub struct PyPipelineResult {
    inner: PipelineResult,
}
#[pymethods]
impl PyPipelineResult {
    /// Final Raster or VectorDataset, or None when saving a definition.
    #[getter]
    fn dataset(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match self.inner.data.as_mut() {
            None => Ok(py.None()),
            Some(PipelineData::Raster(r)) => Ok(Py::new(py, PyRaster { inner: r.shared() })?.into_any()),
            Some(PipelineData::Vector(layers)) => Ok(Py::new(
                py,
                PyVectorDataset::new(py, terrio::VectorDataset { layers: layers.clone(), driver: terrio::formats::VectorDriver::GeoJSON })?,
            )?
            .into_any()),
        }
    }
    /// Destinations requested by write or named materialize steps.
    #[getter]
    fn output_names(&self) -> Vec<String> {
        self.inner.output_names()
    }
    /// Encode requested outputs without creating files.
    fn output_bytes<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let outputs = py.detach_result(|| self.inner.output_bytes()).py()?;
        let result = PyDict::new(py);
        for (name, bytes) in outputs {
            result.set_item(name, PyBytes::new(py, &bytes))?;
        }
        Ok(result)
    }
    /// Write all requested destinations after checking overwrite policy and aliases.
    fn write_outputs(&mut self, py: Python<'_>) -> PyResult<()> {
        py.detach_result(|| self.inner.write_outputs()).py()
    }
}

/// Evaluate a command or GDALG JSON text. Sources may be native paths or aliases
/// in inputs (Raster, vector objects or paths). Outputs are deferred unless write=True.
#[pyfunction]
#[pyo3(signature = (command, inputs=None, *, write=false, base_file=None, input_kind=None))]
fn pipeline(
    py: Python<'_>,
    command: &str,
    inputs: Option<&Bound<'_, PyDict>>,
    write: bool,
    base_file: Option<&str>,
    input_kind: Option<&str>,
) -> PyResult<PyPipelineResult> {
    let mut plan =
        if command.trim_start().starts_with('{') { Pipeline::from_gdalg(command, base_file) } else { Pipeline::parse(command) }.py()?;
    if let Some(kind) = input_kind {
        plan = plan.with_kind(kind.parse().py()?);
    }
    let mut sources = BTreeMap::new();
    if let Some(inputs) = inputs {
        for (key, value) in inputs {
            let name = key.extract::<String>()?;
            sources.insert(name, value.unbind());
        }
    }
    let mut input_error = None;
    let result = py.detach_result(|| {
        // Reference resolution joins with `/`, so a Windows alias keyed by a native path needs a separator-blind match.
        fn windows_alias<'a, V>(sources: &'a BTreeMap<String, V>, resolved: &str) -> Option<&'a V> {
            if !cfg!(windows) {
                return None;
            }
            let normalize = |path: &str| path.replace('\\', "/");
            let wanted = normalize(resolved);
            sources.iter().find(|(name, _)| normalize(name) == wanted).map(|(_, value)| value)
        }
        plan.evaluate_with(|request| {
            let key = if sources.contains_key(&request.source) { &request.source } else { &request.resolved };
            let Some(value) = sources.get(key).or_else(|| windows_alias(&sources, &request.resolved)) else {
                return PipelineInput::open(request);
            };
            Python::attach(|py| -> PyResult<PipelineInput> {
                let value = value.bind(py);
                if let Ok(raster) = value.cast::<PyRaster>() {
                    Ok(PipelineData::Raster(raster.try_borrow_mut()?.inner.shared()).into())
                } else if let Ok(path) = value.extract::<std::path::PathBuf>() {
                    let path = path.into_os_string().into_string().map_err(|_| invalid("input paths must be UTF-8"))?;
                    py.detach_result(|| PipelineInput::open(&ReadRequest { source: path.clone(), resolved: path, kind: request.kind })).py()
                } else {
                    Ok(PipelineData::Vector(crate::ops::input_layers(py, value, None)?).into())
                }
            })
            .map_err(|error| {
                input_error = Some(error);
                terrio::Error::InvalidArgument("pipeline input conversion failed".into())
            })
        })
    });
    if let Some(error) = input_error {
        return Err(error);
    }
    let mut inner = result.py()?;
    if write {
        py.detach_result(|| inner.write_outputs()).py()?;
    }
    Ok(PyPipelineResult { inner })
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyPipelineResult>()?;
    m.add_function(wrap_pyfunction!(pipeline, m)?)?;
    Ok(())
}
