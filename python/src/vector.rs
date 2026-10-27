//! The `VectorDataset` and `Layer` classes.

use crate::{PyResultExt, crs_string, invalid};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyString};
use terrio::crs::SpatialRef;
use terrio::{FieldValue, Layer, VectorDataset};

/// A vector layer: features with geometries and attributes, in memory.
#[pyclass(module = "terrio", name = "Layer")]
pub struct PyLayer {
    pub(crate) inner: Layer,
}

/// A vector dataset: one or more layers.
#[pyclass(module = "terrio", name = "VectorDataset")]
pub struct PyVectorDataset {
    layers: Vec<Py<PyLayer>>,
    driver: String,
}

impl PyVectorDataset {
    /// Wraps a vector dataset read by terrio.
    pub(crate) fn new(py: Python<'_>, ds: VectorDataset) -> PyResult<Self> {
        let layers = ds.layers.into_iter().map(|l| Py::new(py, PyLayer { inner: l })).collect::<PyResult<_>>()?;
        Ok(Self { layers, driver: ds.driver.name().to_string() })
    }

    /// The name of the driver that read the dataset.
    pub(crate) fn driver_name(&self) -> String {
        self.driver.clone()
    }

    /// Copies of the layers.
    pub(crate) fn layer_copies(&self, py: Python<'_>) -> Vec<Layer> {
        self.layers.iter().map(|l| l.borrow(py).inner.clone()).collect()
    }
}

/// Converts an attribute value to a Python object.
fn value_to_py<'py>(py: Python<'py>, v: &FieldValue) -> PyResult<Bound<'py, PyAny>> {
    Ok(match v {
        FieldValue::Integer(i) => i.into_pyobject(py)?.into_any(),
        FieldValue::Float(f) => f.into_pyobject(py)?.into_any(),
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => PyString::new(py, s).into_any(),
        FieldValue::Boolean(b) => b.into_pyobject(py)?.to_owned().into_any(),
        FieldValue::Blob(b) => PyBytes::new(py, b).into_any(),
        FieldValue::Null => py.None().into_bound(py),
    })
}

/// Parses GeoJSON text into a layer.
fn layer_from_geojson_text(text: &str, name: Option<&str>, crs: Option<&str>) -> PyResult<Layer> {
    let mut layer = terrio::memory::parse_geojson_layer(text.as_bytes(), "layer").py()?;
    layer.name = name.unwrap_or("layer").to_string();
    let crs = match crs {
        Some(c) => SpatialRef::parse(c).py()?,
        None => SpatialRef::from_epsg(4326).py()?,
    };
    layer.set_crs_epsg(crs.epsg);
    layer.set_crs_wkt(crs.to_wkt());
    Ok(layer)
}

/// Converts a GeoJSON-like Python object (a dict, a JSON string, or an object with
/// `__geo_interface__`) into a layer.
pub(crate) fn layer_from_object(obj: &Bound<'_, PyAny>, name: Option<&str>, crs: Option<&str>) -> PyResult<Layer> {
    let py = obj.py();
    let text: String = if let Ok(s) = obj.extract::<String>() {
        s
    } else {
        let value = if obj.hasattr("__geo_interface__")? { obj.getattr("__geo_interface__")? } else { obj.clone() };
        py.import("json")?.call_method1("dumps", (value,))?.extract()?
    };
    // A GeoDataFrame knows its CRS.
    let crs = match crs {
        Some(c) => Some(c.to_string()),
        None => match obj.getattr("crs") {
            Ok(c) if !c.is_none() => c.call_method0("to_wkt").ok().and_then(|w| w.extract::<String>().ok()),
            _ => None,
        },
    };
    layer_from_geojson_text(&text, name, crs.as_deref())
}

/// The layers in a Python object: a `Layer`, a `VectorDataset`, a sequence of layers, or a
/// GeoJSON-like object.
pub(crate) fn layers_from_object(obj: &Bound<'_, PyAny>, name: Option<&str>, crs: Option<&str>) -> PyResult<Vec<Layer>> {
    if let Ok(l) = obj.cast::<PyLayer>() {
        return Ok(vec![l.borrow().inner.clone()]);
    }
    if let Ok(ds) = obj.cast::<PyVectorDataset>() {
        return Ok(ds.borrow().layer_copies(obj.py()));
    }
    if obj.cast::<PyList>().is_ok() || obj.cast::<pyo3::types::PyTuple>().is_ok() {
        let items: Vec<Bound<'_, PyAny>> = obj.extract()?;
        if !items.is_empty() && items.iter().all(|i| i.cast::<PyLayer>().is_ok()) {
            return items.iter().map(|i| Ok(i.cast::<PyLayer>()?.borrow().inner.clone())).collect();
        }
    }
    // Arrow data (a pyarrow Table, a Polars data frame...); GeoDataFrames keep going through
    // their geo interface.
    if !obj.hasattr("__geo_interface__")? && crate::arrow::is_arrow(obj)? {
        return Ok(vec![crate::arrow::layer_from_object(obj, name, crs)?]);
    }
    Ok(vec![layer_from_object(obj, name, crs)?])
}

