//! The `PointCloud` and `PointCloudFile` classes: LAS, LAZ and COPC point clouds.

use crate::{PyResultExt, crs_string, invalid};
use numpy::{PyArray1, PyArrayDescrMethods, PyArrayDyn, PyArrayMethods, PyUntypedArray, PyUntypedArrayMethods};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use terrio::crs::SpatialRef;
use terrio::pointcloud::grid::{GridOptions, GridStat};
use terrio::pointcloud::{
    self, Dimension, PointCloud, PointCloudReader, PointFilter, PointFormat, PointReadOptions, PointWriteOptions, ReturnKind, Values,
};

/// A point cloud in memory: columns of values, one per dimension.
///
/// Dimensions are named as laspy names them (`x`, `y`, `z`, `intensity`, `return_number`,
/// `number_of_returns`, `classification`, `gps_time`, `red`...). `cloud["x"]` is a NumPy array;
/// `x`, `y` and `z` are coordinates with the file's scale and offset applied, extra-bytes
/// dimensions with a scale or offset are scaled, other dimensions keep their LAS type.
#[pyclass(module = "terrio", name = "PointCloud")]
pub struct PyPointCloud {
    pub(crate) inner: PointCloud,
}

/// An open LAS, LAZ or COPC file: its header is read, its points on demand.
///
/// Remote COPC files are read in place: `read` fetches only the octree nodes that intersect the
/// bounds asked for, down to the level asked for, with HTTP range requests.
#[pyclass(module = "terrio", name = "PointCloudFile")]
pub struct PyPointCloudFile {
    inner: PointCloudReader,
}

/// Moves a column into a NumPy array, without converting it.
fn values_to_numpy<'py>(py: Python<'py>, v: Values) -> Bound<'py, PyAny> {
    match v {
        Values::U8(v) => PyArray1::from_vec(py, v).into_any(),
        Values::I8(v) => PyArray1::from_vec(py, v).into_any(),
        Values::U16(v) => PyArray1::from_vec(py, v).into_any(),
        Values::I16(v) => PyArray1::from_vec(py, v).into_any(),
        Values::U32(v) => PyArray1::from_vec(py, v).into_any(),
        Values::I32(v) => PyArray1::from_vec(py, v).into_any(),
        Values::U64(v) => PyArray1::from_vec(py, v).into_any(),
        Values::I64(v) => PyArray1::from_vec(py, v).into_any(),
        Values::F32(v) => PyArray1::from_vec(py, v).into_any(),
        Values::F64(v) => PyArray1::from_vec(py, v).into_any(),
    }
}

/// A dimension's values as NumPy sees them: scaled when it has a scale or offset.
fn dimension_to_numpy<'py>(py: Python<'py>, d: &Dimension) -> Bound<'py, PyAny> {
    if d.scale.is_some() || d.offset.is_some() {
        values_to_numpy(py, Values::F64(d.to_f64()))
    } else {
        values_to_numpy(py, d.values.clone())
    }
}

/// Copies a one-dimensional NumPy array into a column of its type.
fn values_from_numpy(array: &Bound<'_, PyUntypedArray>) -> PyResult<Values> {
    let py = array.py();
    let dtype = array.dtype();
    macro_rules! try_types {
        ($($t:ty => $v:ident),*) => {
            $(if dtype.is_equiv_to(&numpy::dtype::<$t>(py)) {
                let a = array.cast::<PyArrayDyn<$t>>()?.readonly();
                return Ok(Values::$v(a.as_array().iter().copied().collect()));
            })*
        };
    }
    try_types!(u8 => U8, i8 => I8, u16 => U16, i16 => I16, u32 => U32, i32 => I32, u64 => U64, i64 => I64, f32 => F32, f64 => F64);
    if dtype.is_equiv_to(&numpy::dtype::<bool>(py)) {
        let a = array.cast::<PyArrayDyn<bool>>()?.readonly();
        return Ok(Values::U8(a.as_array().iter().map(|b| *b as u8).collect()));
    }
    Err(invalid(format!("unsupported array dtype {dtype}")))
}

