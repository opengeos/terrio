//! Native scientific groups and lazy named-dimension array views.
use crate::{PyResultExt, invalid};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyString, PyTuple};
use terrio::multidim::{self, ArrayInfo, View};

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyMdDataset>()?;
    m.add_class::<PyMdArray>()?;
    m.add_function(wrap_pyfunction!(open_multidim, m)?)?;
    m.add_function(wrap_pyfunction!(mosaic_multidim, m)?)?;
    Ok(())
}
fn path(value: &Bound<'_, PyAny>) -> PyResult<String> {
    value.extract::<std::path::PathBuf>()?.into_os_string().into_string().map_err(|_| invalid("scientific dataset paths must be UTF-8"))
}
fn json<'py>(py: Python<'py>, value: &impl serde::Serialize) -> PyResult<Bound<'py, PyAny>> {
    py.import("json")?.call_method1("loads", (serde_json::to_string(value).map_err(|e| invalid(e.to_string()))?,))
}
/// A native scientific dataset containing groups, dimensions and lazy numeric arrays.
#[pyclass(module = "terrio", name = "MultidimDataset")]
pub struct PyMdDataset {
    inner: multidim::Dataset,
}
#[pymethods]
impl PyMdDataset {
    /// Reads every numeric array and coordinate; unsupported arrays fail explicitly.
    fn check<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let summary = crate::with_warnings(py, || terrio::check::check_multidim(&self.inner))?;
        json(py, &summary)
    }
    /// Complete metadata, without reading coordinate or data array payloads.
    fn info<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json(py, &self.inner)
    }
    /// Numeric array paths in group order.
    #[getter]
    fn array_paths(&self) -> Vec<String> {
        self.inner.root.arrays_recursive().iter().map(|a| a.info.path.clone()).collect()
    }
    /// Group paths, including the root.
    #[getter]
    fn group_paths(&self) -> Vec<String> {
        fn visit(g: &multidim::Group, out: &mut Vec<String>) {
            out.push(g.path.clone());
            for child in &g.groups {
                visit(child, out);
            }
        }
        let mut out = Vec::new();
        visit(&self.inner.root, &mut out);
        out
    }
    /// Return a lazy array handle by its absolute path.
    fn array(&self, name: &str) -> PyResult<PyMdArray> {
        let a = self.inner.array(name).py()?;
        Ok(PyMdArray { view: a.view(), info: a.info })
    }
    fn __getitem__(&self, name: &str) -> PyResult<PyMdArray> {
        self.array(name)
    }
    /// Write a new .nc or .zarr numeric snapshot, preserving coordinates and source chunks.
    fn write(&self, py: Python<'_>, destination: &Bound<'_, PyAny>) -> PyResult<()> {
        let destination = path(destination)?;
        py.detach(|| self.inner.write(&destination)).py()
    }
    /// Serialize a numeric snapshot as netCDF-4 bytes.
    fn to_bytes<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let bytes = py.detach(|| self.inner.to_netcdf_bytes()).py()?;
        Ok(PyBytes::new(py, &bytes))
    }
    /// Serialize a Zarr v2 snapshot as a dictionary of relative keys and bytes.
    fn to_zarr_store<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let files = py.detach(|| self.inner.to_zarr_store()).py()?;
        let out = PyDict::new(py);
        for (name, bytes) in files {
            out.set_item(name, PyBytes::new(py, &bytes))?;
        }
        Ok(out)
    }
}
/// A named numeric array with lazy indexing, slicing and transposition.
#[pyclass(module = "terrio", name = "MultidimArray")]
pub struct PyMdArray {
    view: View,
    info: ArrayInfo,
}
#[pymethods]
impl PyMdArray {
    /// Absolute source-array path.
    #[getter]
    fn path(&self) -> &str {
        &self.info.path
    }
    /// Shape after indexing and transposition.
    #[getter]
    fn shape<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        PyTuple::new(py, self.view.shape())
    }
    /// Dimension names in the current view order.
    #[getter]
    fn dimension_names(&self) -> Vec<String> {
        self.view.dimensions().iter().map(|d| d.name.clone()).collect()
    }
    /// Native NumPy element dtype name.
    #[getter]
    fn dtype(&self) -> &'static str {
        dtype(self.info.data_type)
    }
    /// Full array metadata with selected dimensions; chunks describe the source layout.
    fn info<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let mut info = self.info.clone();
        info.dimensions = self.view.dimensions();
        json(py, &info)
    }
    /// Source attributes, including scale/offset and fill values; values remain packed.
    #[getter]
    fn attributes<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json(py, &self.info.attributes)
    }
    /// Coordinate array paths. Use the parent dataset's array() to access them.
    #[getter]
    fn coordinates(&self) -> Vec<String> {
        self.info.coordinates.clone()
    }
    /// Select one index by dimension name and remove that axis, without reading.
    fn isel(&self, dimension: &str, index: usize) -> PyResult<Self> {
        Ok(Self { view: self.view.isel(dimension, index).py()?, info: self.info.clone() })
    }
    /// Select a positive-stride range relative to the current view, without reading.
    #[pyo3(signature=(dimension,start,count,step=1))]
    fn slice(&self, dimension: &str, start: usize, count: usize, step: usize) -> PyResult<Self> {
        Ok(Self { view: self.view.slice(dimension, start, count, step).py()?, info: self.info.clone() })
    }
    /// Select and remove a zero-based axis in the current view, including repeated names.
    fn isel_axis(&self, axis: usize, index: usize) -> PyResult<Self> {
        Ok(Self { view: self.view.isel_axis(axis, index).py()?, info: self.info.clone() })
    }
    /// Slice a zero-based current axis position rather than a dimension name.
    #[pyo3(signature=(axis,start,count,step=1))]
    fn slice_axis(&self, axis: usize, start: usize, count: usize, step: usize) -> PyResult<Self> {
        Ok(Self { view: self.view.slice_axis(axis, start, count, step).py()?, info: self.info.clone() })
    }
    /// Reorder all current dimensions by name; the default reverses their order.
    #[pyo3(signature=(dimensions=None))]
    fn transpose(&self, dimensions: Option<Vec<String>>) -> PyResult<Self> {
        let dims = self.view.dimensions();
        let order = match dimensions {
            Some(names) => names
                .iter()
                .map(|name| {
                    dims.iter()
                        .position(|d| d.name == *name || d.path == *name)
                        .ok_or_else(|| invalid(format!("unknown dimension '{name}'")))
                })
                .collect::<PyResult<Vec<_>>>()?,
            None => (0..dims.len()).rev().collect(),
        };
        Ok(Self { view: self.view.transpose(&order).py()?, info: self.info.clone() })
    }
    /// Read the selected hyperslab as a writable NumPy array of its native type and shape.
    fn read<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let buffer = py.detach(|| self.view.read()).py()?;
        let np = py.import("numpy")?;
        let flat = np.call_method1("frombuffer", (PyBytes::new(py, &buffer.bytes), dtype(buffer.data_type)))?.call_method0("copy")?;
        flat.call_method1("reshape", (PyTuple::new(py, buffer.shape)?,))
    }
}
fn dtype(t: terrio::DataType) -> &'static str {
    use terrio::DataType::*;
    match t {
        U8 => "u1",
        I8 => "i1",
        U16 => "<u2",
        I16 => "<i2",
        U32 => "<u4",
        I32 => "<i4",
        U64 => "<u8",
        I64 => "<i8",
        F32 => "<f4",
        F64 => "<f8",
    }
}
/// Open a native scientific path, netCDF/HDF5 bytes, or Zarr key/value dictionary.
#[pyfunction]
fn open_multidim(py: Python<'_>, src: &Bound<'_, PyAny>) -> PyResult<PyMdDataset> {
    let inner = if src.cast::<PyBytes>().is_ok() {
        let bytes: Vec<u8> = src.extract()?;
        py.detach(|| multidim::open_bytes(&bytes)).py()?
    } else if src.cast::<PyDict>().is_ok() {
        let files: std::collections::HashMap<String, Vec<u8>> = src.extract()?;
        py.detach(|| multidim::open_zarr_store(files.into_iter().collect())).py()?
    } else {
        let source = path(src)?;
        py.detach(|| multidim::open(&source)).py()?
    };
    Ok(PyMdDataset { inner })
}

