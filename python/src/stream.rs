//! Vector layers read as streams: `open_stream` and the `FeatureStream` class.

use crate::vector::PyLayer;
use crate::{PyResultExt, crs_string, invalid};
use arrow_array::ffi_stream::FFI_ArrowArrayStream;
use pyo3::prelude::*;
use pyo3::types::{PyCapsule, PyDict};
use std::sync::Mutex;
use terrio::VectorOpenOptions;
use terrio::crs::SpatialRef;
use terrio::stream::{ArrowStream, FeatureReader};

/// Registers the functions and classes in the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyFeatureStream>()?;
    m.add_function(wrap_pyfunction!(open_stream, m)?)?;
    Ok(())
}

/// A vector layer read as a stream: its features come a chunk at a time, so that layers larger
/// than memory can be read, filtered and converted.
///
/// Iterating gives `Layer` chunks of at most `batch_size` features. The stream also exports
/// Arrow record batches through the Arrow PyCapsule interface (`__arrow_c_stream__`), read as
/// the consumer asks for them: `pyarrow.RecordBatchReader.from_stream(stream)`.
#[pyclass(module = "terrio", name = "FeatureStream")]
pub struct PyFeatureStream {
    reader: Mutex<Option<FeatureReader>>,
    layer: terrio::Layer,
    path: String,
    options: VectorOpenOptions,
    batch_size: usize,
    started: bool,
}

impl PyFeatureStream {
    fn take(&self) -> PyResult<FeatureReader> {
        self.reader.lock().map_err(|_| invalid("the stream is in use"))?.take().ok_or_else(|| invalid("the stream has been consumed"))
    }
}

/// Opens a layer of the reader's dataset.
fn pick(readers: Vec<FeatureReader>, layer: Option<&str>) -> terrio::Result<FeatureReader> {
    let mut readers = readers;
    let i = match layer {
        Some(name) => readers.iter().position(|r| r.layer().name.eq_ignore_ascii_case(name)).ok_or_else(|| {
            let names: Vec<&str> = readers.iter().map(|r| r.layer().name.as_str()).collect();
            terrio::Error::InvalidArgument(format!("no layer named {name} (layers: {})", names.join(", ")))
        })?,
        None if readers.is_empty() => return Err(terrio::Error::InvalidArgument("the dataset has no vector layers".into())),
        None => 0,
    };
    Ok(readers.swap_remove(i))
}

/// Opens a vector layer as a stream of features.
///
/// GeoPackage, FlatGeobuf, GeoParquet, Shapefile, GeoJSON, GeoJSON text sequences
/// (`.geojsonl`) and CSV files are read a chunk at a time, in bounded memory (GeoJSON and CSV
/// files are read twice: first to infer the fields, as GDAL does); other formats are read whole
/// and handed out in chunks.
///
/// Args:
///     path: The dataset's path or URL.
///     layer: The layer's name. By default, the first layer.
///     batch_size: The most features in a chunk.
///     bbox: Only the features whose bounding box intersects this rectangle
///         `(min_x, min_y, max_x, max_y)`, in the layer's CRS, found through the R-tree of a
///         GeoPackage and the bounding boxes of a GeoParquet file (other formats are not
///         filtered).
///
/// Returns:
///     FeatureStream: The stream.
///
/// Raises:
///     InvalidArgumentError: If the dataset has no such layer.
#[pyfunction]
#[pyo3(signature = (path, layer=None, *, batch_size=65536, bbox=None))]
fn open_stream(
    py: Python<'_>,
    path: &str,
    layer: Option<&str>,
    batch_size: usize,
    bbox: Option<(f64, f64, f64, f64)>,
) -> PyResult<PyFeatureStream> {
    if batch_size == 0 {
        return Err(invalid("batch_size must be positive"));
    }
    let options = VectorOpenOptions { spatial_filter: bbox, ..Default::default() };
    let reader = py.detach(|| pick(terrio::stream::open_layers(path, &options, batch_size)?, layer)).py()?;
    let layer = reader.layer().clone();
    Ok(PyFeatureStream { reader: Mutex::new(Some(reader)), layer, path: path.to_string(), options, batch_size, started: false })
}

