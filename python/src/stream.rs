//! Vector layers read as streams: `open_stream` and the `FeatureStream` class.

use crate::Detach;
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
    columns: Option<Vec<String>>,
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
///     columns: Only these fields, by name (the geometry is always read). A struct column of a
///         GeoParquet file is named as a whole: its members, `name.member` in the chunks.
///
/// Returns:
///     FeatureStream: The stream.
///
/// Raises:
///     InvalidArgumentError: If the dataset has no such layer.
#[pyfunction]
#[pyo3(signature = (path, layer=None, *, batch_size=65536, bbox=None, columns=None))]
fn open_stream(
    py: Python<'_>,
    path: &str,
    layer: Option<&str>,
    batch_size: usize,
    bbox: Option<(f64, f64, f64, f64)>,
    columns: Option<Vec<String>>,
) -> PyResult<PyFeatureStream> {
    if batch_size == 0 {
        return Err(invalid("batch_size must be positive"));
    }
    let options = VectorOpenOptions { spatial_filter: bbox, ..Default::default() };
    let reader = py
        .detach_result(|| {
            let reader = pick(terrio::stream::open_layers(path, &options, batch_size)?, layer)?;
            match &columns {
                Some(columns) => reader.select_fields(columns),
                None => Ok(reader),
            }
        })
        .py()?;
    let layer = reader.layer().clone();
    Ok(PyFeatureStream { reader: Mutex::new(Some(reader)), layer, path: path.to_string(), options, columns, batch_size, started: false })
}

/// Whether a stream's path names a GeoParquet file, which its Arrow export reads natively.
fn is_geoparquet(path: &str) -> bool {
    let name = terrio::vfs::Location::parse(path).file_name();
    terrio::formats::VectorDriver::for_extension(&name) == Some(terrio::formats::VectorDriver::Parquet)
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
        let chunk = py.detach_result(|| reader.read_chunk(n)).py()?;
        Ok(chunk.map(PyLayer::from))
    }

    /// Reads the remaining features into a layer.
    ///
    /// Returns:
    ///     Layer: The features not yet read.
    fn read_all(&mut self, py: Python<'_>) -> PyResult<PyLayer> {
        let reader = self.take()?;
        self.started = true;
        Ok(PyLayer::from(py.detach_result(|| reader.read_all()).py()?))
    }

    fn __repr__(&self) -> String {
        format!("<terrio.FeatureStream {:?} ({} fields, batches of {})>", self.layer.name, self.layer.schema.len(), self.batch_size)
    }

    /// Exports the stream as an Arrow C stream of record batches, with WKB geometries (the
    /// Arrow PyCapsule interface), read as the consumer asks for them. The stream is consumed.
    ///
    /// A GeoParquet file hands out the record batches it holds, as GDAL's Arrow stream does:
    /// the columns keep their Arrow types (structs and lists included), the geometry columns are
    /// tagged as GeoArrow extension types with their CRS, and the bounding-box covering column is
    /// left out. Its row groups are decoded in parallel, a window at a time; a URL is read by
    /// range requests. Other formats give the schema `Layer.to_arrow()` gives; a layer with date
    /// fields is read twice, once to choose their Arrow types.
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
        if is_geoparquet(&self.path) {
            drop(reader);
            let options = terrio::arrow::GeoParquetReadOptions {
                columns: self.columns.clone(),
                bbox: self.options.spatial_filter,
                batch_size: self.batch_size,
            };
            let batches = py.detach_result(|| terrio::arrow::open_geoparquet(&self.path, &options)).py()?;
            let ffi = FFI_ArrowArrayStream::new(Box::new(batches));
            return PyCapsule::new_with_value(py, ffi, c"arrow_array_stream");
        }
        let arrow = terrio::arrow::ArrowWriteOptions { batch_size: self.batch_size, ..Default::default() };
        let (path, options, name, columns) = (self.path.clone(), self.options.clone(), self.layer.name.clone(), self.columns.clone());
        let stream = py
            .detach_result(|| -> terrio::Result<ArrowStream> {
                let profile = if terrio::arrow::ArrowEncoder::needs_profile(reader.layer(), &arrow) {
                    let mut first = pick(terrio::stream::open_layers(&path, &options, arrow.batch_size)?, Some(&name))?;
                    if let Some(columns) = &columns {
                        first = first.select_fields(columns)?;
                    }
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