#[pymethods]
impl PyLayer {
    /// The layer name.
    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[setter]
    fn set_name(&mut self, name: String) {
        self.inner.name = name;
    }

    /// The CRS: `"EPSG:xxxx"` (or `"ESRI:xxxx"`) when its code is known, otherwise its WKT, or
    /// `None`.
    #[getter]
    fn crs(&self) -> Option<String> {
        crs_string(&SpatialRef::from_layer(&self.inner))
    }

    /// The CRS as WKT, or `None` when it is unknown.
    #[getter]
    fn crs_wkt(&self) -> Option<String> {
        SpatialRef::from_layer(&self.inner).to_wkt()
    }

    /// The geometry type, such as `"Polygon"`, `"MultiLineString Z"`, `"Unknown"` or `"None"`.
    #[getter]
    fn geometry_type(&self) -> String {
        terrio::vector::geometry_type_name(&self.inner)
    }

    /// The attribute fields, as a dict of field name to type name (`"Integer"`, `"Float"`,
    /// `"Text"`, `"Boolean"`, `"Date"`, `"DateTime"`, `"Blob"`, `"Json"`).
    #[getter]
    fn fields<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for f in self.inner.schema.fields() {
            d.set_item(&f.name, f.field_type.as_str())?;
        }
        Ok(d)
    }

    /// The extent `(min_x, min_y, max_x, max_y)` of the geometries, or `None` when there are none.
    #[getter]
    fn bounds(&self) -> Option<(f64, f64, f64, f64)> {
        terrio::vector::layer_extent(&self.inner)
    }

    fn __len__(&self) -> usize {
        self.inner.features.len()
    }

    fn __repr__(&self) -> String {
        format!(
            "<terrio.Layer '{}' {} features, {} crs={}>",
            self.inner.name,
            self.inner.features.len(),
            self.geometry_type(),
            SpatialRef::from_layer(&self.inner).label()
        )
    }

    /// The layer as a GeoJSON FeatureCollection, in the layer's CRS.
    ///
    /// Returns:
    ///     dict: The FeatureCollection.
    fn to_geojson<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let text = wbvector::geojson::to_string(&self.inner);
        py.import("json")?.call_method1("loads", (text,))
    }

    /// The GeoJSON FeatureCollection of the layer (the geo interface protocol).
    #[getter]
    fn __geo_interface__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.to_geojson(py)
    }

    /// The geometries as Well-Known Binary.
    ///
    /// Returns:
    ///     list[bytes | None]: One WKB geometry per feature, `None` for features without one.
    fn to_wkb<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let items: Vec<Bound<'py, PyAny>> = self
            .inner
            .features
            .iter()
            .map(|f| match &f.geometry {
                Some(g) => PyBytes::new(py, &g.to_wkb()).into_any(),
                None => py.None().into_bound(py),
            })
            .collect();
        PyList::new(py, items)
    }

    /// The attribute values, by column.
    ///
    /// Returns:
    ///     dict[str, list]: A list of values per field, in field order. Dates and times are ISO
    ///     8601 strings; missing values are `None`.
    fn columns<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for (i, f) in self.inner.schema.fields().iter().enumerate() {
            let values: Vec<Bound<'py, PyAny>> = self
                .inner
                .features
                .iter()
                .map(|feat| value_to_py(py, feat.attributes.get(i).unwrap_or(&FieldValue::Null)))
                .collect::<PyResult<_>>()?;
            d.set_item(&f.name, PyList::new(py, values)?)?;
        }
        Ok(d)
    }

    /// Converts the layer to a GeoDataFrame, through WKB geometries. Needs geopandas.
    ///
    /// Returns:
    ///     geopandas.GeoDataFrame: The features, with the layer's CRS.
    ///
    /// Raises:
    ///     ImportError: If geopandas is not installed.
    fn to_geopandas<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let gpd = py.import("geopandas")?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("crs", self.crs_wkt().map(|w| SpatialRef::from_layer(&self.inner).authority_code().unwrap_or(w)))?;
        let geometry = gpd.getattr("GeoSeries")?.call_method("from_wkb", (self.to_wkb(py)?,), Some(&kwargs))?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("geometry", geometry)?;
        gpd.getattr("GeoDataFrame")?.call((self.columns(py)?,), Some(&kwargs))
    }

    /// The layer as Arrow data, with the geometries in a GeoArrow column.
    ///
    /// The result takes the Arrow PyCapsule interface: `pyarrow.table(t)`,
    /// `geopandas.GeoDataFrame.from_arrow(t)`, `polars.DataFrame(t)`, DuckDB and GDAL read it
    /// without copying through Python, and pyarrow is not needed to create it.
    ///
    /// Args:
    ///     geometry_encoding: `"wkb"` (the default: `geoarrow.wkb`, any geometry) or
    ///         `"geoarrow"`: the native encodings (`geoarrow.point`, `geoarrow.multipolygon`...),
    ///         for layers whose geometries share one type (a type and its multi-part form are
    ///         written as the multi-part form).
    ///     coordinates: The layout of native coordinates: `"separated"` (a struct of `x`, `y`
    ///         arrays, the default) or `"interleaved"` (fixed-size lists).
    ///     geometry_column: The name of the geometry column.
    ///     fid_column: When given, a first column of this name holds the feature ids.
    ///
    /// Returns:
    ///     ArrowTable: The attributes as typed columns and the geometry column last, its CRS as
    ///     PROJJSON in the GeoArrow extension metadata.
    ///
    /// Raises:
    ///     UnsupportedError: If native encodings are asked for geometries of mixed types or
    ///         geometry collections.
    #[pyo3(signature = (geometry_encoding="wkb", coordinates="separated", geometry_column="geometry", fid_column=None))]
    fn to_arrow(
        &self,
        py: Python<'_>,
        geometry_encoding: &str,
        coordinates: &str,
        geometry_column: &str,
        fid_column: Option<&str>,
    ) -> PyResult<crate::arrow::PyArrowTable> {
        let options = crate::arrow::write_options(geometry_encoding, coordinates, geometry_column, fid_column)?;
        crate::arrow::layer_to_table(py, &self.inner, &options)
    }

    /// Exports the layer as an Arrow C stream, with WKB geometries (the Arrow PyCapsule
    /// interface), so that `pyarrow.table(layer)` and the like take a layer directly.
    ///
    /// Args:
    ///     requested_schema: A schema capsule the consumer would like. Ignored.
    ///
    /// Returns:
    ///     PyCapsule: An `arrow_array_stream` capsule.
    #[pyo3(signature = (requested_schema=None))]
    fn __arrow_c_stream__<'py>(
        &self,
        py: Python<'py>,
        requested_schema: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, pyo3::types::PyCapsule>> {
        let _ = requested_schema;
        crate::arrow::layer_stream(py, &self.inner)
    }

    /// Creates a layer from Arrow data.
    ///
    /// Args:
    ///     data: Any object exporting Arrow data through the PyCapsule interface (a
    ///         `pyarrow.Table`, `GeoDataFrame.to_arrow()`, a Polars data frame, a GDAL layer...).
    ///     name: The layer name.
    ///     crs: The CRS, overriding the one in the geometry column's metadata.
    ///
    /// Returns:
    ///     Layer: The layer.
    #[staticmethod]
    #[pyo3(signature = (data, name=None, crs=None))]
    fn from_arrow(data: &Bound<'_, PyAny>, name: Option<&str>, crs: Option<&str>) -> PyResult<Self> {
        Ok(Self { inner: crate::arrow::layer_from_object(data, name, crs)? })
    }

    /// Reprojects the layer.
    ///
    /// Args:
    ///     crs: The target CRS: `"EPSG:3857"`, WKT or a PROJ string.
    ///
    /// Returns:
    ///     Layer: A new, reprojected layer.
    ///
    /// Raises:
    ///     CRSError: If a CRS is unknown or cannot be parsed.
    fn to_crs(&self, py: Python<'_>, crs: &str) -> PyResult<Self> {
        let dst = SpatialRef::parse(crs).py()?;
        let inner = &self.inner;
        Ok(Self { inner: py.detach(|| terrio::vector::reproject_layer(inner, &dst)).py()? })
    }

    /// Creates a layer from GeoJSON.
    ///
    /// Args:
    ///     data: A GeoJSON FeatureCollection, Feature or geometry, as a dict or a JSON string, or
    ///         any object with `__geo_interface__` (such as a GeoDataFrame).
    ///     name: The layer name.
    ///     crs: The coordinates' CRS. By default, that of a GeoDataFrame, otherwise EPSG:4326.
    ///
    /// Returns:
    ///     Layer: The layer.
    #[staticmethod]
    #[pyo3(signature = (data, name=None, crs=None))]
    fn from_geojson(data: &Bound<'_, PyAny>, name: Option<&str>, crs: Option<&str>) -> PyResult<Self> {
        Ok(Self { inner: layer_from_object(data, name, crs)? })
    }

    /// Writes the layer to a file.
    ///
    /// Args:
    ///     path: The output path. Its extension tells the format unless `driver` is given.
    ///     driver: The format's short name, such as `"GeoJSON"`, `"GPKG"` or `"ESRI Shapefile"`.
    #[pyo3(signature = (path, driver=None))]
    fn write(&self, py: Python<'_>, path: &str, driver: Option<&str>) -> PyResult<()> {
        let options = crate::ops::vector_write_options(driver)?;
        let layers = std::slice::from_ref(&self.inner);
        py.detach(|| terrio::write_vector(layers, path, &options)).py()
    }

    /// Encodes the layer in a file format, in memory.
    ///
    /// Args:
    ///     filename: A file name whose extension tells the format, such as `"out.fgb"`.
    ///     driver: The format's short name, overriding the extension.
    ///
    /// Returns:
    ///     bytes: The file's contents. Shapefiles require a `.shp.zip` filename and
    ///     return a ZIP archive (MIME `application/zip`) with geometry, index, attributes,
    ///     encoding and CRS members. Bare `.shp` names and unsupported DBF values
    ///     (including text over 254 UTF-8 bytes) raise an error.
    #[pyo3(signature = (filename="layer.geojson", driver=None))]
    fn to_bytes<'py>(&self, py: Python<'py>, filename: &str, driver: Option<&str>) -> PyResult<Bound<'py, PyBytes>> {
        let options = crate::ops::vector_write_options(driver)?;
        let layers = std::slice::from_ref(&self.inner);
        let bytes = py.detach(|| terrio::memory::write_vector_bytes(layers, filename, &options)).py()?;
        Ok(PyBytes::new(py, &bytes))
    }
}

