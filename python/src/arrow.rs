//! Arrow for Python: layers exported and imported through the Arrow PyCapsule interface
//! (`__arrow_c_stream__`, `__arrow_c_array__`, `__arrow_c_schema__`), with no dependency on
//! pyarrow.

use crate::Detach;
use crate::vector::PyLayer;
use crate::{PyResultExt, invalid};
use arrow_array::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
use arrow_array::ffi_stream::{ArrowArrayStreamReader, FFI_ArrowArrayStream};
use arrow_array::{Array, RecordBatch, RecordBatchIterator, StructArray};
use pyo3::prelude::*;
use pyo3::types::{PyCapsule, PyCapsuleMethods, PyTuple};
use std::ffi::CStr;
use std::sync::Arc;
use terrio::arrow::{ArrowTable, ArrowWriteOptions, CoordinateLayout, GeometryEncoding};
use terrio::arrow::{arrow_schema::Schema, layer_from_arrow};
use terrio::crs::SpatialRef;

const STREAM: &CStr = c"arrow_array_stream";
const SCHEMA: &CStr = c"arrow_schema";
const ARRAY: &CStr = c"arrow_array";

fn arrow_err(e: impl std::fmt::Display) -> PyErr {
    crate::to_py(terrio::Error::InvalidData(format!("Arrow: {e}")))
}

/// A capsule holding an Arrow C stream of the table's batches.
fn stream_capsule<'py>(py: Python<'py>, table: &ArrowTable) -> PyResult<Bound<'py, PyCapsule>> {
    let reader = RecordBatchIterator::new(table.batches.clone().into_iter().map(Ok), table.schema.clone());
    let stream = FFI_ArrowArrayStream::new(Box::new(reader));
    PyCapsule::new_with_value(py, stream, STREAM)
}

/// A capsule holding the Arrow C schema of the table.
fn schema_capsule<'py>(py: Python<'py>, schema: &Schema) -> PyResult<Bound<'py, PyCapsule>> {
    let ffi = FFI_ArrowSchema::try_from(schema).map_err(arrow_err)?;
    PyCapsule::new_with_value(py, ffi, SCHEMA)
}

/// A layer as Arrow data: a schema and record batches, readable by any library that takes
/// the Arrow PyCapsule interface (pyarrow, GeoPandas, Polars, DuckDB, GDAL...).
///
/// Convert it with `pyarrow.table(t)`, `geopandas.GeoDataFrame.from_arrow(t)` or
/// `polars.DataFrame(t)`, or with `to_pyarrow()`.
#[pyclass(module = "terrio", name = "ArrowTable", frozen)]
pub struct PyArrowTable {
    table: ArrowTable,
}

#[pymethods]
impl PyArrowTable {
    /// Exports the table as an Arrow C stream (the Arrow PyCapsule interface).
    ///
    /// Args:
    ///     requested_schema: A schema capsule the consumer would like. Ignored: the table is
    ///         exported with its own schema, as the interface allows.
    ///
    /// Returns:
    ///     PyCapsule: An `arrow_array_stream` capsule.
    #[pyo3(signature = (requested_schema=None))]
    fn __arrow_c_stream__<'py>(&self, py: Python<'py>, requested_schema: Option<Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyCapsule>> {
        let _ = requested_schema;
        stream_capsule(py, &self.table)
    }

    /// Exports the table's schema as an Arrow C schema (the Arrow PyCapsule interface).
    ///
    /// Returns:
    ///     PyCapsule: An `arrow_schema` capsule.
    fn __arrow_c_schema__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyCapsule>> {
        schema_capsule(py, &self.table.schema)
    }

    /// The number of rows.
    #[getter]
    fn num_rows(&self) -> usize {
        self.table.num_rows()
    }

    /// The number of columns, the geometry column included.
    #[getter]
    fn num_columns(&self) -> usize {
        self.table.schema.fields().len()
    }

    /// The column names, the geometry column last.
    #[getter]
    fn column_names(&self) -> Vec<String> {
        self.table.schema.fields().iter().map(|f| f.name().clone()).collect()
    }