/// A JSON-serializable value as a Python object.
fn to_python<'py, T: serde::Serialize>(py: Python<'py>, v: &T) -> PyResult<Bound<'py, PyAny>> {
    let text = serde_json::to_string(v).map_err(|e| invalid(e.to_string()))?;
    py.import("json")?.call_method1("loads", (text,))
}

/// Builds read options from the keyword arguments of `read_points` and `PointCloudFile.read`.
#[allow(clippy::too_many_arguments)]
fn read_options(
    bounds: Option<(f64, f64, f64, f64)>,
    z_range: Option<(f64, f64)>,
    classes: Option<Vec<u8>>,
    returns: Option<Vec<String>>,
    max_level: Option<u32>,
    resolution: Option<f64>,
    max_points: Option<u64>,
) -> PyResult<PointReadOptions> {
    let returns = returns.unwrap_or_default().iter().map(|r| ReturnKind::parse(r)).collect::<terrio::Result<Vec<_>>>().py()?;
    Ok(PointReadOptions {
        filter: PointFilter { bounds: bounds.map(|b| [b.0, b.1, b.2, b.3]), z_range, classes: classes.unwrap_or_default(), returns },
        max_level,
        resolution,
        max_points,
    })
}

/// Opens a LAS, LAZ or COPC file, reading only its header.
///
/// Args:
///     path: The file's path or URL (`https://`, `s3://`, `gs://`, `az://`), or a zip archive
///         member. Remote COPC files are read in place; other remote files are downloaded.
///
/// Returns:
///     PointCloudFile: The open file.
#[pyfunction]
pub(crate) fn open_points(py: Python<'_>, path: &str) -> PyResult<PyPointCloudFile> {
    let inner = py.detach(|| pointcloud::open_points(path)).py()?;
    Ok(PyPointCloudFile { inner })
}

/// Reads the points of a LAS, LAZ or COPC file.
///
/// Args:
///     path: The file's path or URL, as for `open_points`.
///     bounds: Keep the points in `(min_x, min_y, max_x, max_y)`, in the file's CRS.
///     z_range: Keep the points with `z` in `(min, max)`.
///     classes: Keep the points of these classifications.
///     returns: Keep these returns of each pulse: `"first"`, `"last"`, `"intermediate"`,
///         `"only"`.
///     max_level: COPC: the deepest octree level to read (0 is the root).
///     resolution: COPC: read the levels needed for this point spacing.
///     max_points: At most about this many points: COPC files are read down to the deepest
///         level that fits, other files are thinned by keeping every n-th point.
///
/// Returns:
///     PointCloud: The points.
#[pyfunction]
#[pyo3(signature = (path, *, bounds=None, z_range=None, classes=None, returns=None, max_level=None, resolution=None, max_points=None))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_points(
    py: Python<'_>,
    path: &str,
    bounds: Option<(f64, f64, f64, f64)>,
    z_range: Option<(f64, f64)>,
    classes: Option<Vec<u8>>,
    returns: Option<Vec<String>>,
    max_level: Option<u32>,
    resolution: Option<f64>,
    max_points: Option<u64>,
) -> PyResult<PyPointCloud> {
    let options = read_options(bounds, z_range, classes, returns, max_level, resolution, max_points)?;
    let inner = py.detach(|| pointcloud::read_points(path, &options)).py()?;
    Ok(PyPointCloud { inner })
}

/// Reads a LAS, LAZ or COPC file held in memory.
///
/// Args:
///     data: The file's contents.
///
/// Returns:
///     PointCloud: The points.
#[pyfunction]
pub(crate) fn read_points_bytes(py: Python<'_>, data: Vec<u8>) -> PyResult<PyPointCloud> {
    let inner = py.detach(|| pointcloud::open_points_bytes(data, "points.las")?.read(&PointReadOptions::default())).py()?;
    Ok(PyPointCloud { inner })
}