#[pymethods]
impl PyVectorDataset {
    /// The layers.
    #[getter]
    fn layers<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        PyList::new(py, &self.layers)
    }

    /// The layer names.
    #[getter]
    fn layer_names(&self, py: Python<'_>) -> Vec<String> {
        self.layers.iter().map(|l| l.borrow(py).inner.name.clone()).collect()
    }

    /// The name of the format driver that read the dataset, such as `"GPKG"`.
    #[getter]
    fn driver(&self) -> &str {
        &self.driver
    }

    fn __len__(&self) -> usize {
        self.layers.len()
    }

    fn __getitem__(&self, py: Python<'_>, key: &Bound<'_, PyAny>) -> PyResult<Py<PyLayer>> {
        if let Ok(i) = key.extract::<isize>() {
            let n = self.layers.len() as isize;
            let j = if i < 0 { i + n } else { i };
            if !(0..n).contains(&j) {
                return Err(pyo3::exceptions::PyIndexError::new_err(format!("layer index {i} out of range")));
            }
            return Ok(self.layers[j as usize].clone_ref(py));
        }
        let name: String = key.extract().map_err(|_| invalid("layers are indexed by position or name"))?;
        self.layers
            .iter()
            .find(|l| l.borrow(py).inner.name == name)
            .map(|l| l.clone_ref(py))
            .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err(name))
    }

    fn __iter__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        Ok(PyList::new(py, &self.layers)?.try_iter()?.into_any())
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        format!("<terrio.VectorDataset driver={} layers={:?}>", self.driver, self.layer_names(py))
    }

    /// Writes the layers to a file. Only GeoPackage holds several layers.
    ///
    /// Args:
    ///     path: The output path. Its extension tells the format unless `driver` is given.
    ///     driver: The format's short name.
    #[pyo3(signature = (path, driver=None))]
    fn write(&self, py: Python<'_>, path: &str, driver: Option<&str>) -> PyResult<()> {
        let options = crate::ops::vector_write_options(driver)?;
        let layers = self.layer_copies(py);
        py.detach(|| terrio::write_vector(&layers, path, &options)).py()
    }
}