    /// The number of record batches.
    #[getter]
    fn num_batches(&self) -> usize {
        self.table.batches.len()
    }

    fn __len__(&self) -> usize {
        self.table.num_rows()
    }

    fn __repr__(&self) -> String {
        let fields = self.table.schema.fields();
        let mut columns: Vec<String> = fields.iter().take(6).map(|f| format!("{}: {}", f.name(), f.data_type())).collect();
        if fields.len() > 6 {
            columns.push(format!("... {} more", fields.len() - 6));
        }
        format!("<terrio.ArrowTable {} rows [{}]>", self.table.num_rows(), columns.join(", "))
    }

    /// Converts the table to a `pyarrow.Table`. Needs pyarrow.
    ///
    /// Returns:
    ///     pyarrow.Table: The table, its geometry column carrying the GeoArrow extension
    ///     metadata.
    ///
    /// Raises:
    ///     ImportError: If pyarrow is not installed.
    fn to_pyarrow<'py>(slf: &Bound<'py, Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        py.import("pyarrow")?.call_method1("table", (slf,))
    }
}

/// Reads the record batches an object exports through the Arrow PyCapsule interface: a stream
/// (`__arrow_c_stream__`: pyarrow tables and readers, GeoPandas' `to_arrow()`, Polars data
/// frames, GDAL layers...) or a struct array (`__arrow_c_array__`: record batches).
fn read_capsules(obj: &Bound<'_, PyAny>) -> PyResult<Option<(Arc<Schema>, Vec<RecordBatch>)>> {
    if obj.hasattr("__arrow_c_stream__")? {
        let capsule = obj.call_method0("__arrow_c_stream__")?.cast_into::<PyCapsule>()?;
        let ptr = capsule.pointer_checked(Some(STREAM))?;
        // SAFETY: the capsule holds an `ArrowArrayStream` (its name says so); `from_raw` moves the
        // stream out and leaves a released one behind, which the capsule's destructor ignores.
        let reader = unsafe { ArrowArrayStreamReader::from_raw(ptr.as_ptr().cast::<FFI_ArrowArrayStream>()) }.map_err(arrow_err)?;
        let schema = arrow_array::RecordBatchReader::schema(&reader);
        let batches = reader.collect::<Result<Vec<_>, _>>().map_err(arrow_err)?;
        return Ok(Some((schema, batches)));
    }
    if obj.hasattr("__arrow_c_array__")? {
        let pair = obj.call_method0("__arrow_c_array__")?.cast_into::<PyTuple>()?;
        let schema_capsule = pair.get_item(0)?.cast_into::<PyCapsule>()?;
        let array_capsule = pair.get_item(1)?.cast_into::<PyCapsule>()?;
        let schema_ptr = schema_capsule.pointer_checked(Some(SCHEMA))?;
        let array_ptr = array_capsule.pointer_checked(Some(ARRAY))?;
        // SAFETY: the capsules hold an `ArrowSchema` and an `ArrowArray` (their names say so). The
        // schema is borrowed; the array is moved out, leaving a released one behind.
        let (ffi_schema, data) = unsafe {
            let ffi_schema = &*schema_ptr.as_ptr().cast::<FFI_ArrowSchema>();
            let array = FFI_ArrowArray::from_raw(array_ptr.as_ptr().cast::<FFI_ArrowArray>());
            (ffi_schema, arrow_array::ffi::from_ffi(array, ffi_schema).map_err(arrow_err)?)
        };
        let schema = Schema::try_from(ffi_schema).map_err(|_| invalid("__arrow_c_array__ must export a struct array (a record batch)"))?;
        let array = StructArray::from(data);
        let schema = Arc::new(schema);
        let batch = RecordBatch::try_new(schema.clone(), array.columns().to_vec()).map_err(arrow_err)?;
        let _ = array.len();
        return Ok(Some((schema, vec![batch])));
    }
    Ok(None)
}

/// Whether an object exports Arrow data through the PyCapsule interface.
pub(crate) fn is_arrow(obj: &Bound<'_, PyAny>) -> PyResult<bool> {
    Ok(obj.hasattr("__arrow_c_stream__")? || obj.hasattr("__arrow_c_array__")?)
}