/// Converts a point cloud to LAS, LAZ, COPC or a vector point layer, filtering and reprojecting
/// on the way, as `terrio convert` does.
///
/// Args:
///     src: The input file's path or URL.
///     dst: The output path: `.las`, `.laz`, `.copc.laz`, or a vector format (`.gpkg`, `.fgb`,
///         `.parquet`, `.geojson`, `.shp`...) for a layer of points.
///     bounds: Keep the points in `(min_x, min_y, max_x, max_y)`, in the input's CRS.
///     z_range: Keep the points with `z` in `(min, max)`.
///     classes: Keep the points of these classifications.
///     returns: Keep these returns: `"first"`, `"last"`, `"intermediate"`, `"only"`.
///     max_level: COPC inputs: the deepest octree level to read.
///     resolution: COPC inputs: read the levels needed for this point spacing.
///     max_points: At most about this many points.
///     a_srs: Assign this CRS to the input, without reprojecting.
///     t_srs: Reproject `x` and `y` into this CRS.
///     point_format: The output point format (0 to 10).
///     minor_version: The output LAS minor version (0 to 4).
///     scale: The output scale `(x, y, z)`.
///     offset: The output offset `(x, y, z)`.
///     dimensions: For vector outputs, the dimensions written as attributes (all by default).
#[pyfunction]
#[pyo3(signature = (
    src, dst, *, bounds=None, z_range=None, classes=None, returns=None, max_level=None, resolution=None, max_points=None,
    a_srs=None, t_srs=None, point_format=None, minor_version=None, scale=None, offset=None, dimensions=None
))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_points(
    py: Python<'_>,
    src: &str,
    dst: &str,
    bounds: Option<(f64, f64, f64, f64)>,
    z_range: Option<(f64, f64)>,
    classes: Option<Vec<u8>>,
    returns: Option<Vec<String>>,
    max_level: Option<u32>,
    resolution: Option<f64>,
    max_points: Option<u64>,
    a_srs: Option<&str>,
    t_srs: Option<&str>,
    point_format: Option<u8>,
    minor_version: Option<u8>,
    scale: Option<(f64, f64, f64)>,
    offset: Option<(f64, f64, f64)>,
    dimensions: Option<Vec<String>>,
) -> PyResult<()> {
    let options = pointcloud::ConvertOptions {
        read: read_options(bounds, z_range, classes, returns, max_level, resolution, max_points)?,
        assign_srs: a_srs.map(SpatialRef::parse).transpose().py()?,
        target_srs: t_srs.map(SpatialRef::parse).transpose().py()?,
        write: PointWriteOptions {
            point_format,
            minor_version,
            scale: scale.map(|s| [s.0, s.1, s.2]),
            offset: offset.map(|o| [o.0, o.1, o.2]),
            ..Default::default()
        },
        layer: pointcloud::vector::ToLayerOptions { dimensions: dimensions.unwrap_or_default(), ..Default::default() },
    };
    py.detach(|| pointcloud::convert(src, dst, &options)).py()
}