#[pymethods]
impl PyFeatureStream {
    /// The layer name.
    #[getter]
    fn name(&self) -> &str {
        &self.layer.name
    }

    /// The CRS: `EPSG:xxxx` when the code is known, otherwise WKT; `None` when unknown.
    #[getter]
    fn crs(&self) -> Option<String> {
        crs_string(&SpatialRef::from_layer(&self.layer))
    }

    /// The declared geometry type (`Point`, `MultiPolygon`...), or `Unknown`.
    #[getter]
    fn geometry_type(&self) -> String {
        self.layer.geom_type.map_or("Unknown".to_string(), |t| t.as_str().to_string())
    }

    /// The fields: names mapped to their types (`Integer`, `Real`, `String`...).
    #[getter]
    fn fields<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for f in self.layer.schema.fields() {
            d.set_item(&f.name, f.field_type.as_str())?;
        }
        Ok(d)
    }

    /// The most features in a chunk.
    #[getter]
    fn batch_size(&self) -> usize {
        self.batch_size
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<PyLayer>> {
        let mut guard = self.reader.lock().map_err(|_| invalid("the stream is in use"))?;
        let Some(reader) = guard.as_mut() else { return Ok(None) };
        self.started = true;
        let n = self.batch_size;
        let chunk = py.detach(|| reader.read_chunk(n)).py()?;
        Ok(chunk.map(|inner| PyLayer { inner }))
    }

    /// Reads the remaining features into a layer.
    ///
    /// Returns:
    ///     Layer: The features not yet read.
    fn read_all(&mut self, py: Python<'_>) -> PyResult<PyLayer> {
        let reader = self.take()?;
        self.started = true;
        Ok(PyLayer { inner: py.detach(|| reader.read_all()).py()? })
    }

    fn __repr__(&self) -> String {
        format!("<terrio.FeatureStream {:?} ({} fields, batches of {})>", self.layer.name, self.layer.schema.len(), self.batch_size)
    }

    /// Exports the stream as an Arrow C stream of record batches, with WKB geometries (the
    /// Arrow PyCapsule interface), read as the consumer asks for them. A layer with date fields
    /// is read twice, once to choose their Arrow types. The stream is consumed.
    ///
    /// Args:
    ///     requested_schema: A schema capsule the consumer would like. Ignored.
    ///
    /// Returns:
    ///     PyCapsule: An `arrow_array_stream` capsule.
    ///
    /// Raises:
    ///     InvalidArgumentError: If features have already been read from the stream.
    #[pyo3(signature = (requested_schema=None))]
    fn __arrow_c_stream__<'py>(&mut self, py: Python<'py>, requested_schema: Option<Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyCapsule>> {
        let _ = requested_schema;
        if self.started {
            return Err(invalid("features have been read from the stream: open it again to export it"));
        }
        let reader = self.take()?;
        self.started = true;
        let arrow = terrio::arrow::ArrowWriteOptions { batch_size: self.batch_size, ..Default::default() };
        let (path, options, name) = (self.path.clone(), self.options.clone(), self.layer.name.clone());
        let stream = py
            .detach(|| -> terrio::Result<ArrowStream> {
                let profile = if terrio::arrow::ArrowEncoder::needs_profile(reader.layer(), &arrow) {
                    let mut first = pick(terrio::stream::open_layers(&path, &options, arrow.batch_size)?, Some(&name))?;
                    let mut p = terrio::stream::LayerProfile::new(first.layer());
                    while let Some(c) = first.read_chunk(arrow.batch_size)? {
                        p.add(&c.features);
                    }
                    Some(p)
                } else {
                    None
                };
                ArrowStream::new(reader, profile.as_ref(), &arrow)
            })
            .py()?;
        let ffi = FFI_ArrowArrayStream::new(Box::new(stream));
        PyCapsule::new_with_value(py, ffi, c"arrow_array_stream")
    }
}