/// Converts an object exporting Arrow data into a layer.
pub(crate) fn layer_from_object(obj: &Bound<'_, PyAny>, name: Option<&str>, crs: Option<&str>) -> PyResult<terrio::Layer> {
    // A GeoDataFrame exports itself with `to_arrow()` (pandas' own `__arrow_c_stream__` cannot
    // convert its geometry column).
    if obj.hasattr("__geo_interface__")? && obj.hasattr("to_arrow")? {
        return layer_from_object(&obj.call_method0("to_arrow")?, name, crs);
    }
    let (schema, batches) = match read_capsules(obj)? {
        Some(x) => x,
        // A GeoDataFrame, which exports itself with `to_arrow()`.
        None if obj.hasattr("to_arrow")? => {
            read_capsules(&obj.call_method0("to_arrow")?)?.ok_or_else(|| invalid("to_arrow() did not return Arrow data"))?
        }
        None => return Err(invalid("expected an object with __arrow_c_stream__ or __arrow_c_array__ (a pyarrow Table, ...)")),
    };
    let mut layer = obj.py().detach_result(|| layer_from_arrow(&schema, &batches, name.unwrap_or("layer"))).py()?;
    if let Some(c) = crs {
        SpatialRef::parse(c).py()?.apply_to_layer(&mut layer);
    }
    Ok(layer)
}

/// The Arrow export options from the Python keyword arguments.
pub(crate) fn write_options(
    geometry_encoding: &str,
    coordinates: &str,
    geometry_column: &str,
    fid_column: Option<&str>,
) -> PyResult<ArrowWriteOptions> {
    Ok(ArrowWriteOptions {
        geometry_encoding: GeometryEncoding::parse(geometry_encoding).py()?,
        coordinates: CoordinateLayout::parse(coordinates).py()?,
        geometry_column: geometry_column.to_string(),
        fid_column: fid_column.map(str::to_string),
        ..Default::default()
    })
}

/// Converts a layer to an `ArrowTable`.
pub(crate) fn layer_to_table(py: Python<'_>, layer: &terrio::Layer, options: &ArrowWriteOptions) -> PyResult<PyArrowTable> {
    Ok(PyArrowTable { table: py.detach_result(|| terrio::arrow::layer_to_arrow(layer, options)).py()? })
}

/// Exports a layer as an Arrow C stream, with WKB geometries.
pub(crate) fn layer_stream<'py>(py: Python<'py>, layer: &terrio::Layer) -> PyResult<Bound<'py, PyCapsule>> {
    let table = py.detach_result(|| terrio::arrow::layer_to_arrow(layer, &ArrowWriteOptions::default())).py()?;
    stream_capsule(py, &table)
}

/// Creates a layer from Arrow data.
///
/// Args:
///     data: Any object exporting Arrow data through the PyCapsule interface: a
///         `pyarrow.Table`, `RecordBatch` or `RecordBatchReader`, the result of
///         `GeoDataFrame.to_arrow()` (or a GeoDataFrame itself), a Polars data frame, a GDAL
///         layer, a terrio `ArrowTable`... The geometry column is found by its GeoArrow
///         extension metadata (WKB, WKT or native encodings), GDAL's `ogc.wkb`, GeoParquet
///         `geo` metadata, or a binary column named `geometry` or `wkb_geometry`.
///     name: The layer name (`"layer"` by default).
///     crs: The CRS, overriding the one in the geometry column's metadata.
///
/// Returns:
///     Layer: The layer, its features numbered from 0.
///
/// Raises:
///     InvalidArgumentError: If `data` does not export Arrow data.
///     UnsupportedError: For GeoArrow geometry types terrio cannot read (boxes, geometry
///         collections).
#[pyfunction]
#[pyo3(signature = (data, name=None, crs=None))]
pub(crate) fn from_arrow(data: &Bound<'_, PyAny>, name: Option<&str>, crs: Option<&str>) -> PyResult<PyLayer> {
    Ok(PyLayer { inner: layer_from_object(data, name, crs)? })
}