#[pymethods]
impl PyPointCloudFile {
    /// `"LAS"`, `"LAZ"` or `"COPC"`.
    #[getter]
    fn format(&self) -> &'static str {
        self.inner.format_name()
    }

    /// The number of points.
    #[getter]
    fn point_count(&self) -> u64 {
        self.inner.header().point_count
    }

    /// The CRS: `"EPSG:xxxx"` when the code is known, otherwise the WKT, or `None`.
    #[getter]
    fn crs(&self) -> Option<String> {
        crs_string(self.inner.crs())
    }

    /// The bounds from the header: `(min_x, min_y, min_z, max_x, max_y, max_z)`.
    #[getter]
    fn bounds(&self) -> (f64, f64, f64, f64, f64, f64) {
        let h = self.inner.header();
        (h.min[0], h.min[1], h.min[2], h.max[0], h.max[1], h.max[2])
    }

    /// The header: version, point format, counts, scale, offset, VLRs...
    #[getter]
    fn header<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        to_python(py, self.inner.header())
    }

    /// The dimensions: `{name: dtype}`.
    #[getter]
    fn dimensions<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for (name, t) in self.inner.dimensions() {
            d.set_item(name, t.name())?;
        }
        Ok(d)
    }

    /// The summary `terrio info --json` prints for the file.
    ///
    /// Args:
    ///     stats: Read every point to count classifications and returns.
    ///
    /// Returns:
    ///     dict: The summary.
    #[pyo3(signature = (stats=false))]
    fn info<'py>(&mut self, py: Python<'py>, stats: bool) -> PyResult<Bound<'py, PyAny>> {
        let info = py.detach(|| pointcloud::info::point_cloud_info(&mut self.inner, stats)).py()?;
        to_python(py, &info)
    }

    /// The COPC octree nodes, for COPC files: one dict per node with `key` (`level`, `x`, `y`,
    /// `z`), `offset`, `byte_size` and `point_count`.
    fn copc_nodes<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let nodes = py.detach(|| self.inner.copc_nodes()).py()?;
        to_python(py, &nodes)
    }

    /// Reads the points.
    ///
    /// Args:
    ///     bounds: Keep the points in `(min_x, min_y, max_x, max_y)`, in the file's CRS.
    ///     z_range: Keep the points with `z` in `(min, max)`.
    ///     classes: Keep the points of these classifications.
    ///     returns: Keep these returns: `"first"`, `"last"`, `"intermediate"`, `"only"`.
    ///     max_level: COPC: the deepest octree level to read.
    ///     resolution: COPC: read the levels needed for this point spacing.
    ///     max_points: At most about this many points.
    ///
    /// Returns:
    ///     PointCloud: The points.
    #[pyo3(signature = (*, bounds=None, z_range=None, classes=None, returns=None, max_level=None, resolution=None, max_points=None))]
    #[allow(clippy::too_many_arguments)]
    fn read(
        &mut self,
        py: Python<'_>,
        bounds: Option<(f64, f64, f64, f64)>,
        z_range: Option<(f64, f64)>,
        classes: Option<Vec<u8>>,
        returns: Option<Vec<String>>,
        max_level: Option<u32>,
        resolution: Option<f64>,
        max_points: Option<u64>,
    ) -> PyResult<PyPointCloud> {
        let options = read_options(bounds, z_range, classes, returns, max_level, resolution, max_points)?;
        let inner = py.detach(|| self.inner.read(&options)).py()?;
        Ok(PyPointCloud { inner })
    }

    fn __repr__(&self) -> String {
        let h = self.inner.header();
        format!(
            "<PointCloudFile {} {}: LAS {}, format {}, {} points>",
            self.inner.format_name(),
            self.inner.name(),
            h.version_string(),
            h.point_format,
            h.point_count
        )
    }
}