/// Build a lazy coordinate-aligned mosaic from MultidimDataset objects or scientific inputs.
/// arrays selects array paths/names; None selects all arrays with at least two dimensions.
/// Last input wins overlaps. Reading a view fetches only intersecting source hyperslabs.
/// dst optionally writes a new netCDF/Zarr snapshot; existing outputs are refused.
#[pyfunction]
#[pyo3(signature=(sources, dst=None, *, arrays=None))]
fn mosaic_multidim(
    py: Python<'_>,
    sources: &Bound<'_, PyAny>,
    dst: Option<&Bound<'_, PyAny>>,
    arrays: Option<Vec<String>>,
) -> PyResult<PyMdDataset> {
    let message = "sources must be a sequence of multidimensional inputs; wrap a single input in a list";
    if sources.is_instance_of::<PyString>()
        || sources.is_instance_of::<PyBytes>()
        || sources.is_instance_of::<PyDict>()
        || sources.hasattr("__fspath__")?
        || sources.cast::<PyMdDataset>().is_ok()
    {
        return Err(invalid(message));
    }
    let mut datasets = Vec::new();
    for source in sources.try_iter().map_err(|_| invalid(message))? {
        let source = source?;
        if let Ok(dataset) = source.cast::<PyMdDataset>() {
            datasets.push(dataset.try_borrow().map_err(|e| invalid(e.to_string()))?.inner.clone());
        } else {
            datasets.push(open_multidim(py, &source)?.inner);
        }
    }
    let destination = dst.map(path).transpose()?;
    let inner = py
        .detach(|| {
            let output = multidim::mosaic(&datasets, &multidim::MosaicOptions { arrays: arrays.unwrap_or_default() })?;
            if let Some(path) = destination {
                output.write(&path)?;
            }
            Ok(output)
        })
        .py()?;
    Ok(PyMdDataset { inner })
}