#[pymethods]
impl PyPointCloud {
    /// Makes a point cloud from arrays.
    ///
    /// Args:
    ///     arrays: `{name: array}`, with at least `x`, `y` and `z`. Standard LAS dimensions
    ///         (`intensity`, `classification`...) are converted to their LAS types; other arrays
    ///         become extra-bytes dimensions of their own type.
    ///     crs: The CRS (`"EPSG:xxxx"`, WKT, PROJ string...).
    ///
    /// Returns:
    ///     PointCloud: The point cloud, with a scale and offset suited to the coordinates.
    #[staticmethod]
    #[pyo3(signature = (arrays, crs=None))]
    fn from_dict(arrays: &Bound<'_, PyDict>, crs: Option<&str>) -> PyResult<Self> {
        let mut dims = Vec::new();
        let mut n = None;
        for (k, v) in arrays.iter() {
            let name: String = k.extract()?;
            let np = arrays.py().import("numpy")?;
            let array = np.call_method1("ascontiguousarray", (v,))?;
            let values = values_from_numpy(array.cast::<PyUntypedArray>()?)?;
            if *n.get_or_insert(values.len()) != values.len() {
                return Err(invalid(format!("array '{name}' has {} values, the others {}", values.len(), n.unwrap_or(0))));
            }
            dims.push(Dimension::new(&name, values));
        }
        for c in ["x", "y", "z"] {
            if !dims.iter().any(|d| d.name == c) {
                return Err(invalid(format!("no '{c}' array")));
            }
        }
        // Standard dimensions take their LAS type.
        for d in &mut dims {
            if let Some(t) = pointcloud::standard_value_type(&d.name) {
                d.values = d.values.cast(t);
            }
        }
        let crs = crs.map(SpatialRef::parse).transpose().py()?.unwrap_or_default();
        Ok(Self { inner: pointcloud::from_dimensions(dims, crs) })
    }

    fn __len__(&self) -> usize {
        self.inner.len()
    }

    fn __contains__(&self, name: &str) -> bool {
        self.inner.dim(name).is_some()
    }

    /// A dimension's values as a NumPy array.
    fn __getitem__<'py>(&self, py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyAny>> {
        let d = self.inner.dim(name).ok_or_else(|| pyo3::exceptions::PyKeyError::new_err(name.to_string()))?;
        Ok(dimension_to_numpy(py, d))
    }

    /// The dimension names.
    #[getter]
    fn dimensions(&self) -> Vec<String> {
        self.inner.dimensions.iter().map(|d| d.name.clone()).collect()
    }

    /// The CRS: `"EPSG:xxxx"` when the code is known, otherwise the WKT, or `None`.
    #[getter]
    fn crs(&self) -> Option<String> {
        crs_string(&self.inner.crs)
    }

    /// The bounds of the points: `(min_x, min_y, min_z, max_x, max_y, max_z)`, or `None`.
    #[getter]
    fn bounds(&self) -> Option<(f64, f64, f64, f64, f64, f64)> {
        self.inner.bounds().map(|b| (b[0], b[1], b[2], b[3], b[4], b[5]))
    }

    /// The header of the file the points come from.
    #[getter]
    fn header<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        to_python(py, &self.inner.header)
    }

    /// The points as a dict of NumPy arrays, one per dimension.
    ///
    /// Returns:
    ///     dict[str, numpy.ndarray]: The arrays.
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for dim in &self.inner.dimensions {
            d.set_item(&dim.name, dimension_to_numpy(py, dim))?;
        }
        Ok(d)
    }

    /// The points as a NumPy structured array, one field per dimension.
    ///
    /// Returns:
    ///     numpy.ndarray: The array.
    fn to_numpy<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let np = py.import("numpy")?;
        let columns: Vec<(String, Bound<'py, PyAny>)> =
            self.inner.dimensions.iter().map(|d| (d.name.clone(), dimension_to_numpy(py, d))).collect();
        let fields = PyList::empty(py);
        for (name, a) in &columns {
            fields.append((name, a.getattr("dtype")?))?;
        }
        let out = np.call_method1("empty", (self.inner.len(), np.call_method1("dtype", (fields,))?))?;
        for (name, a) in columns {
            out.set_item(name, a)?;
        }
        Ok(out)
    }

    /// The points that pass a filter.
    ///
    /// Args:
    ///     bounds: Keep the points in `(min_x, min_y, max_x, max_y)`.
    ///     z_range: Keep the points with `z` in `(min, max)`.
    ///     classes: Keep the points of these classifications.
    ///     returns: Keep these returns: `"first"`, `"last"`, `"intermediate"`, `"only"`.
    ///
    /// Returns:
    ///     PointCloud: The points kept.
    #[pyo3(signature = (*, bounds=None, z_range=None, classes=None, returns=None))]
    fn filter(
        &self,
        bounds: Option<(f64, f64, f64, f64)>,
        z_range: Option<(f64, f64)>,
        classes: Option<Vec<u8>>,
        returns: Option<Vec<String>>,
    ) -> PyResult<Self> {
        let o = read_options(bounds, z_range, classes, returns, None, None, None)?;
        Ok(Self { inner: self.inner.filter(&o.filter) })
    }

    /// Reprojects `x` and `y` into another CRS.
    ///
    /// Args:
    ///     crs: The target CRS.
    ///
    /// Returns:
    ///     PointCloud: The reprojected points.
    fn reproject(&self, py: Python<'_>, crs: &str) -> PyResult<Self> {
        let dst = SpatialRef::parse(crs).py()?;
        let mut inner = self.inner.clone();
        py.detach(|| inner.reproject(&dst)).py()?;
        Ok(Self { inner })
    }

    /// The number of points of each classification.
    ///
    /// Returns:
    ///     dict[int, int]: `{class: count}`.
    fn classification_counts(&self) -> std::collections::BTreeMap<u8, u64> {
        self.inner.classification_counts().into_iter().collect()
    }

    /// Writes the points to a LAS, LAZ or COPC file.
    ///
    /// Args:
    ///     path: The output path: `.las`, `.laz` or `.copc.laz` (unless `format` says).
    ///     format: `"LAS"`, `"LAZ"` or `"COPC"`.
    ///     point_format: The point format (0 to 10). By default, the source's (6 to 8 for COPC).
    ///     minor_version: The LAS minor version. By default, the source's, raised as the point
    ///         format needs.
    ///     scale: The scale of the stored coordinates `(x, y, z)`.
    ///     offset: The offset of the stored coordinates `(x, y, z)`.
    #[pyo3(signature = (path, *, format=None, point_format=None, minor_version=None, scale=None, offset=None))]
    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        py: Python<'_>,
        path: &str,
        format: Option<&str>,
        point_format: Option<u8>,
        minor_version: Option<u8>,
        scale: Option<(f64, f64, f64)>,
        offset: Option<(f64, f64, f64)>,
    ) -> PyResult<()> {
        let options = write_options(format, point_format, minor_version, scale, offset)?;
        py.detach(|| pointcloud::write_points(&self.inner, path, &options)).py()
    }

    /// The points as the bytes of a LAS, LAZ or COPC file.
    ///
    /// Args:
    ///     format: `"LAS"`, `"LAZ"` or `"COPC"`.
    ///     point_format: The point format (0 to 10).
    ///
    /// Returns:
    ///     bytes: The file.
    #[pyo3(signature = (format="LAZ", point_format=None))]
    fn to_bytes<'py>(&self, py: Python<'py>, format: &str, point_format: Option<u8>) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
        let options = write_options(Some(format), point_format, None, None, None)?;
        let bytes = py.detach(|| pointcloud::write_points_bytes(&self.inner, "points", &options)).py()?;
        Ok(pyo3::types::PyBytes::new(py, &bytes))
    }

    /// Grids the points into a raster, as PDAL's `writers.gdal` does.
    ///
    /// Args:
    ///     resolution: The cell size.
    ///     stat: The statistics, one band each: `"min"`, `"max"`, `"mean"`, `"idw"`, `"count"`,
    ///         `"stdev"`, a list of them, or `"all"`. `"mean"` by default.
    ///     dimension: The dimension gridded. `"z"` by default.
    ///     radius: Points closer than this to a cell centre count. By default the cell
    ///         diagonal.
    ///     power: The power of the inverse distance weights.
    ///     window_size: Fill empty cells from the cells within this many cells.
    ///     bin_mode: Count each point in its own cell only.
    ///     bounds: The grid `(min_x, min_y, max_x, max_y)`. By default the points' bounds.
    ///     nodata: The nodata value.
    ///     ot: The output data type (`"Float32"`, `"Int16"`...). `"Float64"` by default.
    ///
    /// Returns:
    ///     Raster: The grid.
    #[pyo3(signature = (resolution, *, stat=None, dimension="z", radius=None, power=1.0, window_size=0, bin_mode=false, bounds=None, nodata=-9999.0, ot=None))]
    #[allow(clippy::too_many_arguments)]
    fn grid(
        &self,
        py: Python<'_>,
        resolution: f64,
        stat: Option<&Bound<'_, PyAny>>,
        dimension: &str,
        radius: Option<f64>,
        power: f64,
        window_size: usize,
        bin_mode: bool,
        bounds: Option<(f64, f64, f64, f64)>,
        nodata: f64,
        ot: Option<&str>,
    ) -> PyResult<crate::raster::PyRaster> {
        let stats = match stat {
            None => Vec::new(),
            Some(s) => match s.extract::<String>() {
                Ok(s) => GridStat::parse_list(&s).py()?,
                Err(_) => s.extract::<Vec<String>>()?.iter().map(|s| GridStat::parse(s)).collect::<terrio::Result<_>>().py()?,
            },
        };
        let options = GridOptions {
            resolution,
            stats,
            dimension: dimension.to_string(),
            radius,
            power,
            window_size,
            bin_mode,
            bounds: bounds.map(|b| [b.0, b.1, b.2, b.3]),
            nodata,
            data_type: ot.map(terrio::ops::parse_data_type).transpose().py()?.unwrap_or(terrio::DataType::F64),
        };
        let ds = py.detach(|| pointcloud::grid::grid(&self.inner, &options)).py()?;
        Ok(crate::raster::PyRaster::from_dataset(ds))
    }

    /// The points as a vector layer, one point feature each, with the dimensions as attributes.
    ///
    /// Args:
    ///     dimensions: The dimensions written as attributes. All but `x` and `y` by default.
    ///     name: The layer name.
    ///
    /// Returns:
    ///     Layer: The layer.
    #[pyo3(signature = (dimensions=None, name=None))]
    fn to_layer(&self, dimensions: Option<Vec<String>>, name: Option<String>) -> PyResult<crate::vector::PyLayer> {
        let options = pointcloud::vector::ToLayerOptions { name, dimensions: dimensions.unwrap_or_default(), two_d: false };
        Ok(crate::vector::PyLayer { inner: pointcloud::vector::to_layer(&self.inner, &options).py()? })
    }

    fn __repr__(&self) -> String {
        format!("<PointCloud: {} points, dimensions {}>", self.inner.len(), self.inner.dimension_names().join(", "))
    }
}

fn write_options(
    format: Option<&str>,
    point_format: Option<u8>,
    minor_version: Option<u8>,
    scale: Option<(f64, f64, f64)>,
    offset: Option<(f64, f64, f64)>,
) -> PyResult<PointWriteOptions> {
    let format = format
        .map(|f| PointFormat::from_name(f).ok_or_else(|| invalid(format!("unknown point cloud format '{f}' (LAS, LAZ, COPC)"))))
        .transpose()?;
    Ok(PointWriteOptions {
        format,
        point_format,
        minor_version,
        scale: scale.map(|s| [s.0, s.1, s.2]),
        offset: offset.map(|o| [o.0, o.1, o.2]),
        ..Default::default()
    })
}

/// Registers the point cloud functions and classes.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyPointCloud>()?;
    m.add_class::<PyPointCloudFile>()?;
    m.add_function(wrap_pyfunction!(open_points, m)?)?;
    m.add_function(wrap_pyfunction!(read_points, m)?)?;
    m.add_function(wrap_pyfunction!(read_points_bytes, m)?)?;
    m.add_function(wrap_pyfunction!(convert_points, m)?)?;
    Ok(())
}
