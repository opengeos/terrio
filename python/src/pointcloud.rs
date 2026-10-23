//! The `PointCloud`, `PointCloudFile` and `PointCloudWriter` classes: LAS, LAZ, COPC, EPT, PLY
//! and E57 point clouds.

use crate::{PyResultExt, crs_string, invalid};
use numpy::{PyArray1, PyArrayDescrMethods, PyArrayDyn, PyArrayMethods, PyUntypedArray, PyUntypedArrayMethods};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use terrio::crs::SpatialRef;
use terrio::pointcloud::filters::{
    self, AssignOptions, BandDimension, ClusterOptions, CovarianceFeaturesOptions, CropBox, CropOptions, CsfOptions, DbscanOptions,
    DimRange, EigenvaluesOptions, FeatureMode, HagOptions, Neighbourhood, NnDistanceMode, NormalOptions, OutlierMethod, OutlierOptions,
    OverlayOptions, PmfOptions, SampleOptions, SmrfOptions, SplitterOptions, VoxelMode,
};
use terrio::pointcloud::grid::{GridMethod, GridOptions, GridStat};
use terrio::pointcloud::{
    self, Dimension, EptDataType, PlyEncoding, PointCloud, PointCloudReader, PointFilter, PointFormat, PointReadOptions, PointWriteOptions,
    PointWriter, ReturnKind, Values,
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

/// An open point cloud (LAS, LAZ, COPC, EPT, PLY or E57): its header is read, its points on
/// demand, whole (`read`) or in chunks (`chunks`).
///
/// Remote COPC files and EPT datasets are read in place: `read` fetches only the octree nodes
/// that intersect the bounds asked for, down to the level asked for.
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

/// Opens a point cloud, reading only its header.
///
/// Args:
///     path: A LAS, LAZ, COPC, PLY or E57 file, or an EPT dataset's `ept.json` (or an `ept://`
///         path): a path or URL (`https://`, `s3://`, `gs://`, `az://`), or a zip archive
///         member. Remote COPC files and EPT datasets are read in place; other remote files are
///         downloaded.
///
/// Returns:
///     PointCloudFile: The open file.
#[pyfunction]
pub(crate) fn open_points(py: Python<'_>, path: &str) -> PyResult<PyPointCloudFile> {
    let inner = py.detach(|| pointcloud::open_points(path)).py()?;
    Ok(PyPointCloudFile { inner })
}

/// Reads the points of a point cloud (LAS, LAZ, COPC, EPT, PLY or E57).
///
/// Args:
///     path: The file's path or URL, as for `open_points`.
///     bounds: Keep the points in `(min_x, min_y, max_x, max_y)`, in the file's CRS.
///     z_range: Keep the points with `z` in `(min, max)`.
///     classes: Keep the points of these classifications.
///     returns: Keep these returns of each pulse: `"first"`, `"last"`, `"intermediate"`,
///         `"only"`.
///     max_level: COPC and EPT: the deepest octree level to read (0 is the root).
///     resolution: COPC and EPT: read the levels needed for this point spacing.
///     max_points: At most about this many points: COPC and EPT datasets are read down to the
///         deepest level that fits, other files are thinned by keeping every n-th point.
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

/// Reads a LAS, LAZ, COPC, PLY or E57 file held in memory.
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

/// Converts a point cloud to LAS, LAZ, COPC, PLY, E57 or a vector point layer, filtering and
/// reprojecting on the way, as `terrio convert` does. Point cloud outputs other than E57 are
/// written as the points are read, in chunks: files larger than memory convert in bounded
/// memory.
///
/// Args:
///     src: The input file's path or URL.
///     dst: The output path: `.las`, `.laz`, `.copc.laz`, `.ply`, `.e57`, an EPT dataset's
///         `ept.json` (its directory is written), or a vector format
///         (`.gpkg`, `.fgb`, `.parquet`, `.geojson`, `.shp`...) for a layer of points.
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
///     dimensions: For vector, PLY and E57 outputs, the dimensions written (all by default).
///     chunk_points: The points read and written at a time (about a million by default).
///     ply_encoding: PLY outputs: `"ascii"` (the default), `"binary_little_endian"` or
///         `"binary_big_endian"`.
///     ply_precision: PLY ASCII outputs: the digits after the decimal point of floats.
///     ept_data_type: EPT outputs: how the nodes' points are stored, `"laszip"` (the default),
///         `"binary"` or `"zstandard"`.
///     ept_bounds: EPT outputs: build the octree's cube around these bounds,
///         `(min_x, min_y, min_z, max_x, max_y, max_z)`, rather than the points' (as Entwine's
///         `--bounds`), so that points of other files within them can be appended later.
///     append: Add the points to the existing EPT dataset `dst` rather than write a new one
///         (see `append_points`).
#[pyfunction]
#[pyo3(signature = (
    src, dst, *, bounds=None, z_range=None, classes=None, returns=None, max_level=None, resolution=None, max_points=None,
    a_srs=None, t_srs=None, point_format=None, minor_version=None, scale=None, offset=None, dimensions=None,
    chunk_points=0, ply_encoding=None, ply_precision=None, ept_data_type=None, ept_bounds=None, append=false
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
    chunk_points: usize,
    ply_encoding: Option<&str>,
    ply_precision: Option<usize>,
    ept_data_type: Option<&str>,
    ept_bounds: Option<[f64; 6]>,
    append: bool,
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
            ply_encoding: ply_encoding.map(PlyEncoding::parse).transpose().py()?.unwrap_or_default(),
            ply_precision,
            dimensions: dimensions.clone().unwrap_or_default(),
            ept_data_type: ept_data_type.map(EptDataType::parse).transpose().py()?.unwrap_or_default(),
            ept_bounds,
            ..Default::default()
        },
        layer: pointcloud::vector::ToLayerOptions { dimensions: dimensions.unwrap_or_default(), ..Default::default() },
        chunk_points,
        append,
    };
    py.detach(|| pointcloud::convert(src, dst, &options)).py()
}

/// Appends the points of point clouds to an existing EPT dataset, in place, as Entwine does
/// when it builds into an existing output.
///
/// The points go into the dataset's octree, whose cube stays: points outside it are refused
/// (and the dataset left as it was). Each input becomes an entry of the dataset's source list
/// (`ept-sources/manifest.json`). The points take the dataset's schema: dimensions it has and
/// the points do not are written as zeros, those it does not have are dropped (with a
/// warning). The update is written beside the dataset and moved into place once complete,
/// `ept.json` last; an append interrupted while the files are moved in is completed by the next
/// one.
///
/// Args:
///     dataset: The EPT dataset: its `ept.json`, or its directory.
///     inputs: The point clouds to add (LAS, LAZ, COPC, EPT, PLY, E57; paths or URLs), read in
///         chunks.
///     bounds: Add the points in `(min_x, min_y, max_x, max_y)` only, in the inputs' CRS.
///     z_range: Add the points with `z` in `(min, max)` only.
///     classes: Add the points of these classifications only.
///     returns: Add these returns only: `"first"`, `"last"`, `"intermediate"`, `"only"`.
///     a_srs: Assign this CRS to the inputs, without reprojecting.
///     t_srs: Reproject the inputs' `x` and `y` into this CRS (the dataset's).
///     chunk_points: The points read at a time (about a million by default).
///     leaf_points: The most points of a leaf node before it is split (100 000 by default).
///     memory_points: The most points held in memory at a time (4 000 000 by default).
#[pyfunction]
#[pyo3(signature = (
    dataset, inputs, *, bounds=None, z_range=None, classes=None, returns=None, a_srs=None, t_srs=None, chunk_points=0,
    leaf_points=None, memory_points=None
))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn append_points(
    py: Python<'_>,
    dataset: &str,
    inputs: Vec<String>,
    bounds: Option<(f64, f64, f64, f64)>,
    z_range: Option<(f64, f64)>,
    classes: Option<Vec<u8>>,
    returns: Option<Vec<String>>,
    a_srs: Option<&str>,
    t_srs: Option<&str>,
    chunk_points: usize,
    leaf_points: Option<u64>,
    memory_points: Option<u64>,
) -> PyResult<()> {
    let options = pointcloud::ConvertOptions {
        read: read_options(bounds, z_range, classes, returns, None, None, None)?,
        assign_srs: a_srs.map(SpatialRef::parse).transpose().py()?,
        target_srs: t_srs.map(SpatialRef::parse).transpose().py()?,
        write: PointWriteOptions { copc_leaf_points: leaf_points, copc_memory_points: memory_points, ..Default::default() },
        chunk_points,
        append: true,
        ..Default::default()
    };
    let inputs: Vec<&str> = inputs.iter().map(String::as_str).collect();
    py.detach(|| pointcloud::append_points(dataset, &inputs, &options)).py()
}

#[pymethods]
impl PyPointCloudFile {
    /// `"LAS"`, `"LAZ"`, `"COPC"`, `"EPT"`, `"PLY"` or `"E57"`.
    #[getter]
    fn format(&self) -> &'static str {
        self.inner.format_name()
    }

    /// The metadata of an EPT dataset (bounds, data type, span, schema...), or `None`.
    #[getter]
    fn ept_info<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
        self.inner.ept_info().map(|i| to_python(py, i)).transpose()
    }

    /// The wave packet descriptors of a LAS file with waveforms (point formats 4, 5, 9 and 10):
    /// one dict per descriptor, with its `index`, `bits_per_sample`, `number_of_samples`,
    /// `temporal_sample_spacing` (picoseconds), `digitizer_gain` and `digitizer_offset`.
    #[getter]
    fn wave_packet_descriptors<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        to_python(py, &self.inner.wave_packet_descriptors())
    }

    /// The waveform of a point, from the waveform data packets in the file or its `.wdp` file.
    ///
    /// Args:
    ///     index: The point's `wavepacket_index`.
    ///     offset: The point's `wavepacket_offset`.
    ///     size: The point's `wavepacket_size`.
    ///
    /// Returns:
    ///     numpy.ndarray: The samples, as digitized (uint32); volts are
    ///         `digitizer_offset + digitizer_gain * samples`.
    fn waveform<'py>(&mut self, py: Python<'py>, index: u8, offset: u64, size: u32) -> PyResult<Bound<'py, PyAny>> {
        let w = py.detach(|| self.inner.waveform(index, offset, size)).py()?;
        Ok(PyArray1::from_vec(py, w.samples).into_any())
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

    /// Reads the points in chunks, for point clouds larger than memory.
    ///
    /// Args:
    ///     chunk_points: About how many points each chunk has (about a million by default).
    ///     bounds: Keep the points in `(min_x, min_y, max_x, max_y)`, in the file's CRS.
    ///     z_range: Keep the points with `z` in `(min, max)`.
    ///     classes: Keep the points of these classifications.
    ///     returns: Keep these returns: `"first"`, `"last"`, `"intermediate"`, `"only"`.
    ///     max_level: COPC and EPT: the deepest octree level to read.
    ///     resolution: COPC and EPT: read the levels needed for this point spacing.
    ///     max_points: At most about this many points.
    ///
    /// Returns:
    ///     PointChunks: An iterator of `PointCloud` chunks.
    #[pyo3(signature = (chunk_points=0, *, bounds=None, z_range=None, classes=None, returns=None, max_level=None, resolution=None, max_points=None))]
    #[allow(clippy::too_many_arguments)]
    fn chunks(
        slf: Bound<'_, Self>,
        chunk_points: usize,
        bounds: Option<(f64, f64, f64, f64)>,
        z_range: Option<(f64, f64)>,
        classes: Option<Vec<u8>>,
        returns: Option<Vec<String>>,
        max_level: Option<u32>,
        resolution: Option<f64>,
        max_points: Option<u64>,
    ) -> PyResult<PyPointChunks> {
        let options = read_options(bounds, z_range, classes, returns, max_level, resolution, max_points)?;
        {
            let mut f = slf.borrow_mut();
            let py = slf.py();
            let reader = &mut f.inner;
            py.detach(|| reader.start_chunks(&options, chunk_points)).py()?;
        }
        Ok(PyPointChunks { file: slf.unbind() })
    }

    /// A point cloud without points, with the file's header, CRS and dimensions: the template of
    /// a `PointCloudWriter`.
    ///
    /// Returns:
    ///     PointCloud: The template.
    fn template(&mut self, py: Python<'_>) -> PyResult<PyPointCloud> {
        Ok(PyPointCloud { inner: py.detach(|| self.inner.template()).py()? })
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

/// The chunks of points of a `PointCloudFile`: an iterator of `PointCloud`.
#[pyclass(module = "terrio", name = "PointChunks")]
pub struct PyPointChunks {
    file: Py<PyPointCloudFile>,
}

#[pymethods]
impl PyPointChunks {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&self, py: Python<'_>) -> PyResult<Option<PyPointCloud>> {
        let mut f = self.file.borrow_mut(py);
        let reader = &mut f.inner;
        let chunk = py.detach(|| reader.next_chunk()).py()?;
        Ok(chunk.map(|inner| PyPointCloud { inner }))
    }
}

/// Writes points as they come, in chunks, to a LAS, LAZ, COPC or PLY file: for point clouds
/// larger than memory. Use it as a context manager, or call `close`.
///
/// Args:
///     path: The output path: `.las`, `.laz`, `.copc.laz` or `.ply` (unless `format` says).
///     template: Points like the ones to write (their header, CRS and dimensions), such as
///         `PointCloudFile.template()` or the first chunk.
///     format: `"LAS"`, `"LAZ"`, `"COPC"` or `"PLY"`.
///     point_format: The point format (0 to 10).
///     minor_version: The LAS minor version.
///     scale: The scale of the stored coordinates `(x, y, z)`.
///     offset: The offset of the stored coordinates `(x, y, z)`.
#[pyclass(module = "terrio", name = "PointCloudWriter")]
pub struct PyPointCloudWriter {
    inner: Option<PointWriter>,
}

#[pymethods]
impl PyPointCloudWriter {
    #[new]
    #[pyo3(signature = (path, template, *, format=None, point_format=None, minor_version=None, scale=None, offset=None))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        path: &str,
        template: PyRef<'_, PyPointCloud>,
        format: Option<&str>,
        point_format: Option<u8>,
        minor_version: Option<u8>,
        scale: Option<(f64, f64, f64)>,
        offset: Option<(f64, f64, f64)>,
    ) -> PyResult<Self> {
        let options = write_options(format, point_format, minor_version, scale, offset)?;
        let t = &template.inner;
        let inner = py.detach(|| PointWriter::create(path, t, &options)).py()?;
        Ok(Self { inner: Some(inner) })
    }

    /// Writes points.
    ///
    /// Args:
    ///     points: The points.
    fn write(&mut self, py: Python<'_>, points: PyRef<'_, PyPointCloud>) -> PyResult<()> {
        let w = self.inner.as_mut().ok_or_else(|| invalid("the writer is closed"))?;
        let c = &points.inner;
        py.detach(|| w.write(c)).py()
    }

    /// Finishes the file (its counts, bounds and, for COPC, octree).
    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        match self.inner.take() {
            Some(w) => py.detach(|| w.finish()).py(),
            None => Ok(()),
        }
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&mut self, py: Python<'_>, _args: &Bound<'_, pyo3::types::PyTuple>) -> PyResult<bool> {
        self.close(py)?;
        Ok(false)
    }
}

/// Grids a point cloud read in chunks: for the statistics, in bounded memory, whatever its size.
///
/// Args:
///     path: The point cloud, as for `open_points`.
///     resolution: The cell size.
///     method: `"statistics"` (PDAL's `writers.gdal`, the default), `"linear"` (a TIN, as
///         `gdal_grid -a linear`) or `"nearest"` (as `gdal_grid -a nearest`).
///     stat: The statistics, as for `PointCloud.grid`.
///     dimension: The dimension gridded. `"z"` by default.
///     radius: As for `PointCloud.grid`.
///     power: The power of the inverse distance weights.
///     window_size: Fill empty cells from the cells within this many cells.
///     bin_mode: Count each point in its own cell only.
///     bounds: The grid `(min_x, min_y, max_x, max_y)`. By default the points' bounds.
///     nodata: The nodata value.
///     ot: The output data type.
///     classes: Grid the points of these classifications.
///     returns: Grid these returns.
///     a_srs: Assign this CRS to the points.
///
/// Returns:
///     Raster: The grid.
#[pyfunction]
#[pyo3(signature = (path, resolution, *, method="statistics", stat=None, dimension="z", radius=None, power=1.0, window_size=0, bin_mode=false, bounds=None, nodata=-9999.0, ot=None, classes=None, returns=None, a_srs=None))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn grid_points(
    py: Python<'_>,
    path: &str,
    resolution: f64,
    method: &str,
    stat: Option<&Bound<'_, PyAny>>,
    dimension: &str,
    radius: Option<f64>,
    power: f64,
    window_size: usize,
    bin_mode: bool,
    bounds: Option<(f64, f64, f64, f64)>,
    nodata: f64,
    ot: Option<&str>,
    classes: Option<Vec<u8>>,
    returns: Option<Vec<String>>,
    a_srs: Option<&str>,
) -> PyResult<crate::raster::PyRaster> {
    let options = grid_options(resolution, method, stat, dimension, radius, power, window_size, bin_mode, bounds, nodata, ot)?;
    let read = read_options(None, None, classes, returns, None, None, None)?;
    let srs = a_srs.map(SpatialRef::parse).transpose().py()?;
    let ds = py
        .detach(|| {
            let mut r = pointcloud::open_points(path)?;
            pointcloud::grid::grid_reader(&mut r, &read, &options, srs.as_ref())
        })
        .py()?;
    Ok(crate::raster::PyRaster::from_dataset(ds))
}

#[allow(clippy::too_many_arguments)]
fn grid_options(
    resolution: f64,
    method: &str,
    stat: Option<&Bound<'_, PyAny>>,
    dimension: &str,
    radius: Option<f64>,
    power: f64,
    window_size: usize,
    bin_mode: bool,
    bounds: Option<(f64, f64, f64, f64)>,
    nodata: f64,
    ot: Option<&str>,
) -> PyResult<GridOptions> {
    let stats = match stat {
        None => Vec::new(),
        Some(s) => match s.extract::<String>() {
            Ok(s) => GridStat::parse_list(&s).py()?,
            Err(_) => s.extract::<Vec<String>>()?.iter().map(|s| GridStat::parse(s)).collect::<terrio::Result<_>>().py()?,
        },
    };
    Ok(GridOptions {
        resolution,
        method: GridMethod::parse(method).py()?,
        stats,
        dimension: dimension.to_string(),
        radius,
        power,
        window_size,
        bin_mode,
        bounds: bounds.map(|b| [b.0, b.1, b.2, b.3]),
        nodata,
        data_type: ot.map(terrio::ops::parse_data_type).transpose().py()?.unwrap_or(terrio::DataType::F64),
    })
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

    /// Writes the points to a LAS, LAZ, COPC, PLY or E57 file.
    ///
    /// Args:
    ///     path: The output path: `.las`, `.laz`, `.copc.laz`, `.ply` or `.e57` (unless `format`
    ///         says).
    ///     format: `"LAS"`, `"LAZ"`, `"COPC"`, `"PLY"` or `"E57"`.
    ///     point_format: The point format (0 to 10). By default, the source's (6 to 8 for COPC).
    ///     minor_version: The LAS minor version. By default, the source's, raised as the point
    ///         format needs.
    ///     scale: The scale of the stored coordinates `(x, y, z)`.
    ///     offset: The offset of the stored coordinates `(x, y, z)`.
    ///     ply_encoding: PLY: `"ascii"` (the default), `"binary_little_endian"` or
    ///         `"binary_big_endian"`.
    ///     ply_precision: PLY, ASCII: the digits after the decimal point of floats (by default,
    ///         as many as needed to read the same values back).
    ///     dimensions: PLY and E57: the dimensions written (all by default).
    ///     ept_data_type: EPT (a path ending with `ept.json`): how the nodes' points are stored,
    ///         `"laszip"` (the default), `"binary"` or `"zstandard"`.
    ///     ept_bounds: EPT: build the octree's cube around these bounds,
    ///         `(min_x, min_y, min_z, max_x, max_y, max_z)`, rather than the points', so that
    ///         points of other files within them can be appended later (`append_points`).
    #[pyo3(signature = (path, *, format=None, point_format=None, minor_version=None, scale=None, offset=None, ply_encoding=None, ply_precision=None, dimensions=None, ept_data_type=None, ept_bounds=None))]
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
        ply_encoding: Option<&str>,
        ply_precision: Option<usize>,
        dimensions: Option<Vec<String>>,
        ept_data_type: Option<&str>,
        ept_bounds: Option<[f64; 6]>,
    ) -> PyResult<()> {
        let mut options = write_options(format, point_format, minor_version, scale, offset)?;
        options.ept_data_type = ept_data_type.map(EptDataType::parse).transpose().py()?.unwrap_or_default();
        options.ept_bounds = ept_bounds;
        options.ply_encoding = ply_encoding.map(PlyEncoding::parse).transpose().py()?.unwrap_or_default();
        options.ply_precision = ply_precision;
        options.dimensions = dimensions.unwrap_or_default();
        py.detach(|| pointcloud::write_points(&self.inner, path, &options)).py()
    }

    /// The points as the bytes of a LAS, LAZ, COPC, PLY or E57 file.
    ///
    /// Args:
    ///     format: `"LAS"`, `"LAZ"`, `"COPC"`, `"PLY"` or `"E57"`.
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

    /// Grids the points into a raster: statistics per cell as PDAL's `writers.gdal` does, or
    /// interpolated as `gdal_grid` does.
    ///
    /// Args:
    ///     resolution: The cell size.
    ///     method: `"statistics"` (the default), `"linear"` (linear interpolation in the
    ///         points' Delaunay triangulation, a TIN, as `gdal_grid -a linear`) or `"nearest"`
    ///         (the nearest point's value, as `gdal_grid -a nearest`).
    ///     stat: The statistics, one band each: `"min"`, `"max"`, `"mean"`, `"idw"`, `"count"`,
    ///         `"stdev"`, a list of them, or `"all"`. `"mean"` by default.
    ///     dimension: The dimension gridded. `"z"` by default.
    ///     radius: Statistics: points closer than this to a cell centre count (by default the
    ///         cell diagonal). Linear: the search radius of the nearest point outside the
    ///         triangulation (negative: none, the default; 0: nodata). Nearest: the half-side
    ///         of the square searched (all points by default).
    ///     power: The power of the inverse distance weights.
    ///     window_size: Fill empty cells from the cells within this many cells.
    ///     bin_mode: Count each point in its own cell only.
    ///     bounds: The grid `(min_x, min_y, max_x, max_y)`. By default the points' bounds.
    ///     nodata: The nodata value.
    ///     ot: The output data type (`"Float32"`, `"Int16"`...). `"Float64"` by default.
    ///
    /// Returns:
    ///     Raster: The grid.
    #[pyo3(signature = (resolution, *, method="statistics", stat=None, dimension="z", radius=None, power=1.0, window_size=0, bin_mode=false, bounds=None, nodata=-9999.0, ot=None))]
    #[allow(clippy::too_many_arguments)]
    fn grid(
        &self,
        py: Python<'_>,
        resolution: f64,
        method: &str,
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
        let options = grid_options(resolution, method, stat, dimension, radius, power, window_size, bin_mode, bounds, nodata, ot)?;
        let ds = py.detach(|| pointcloud::grid::grid(&self.inner, &options)).py()?;
        Ok(crate::raster::PyRaster::from_dataset(ds))
    }

    /// The points within ranges of dimensions, as PDAL's `filters.range`.
    ///
    /// Args:
    ///     limits: Ranges as PDAL writes them, comma-separated: `"Classification[2:2]"`,
    ///         `"Z[0:100), Intensity[:500]"`, `"Classification![7:7]"`. A point passes when it is
    ///         in any range of each dimension named.
    ///
    /// Returns:
    ///     PointCloud: The points that pass.
    fn range(&self, limits: &str) -> PyResult<Self> {
        let ranges = DimRange::parse_list(limits).py()?;
        Ok(Self { inner: filters::range(&self.inner, &ranges).py()? })
    }

    /// Poisson sampling, as PDAL's `filters.sample`: keeps each point, in order, at least
    /// `radius` from every point kept before it.
    ///
    /// Args:
    ///     radius: The smallest distance between points kept.
    ///     cell: Or the voxel size (the radius is then `cell / 2 * sqrt(3)`).
    ///     origin: The voxels' origin `(x, y, z)`. By default, the first point's.
    ///
    /// Returns:
    ///     PointCloud: The points kept.
    #[pyo3(signature = (radius=None, *, cell=None, origin=None))]
    fn sample(&self, py: Python<'_>, radius: Option<f64>, cell: Option<f64>, origin: Option<(f64, f64, f64)>) -> PyResult<Self> {
        let o = SampleOptions { cell, radius, origin: origin.map(|o| [o.0, o.1, o.2]) };
        Ok(Self { inner: py.detach(|| filters::sample(&self.inner, &o)).py()? })
    }

    /// The point nearest the center of each voxel, as PDAL's
    /// `filters.voxelcenternearestneighbor`.
    ///
    /// Args:
    ///     cell: The voxel size. 1 by default.
    ///
    /// Returns:
    ///     PointCloud: The points kept.
    #[pyo3(signature = (cell=1.0))]
    fn voxel_center_nearest_neighbor(&self, py: Python<'_>, cell: f64) -> PyResult<Self> {
        Ok(Self { inner: py.detach(|| filters::voxel_center_nearest_neighbor(&self.inner, cell)).py()? })
    }

    /// The first point of each voxel, as PDAL's `filters.voxeldownsize`.
    ///
    /// Args:
    ///     cell: The voxel size. 0.001 by default.
    ///     mode: `"center"` (the default: the point moves to the voxel's center) or `"first"`.
    ///
    /// Returns:
    ///     PointCloud: The points kept.
    #[pyo3(signature = (cell=0.001, mode="center"))]
    fn voxel_downsize(&self, py: Python<'_>, cell: f64, mode: &str) -> PyResult<Self> {
        let mode = VoxelMode::parse(mode).py()?;
        Ok(Self { inner: py.detach(|| filters::voxel_downsize(&self.inner, cell, mode)).py()? })
    }

    /// Classifies outliers as noise, as PDAL's `filters.outlier`: all points are kept, outliers
    /// with the classification `classification`.
    ///
    /// Args:
    ///     method: `"statistical"` (the default) or `"radius"`.
    ///     min_k: Radius: the fewest neighbours within `radius` to be kept. 2 by default.
    ///     radius: Radius: the radius. 1 by default.
    ///     mean_k: Statistical: the number of neighbours. 8 by default.
    ///     multiplier: Statistical: the number of standard deviations. 2 by default.
    ///     classification: The class of outliers. 7 (low point, noise) by default.
    ///
    /// Returns:
    ///     PointCloud: The points, outliers classified.
    #[pyo3(signature = (method="statistical", *, min_k=2, radius=1.0, mean_k=8, multiplier=2.0, classification=7))]
    #[allow(clippy::too_many_arguments)]
    fn outlier(
        &self,
        py: Python<'_>,
        method: &str,
        min_k: usize,
        radius: f64,
        mean_k: usize,
        multiplier: f64,
        classification: u8,
    ) -> PyResult<Self> {
        let o = OutlierOptions { method: OutlierMethod::parse(method).py()?, min_k, radius, mean_k, multiplier, class: classification };
        Ok(Self { inner: py.detach(|| filters::outlier(&self.inner, &o)).py()? })
    }

    /// Classifies ground points with the Simple Morphological Filter, as PDAL's `filters.smrf`:
    /// of the returns selected, ground points become class 2 and others class 1; other points
    /// keep their class.
    ///
    /// Args:
    ///     cell: The cell size. 1 by default.
    ///     slope: The slope (rise over run). 0.15 by default.
    ///     window: The largest window, in CRS units. 18 cells by default.
    ///     scalar: The elevation scalar. 1.25 by default.
    ///     threshold: The elevation threshold. 0.5 by default.
    ///     cut: The cut net size (0, the default: no net cutting).
    ///     ignore: Leave alone the points in these ranges (as for `range`).
    ///     returns: The returns classified. `["last", "only"]` by default.
    ///     ground_class: The class of ground points. 2 by default.
    ///     other_class: The class of other points. 1 by default.
    ///     only_ground: Change the class of ground points only.
    ///
    /// Returns:
    ///     PointCloud: The points, classified.
    #[pyo3(signature = (*, cell=1.0, slope=0.15, window=None, scalar=1.25, threshold=0.5, cut=0.0, ignore=None, returns=None, ground_class=2, other_class=1, only_ground=false))]
    #[allow(clippy::too_many_arguments)]
    fn smrf(
        &self,
        py: Python<'_>,
        cell: f64,
        slope: f64,
        window: Option<f64>,
        scalar: f64,
        threshold: f64,
        cut: f64,
        ignore: Option<&str>,
        returns: Option<Vec<String>>,
        ground_class: u8,
        other_class: u8,
        only_ground: bool,
    ) -> PyResult<Self> {
        let returns = match returns {
            None => vec![ReturnKind::Last, ReturnKind::Only],
            Some(r) => r.iter().map(|k| ReturnKind::parse(k)).collect::<terrio::Result<_>>().py()?,
        };
        let o = SmrfOptions {
            cell,
            slope,
            window,
            scalar,
            threshold,
            cut,
            ignore: ignore.map(DimRange::parse_list).transpose().py()?.unwrap_or_default(),
            returns,
            ground_class,
            other_class,
            only_ground,
        };
        Ok(Self { inner: py.detach(|| filters::smrf(&self.inner, &o)).py()? })
    }

    /// Classifies ground points with the Progressive Morphological Filter, as PDAL's
    /// `filters.pmf`: of the returns selected, ground points become class 2 and others class 1.
    ///
    /// Args:
    ///     cell_size: The cell size. 1 by default.
    ///     exponential: Grow the windows exponentially (the default) rather than linearly.
    ///     initial_distance: The initial height threshold. 0.15 by default.
    ///     max_distance: The largest height threshold. 2.5 by default.
    ///     max_window_size: The largest window. 33 by default.
    ///     slope: The slope. 1 by default.
    ///     ignore: Leave alone the points in these ranges (as for `range`).
    ///     returns: The returns classified. `["last", "only"]` by default.
    ///     ground_class: The class of ground points. 2 by default.
    ///     other_class: The class of other points. 1 by default.
    ///     only_ground: Change the class of ground points only.
    ///
    /// Returns:
    ///     PointCloud: The points, classified.
    #[pyo3(signature = (*, cell_size=1.0, exponential=true, initial_distance=0.15, max_distance=2.5, max_window_size=33.0, slope=1.0, ignore=None, returns=None, ground_class=2, other_class=1, only_ground=false))]
    #[allow(clippy::too_many_arguments)]
    fn pmf(
        &self,
        py: Python<'_>,
        cell_size: f64,
        exponential: bool,
        initial_distance: f64,
        max_distance: f64,
        max_window_size: f64,
        slope: f64,
        ignore: Option<&str>,
        returns: Option<Vec<String>>,
        ground_class: u8,
        other_class: u8,
        only_ground: bool,
    ) -> PyResult<Self> {
        let returns = match returns {
            None => vec![ReturnKind::Last, ReturnKind::Only],
            Some(r) => r.iter().map(|k| ReturnKind::parse(k)).collect::<terrio::Result<_>>().py()?,
        };
        let o = PmfOptions {
            cell: cell_size,
            exponential,
            initial_distance,
            max_distance,
            max_window: max_window_size,
            slope,
            ignore: ignore.map(DimRange::parse_list).transpose().py()?.unwrap_or_default(),
            returns,
            ground_class,
            other_class,
            only_ground,
        };
        Ok(Self { inner: py.detach(|| filters::pmf(&self.inner, &o)).py()? })
    }

    /// Classifies ground points with the Cloth Simulation Filter, as PDAL's `filters.csf`: of
    /// the returns selected, ground points become class 2 and others class 1.
    ///
    /// Args:
    ///     resolution: The cloth's resolution. 1 by default.
    ///     rigidness: The cloth's rigidness (1 to 3). 3 by default.
    ///     step: The time step. 0.65 by default.
    ///     threshold: The largest distance of ground points from the cloth. 0.5 by default.
    ///     hdiff: The height difference threshold of slope handling. 0.3 by default.
    ///     iterations: The most simulation steps. 500 by default.
    ///     smooth: Handle steep slopes after the simulation. True by default.
    ///     ignore: Leave alone the points in these ranges (as for `range`).
    ///     returns: The returns classified. `["last", "only"]` by default.
    ///     ground_class: The class of ground points. 2 by default.
    ///     other_class: The class of other points. 1 by default.
    ///     only_ground: Change the class of ground points only.
    ///
    /// Returns:
    ///     PointCloud: The points, classified.
    #[pyo3(signature = (*, resolution=1.0, rigidness=3, step=0.65, threshold=0.5, hdiff=0.3, iterations=500, smooth=true, ignore=None, returns=None, ground_class=2, other_class=1, only_ground=false))]
    #[allow(clippy::too_many_arguments)]
    fn csf(
        &self,
        py: Python<'_>,
        resolution: f64,
        rigidness: usize,
        step: f64,
        threshold: f64,
        hdiff: f64,
        iterations: usize,
        smooth: bool,
        ignore: Option<&str>,
        returns: Option<Vec<String>>,
        ground_class: u8,
        other_class: u8,
        only_ground: bool,
    ) -> PyResult<Self> {
        let returns = match returns {
            None => vec![ReturnKind::Last, ReturnKind::Only],
            Some(r) => r.iter().map(|k| ReturnKind::parse(k)).collect::<terrio::Result<_>>().py()?,
        };
        let o = CsfOptions {
            smooth,
            step,
            threshold,
            hdiff,
            resolution,
            rigidness,
            iterations,
            ignore: ignore.map(DimRange::parse_list).transpose().py()?.unwrap_or_default(),
            returns,
            ground_class,
            other_class,
            only_ground,
        };
        Ok(Self { inner: py.detach(|| filters::csf(&self.inner, &o)).py()? })
    }

    /// Classifies low noise with the Extended Local Minimum, as PDAL's `filters.elm`.
    ///
    /// Args:
    ///     cell: The cell size. 10 by default.
    ///     threshold: The elevation difference above which the lower point is noise. 1 by
    ///         default.
    ///     classification: The class of noise points. 7 (low point) by default.
    ///
    /// Returns:
    ///     PointCloud: The points, low noise classified.
    #[pyo3(signature = (*, cell=10.0, threshold=1.0, classification=7))]
    fn elm(&self, py: Python<'_>, cell: f64, threshold: f64, classification: u8) -> PyResult<Self> {
        Ok(Self { inner: py.detach(|| filters::elm(&self.inner, cell, threshold, classification)).py()? })
    }

    /// Heights above the nearest ground points, as PDAL's `filters.hag_nn`: the
    /// `height_above_ground` dimension.
    ///
    /// Args:
    ///     count: The ground points used for each point (inverse-distance weighted). 1 by
    ///         default.
    ///     max_distance: Ground points farther than this are not used (0, the default: no
    ///         limit).
    ///     allow_extrapolation: Interpolate outside the ground points' bounds (else heights
    ///         there are 0). True by default.
    ///     ground_class: The class of ground points. 2 by default.
    ///
    /// Returns:
    ///     PointCloud: The points, with their heights above ground.
    #[pyo3(signature = (*, count=1, max_distance=0.0, allow_extrapolation=true, ground_class=2))]
    fn hag_nn(&self, py: Python<'_>, count: usize, max_distance: f64, allow_extrapolation: bool, ground_class: u8) -> PyResult<Self> {
        let o = HagOptions { count, max_distance, allow_extrapolation, class: ground_class };
        Ok(Self { inner: py.detach(|| filters::hag_nn(&self.inner, &o)).py()? })
    }

    /// Heights above a Delaunay triangulation of the nearest ground points, as PDAL's
    /// `filters.hag_delaunay`: the `height_above_ground` dimension.
    ///
    /// Args:
    ///     count: The ground points triangulated for each point. 10 by default.
    ///     allow_extrapolation: Interpolate outside the ground points' bounds (else heights
    ///         there are 0). True by default.
    ///     ground_class: The class of ground points. 2 by default.
    ///
    /// Returns:
    ///     PointCloud: The points, with their heights above ground.
    #[pyo3(signature = (*, count=10, allow_extrapolation=true, ground_class=2))]
    fn hag_delaunay(&self, py: Python<'_>, count: usize, allow_extrapolation: bool, ground_class: u8) -> PyResult<Self> {
        let o = HagOptions { count, allow_extrapolation, class: ground_class, ..HagOptions::delaunay() };
        Ok(Self { inner: py.detach(|| filters::hag_delaunay(&self.inner, &o)).py()? })
    }

    /// The points in boxes, polygons, circles or spheres, as PDAL's `filters.crop`: for each
    /// geometry in turn (polygons, then boxes, then circles and spheres), the points inside it.
    ///
    /// Args:
    ///     bounds: Boxes as PDAL writes them: `"([xmin, xmax], [ymin, ymax])"`, with
    ///         `[zmin, zmax]` for 3D.
    ///     polygons: Polygons or multipolygons, as WKT or GeoJSON.
    ///     points: Centres of circles `(x, y)` or spheres `(x, y, z)` of radius `distance`.
    ///     distance: The radius of the circles and spheres.
    ///     outside: Keep the points outside the geometries instead.
    ///     a_srs: The CRS of the geometries, when it is not the points'.
    ///
    /// Returns:
    ///     PointCloud: The points kept.
    #[pyo3(signature = (*, bounds=None, polygons=None, points=None, distance=0.0, outside=false, a_srs=None))]
    #[allow(clippy::too_many_arguments)]
    fn crop(
        &self,
        py: Python<'_>,
        bounds: Option<Vec<String>>,
        polygons: Option<Vec<String>>,
        points: Option<Vec<Vec<f64>>>,
        distance: f64,
        outside: bool,
        a_srs: Option<&str>,
    ) -> PyResult<Self> {
        let o = CropOptions {
            bounds: bounds.unwrap_or_default().iter().map(|b| CropBox::parse(b)).collect::<terrio::Result<_>>().py()?,
            polygons: polygons.unwrap_or_default(),
            points: points.unwrap_or_default(),
            distance,
            outside,
            a_srs: a_srs.map(SpatialRef::parse).transpose().py()?,
        };
        Ok(Self { inner: py.detach(|| filters::crop(&self.inner, &o)).py()? })
    }

    /// Every `step`-th point, as PDAL's `filters.decimation`.
    ///
    /// Args:
    ///     step: The step (a fraction is allowed). 1 by default.
    ///     offset: The index of the first point considered.
    ///     limit: The index past the last point considered.
    ///
    /// Returns:
    ///     PointCloud: The points kept.
    #[pyo3(signature = (step=1.0, *, offset=0, limit=None))]
    fn decimation(&self, step: f64, offset: usize, limit: Option<usize>) -> PyResult<Self> {
        Ok(Self { inner: filters::decimation(&self.inner, step, offset, limit).py()? })
    }

    /// The points for which an expression is true, as PDAL's `filters.expression`.
    ///
    /// Args:
    ///     expression: A condition in PDAL's expression language, such as
    ///         `"Classification == 2 && Z > 100"`.
    ///
    /// Returns:
    ///     PointCloud: The points kept.
    fn expression(&self, py: Python<'_>, expression: String) -> PyResult<Self> {
        Ok(Self { inner: py.detach(|| filters::expression(&self.inner, &[expression])).py()? })
    }

    /// Assigns values to dimensions, as PDAL's `filters.assign`.
    ///
    /// Args:
    ///     assignment: Ranges with values: `"Classification[1:1]=2"` (or a list of them).
    ///     condition: Only change the points in this range, such as `"ReturnNumber[1:1]"`.
    ///     value: Expressions: `"Classification = 2 WHERE HeightAboveGround < 0.5"` (or a list
    ///         of them); a dimension the points lack is added.
    ///
    /// Returns:
    ///     PointCloud: The points, changed.
    #[pyo3(signature = (*, assignment=None, condition=None, value=None))]
    fn assign(
        &self,
        py: Python<'_>,
        assignment: Option<&Bound<'_, PyAny>>,
        condition: Option<&str>,
        value: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let o = AssignOptions {
            assignments: strings(assignment)?,
            condition: condition.map(DimRange::parse).transpose().py()?,
            values: strings(value)?,
        };
        Ok(Self { inner: py.detach(|| filters::assign(&self.inner, &o)).py()? })
    }

    /// Splits the points into squares, as PDAL's `filters.splitter`.
    ///
    /// Args:
    ///     length: The side of the squares. 1000 by default.
    ///     origin: The squares' origin `(x, y)`. By default, the first point's.
    ///     buffer: A buffer around each square whose points it takes too.
    ///
    /// Returns:
    ///     list[PointCloud]: The points of each square with points, in the order of their first
    ///     points.
    #[pyo3(signature = (length=1000.0, *, origin=None, buffer=0.0))]
    fn splitter(&self, py: Python<'_>, length: f64, origin: Option<(f64, f64)>, buffer: f64) -> PyResult<Vec<Self>> {
        let o = SplitterOptions { length, origin: origin.map(|o| [o.0, o.1]), buffer };
        let tiles = py.detach(|| filters::splitter(&self.inner, &o)).py()?;
        Ok(tiles.into_iter().map(|(_, c)| Self { inner: c }).collect())
    }

    /// Splits the points into squarish chips of at most `capacity` points, as PDAL's
    /// `filters.chipper`.
    ///
    /// Args:
    ///     capacity: The most points of a chip. 5000 by default.
    ///
    /// Returns:
    ///     list[PointCloud]: The chips, in PDAL's order.
    #[pyo3(signature = (capacity=5000))]
    fn chipper(&self, py: Python<'_>, capacity: usize) -> PyResult<Vec<Self>> {
        let chips = py.detach(|| filters::chipper(&self.inner, capacity)).py()?;
        Ok(chips.into_iter().map(|c| Self { inner: c }).collect())
    }

    /// Normals and curvature, as PDAL's `filters.normal`: `normal_x`, `normal_y`, `normal_z`
    /// and `curvature`.
    ///
    /// Args:
    ///     knn: The neighbours of each point. 8 by default.
    ///     viewpoint: Orient the normals towards this point `(x, y, z)`.
    ///     always_up: Orient the normals up (when there is no viewpoint). True by default.
    ///
    /// Returns:
    ///     PointCloud: The points, with their normals.
    #[pyo3(signature = (*, knn=8, viewpoint=None, always_up=true))]
    fn normal(&self, py: Python<'_>, knn: usize, viewpoint: Option<(f64, f64, f64)>, always_up: bool) -> PyResult<Self> {
        let o = NormalOptions { knn, viewpoint: viewpoint.map(|v| [v.0, v.1, v.2]), always_up };
        Ok(Self { inner: py.detach(|| filters::normal(&self.inner, &o)).py()? })
    }

    /// Labels Euclidean clusters, as PDAL's `filters.cluster`, in `cluster_id` (from 1; 0 for
    /// points of clusters too small or too large).
    ///
    /// Args:
    ///     tolerance: The distance within which points are neighbours. 1 by default.
    ///     min_points: The fewest points of a cluster. 1 by default.
    ///     max_points: The most points of a cluster. Unlimited by default.
    ///     is3d: Measure distances in 3D (the default) rather than in x and y.
    ///
    /// Returns:
    ///     PointCloud: The points, with their clusters.
    #[pyo3(signature = (tolerance=1.0, *, min_points=1, max_points=None, is3d=true))]
    fn cluster(&self, py: Python<'_>, tolerance: f64, min_points: u64, max_points: Option<u64>, is3d: bool) -> PyResult<Self> {
        let o = ClusterOptions { min_points, max_points: max_points.unwrap_or(u64::MAX), tolerance, is_3d: is3d };
        Ok(Self { inner: py.detach(|| filters::cluster(&self.inner, &o)).py()? })
    }

    /// Labels density-based clusters, as PDAL's `filters.dbscan`, in `cluster_id` (from 0; -1
    /// for noise).
    ///
    /// Args:
    ///     eps: The distance within which points are neighbours. 1 by default.
    ///     min_points: The fewest points (itself included) within `eps` of a core point. 6 by
    ///         default.
    ///     dimensions: The dimensions distances are measured in. `X`, `Y` and `Z` by default.
    ///
    /// Returns:
    ///     PointCloud: The points, with their clusters.
    #[pyo3(signature = (eps=1.0, *, min_points=6, dimensions=None))]
    fn dbscan(&self, py: Python<'_>, eps: f64, min_points: u64, dimensions: Option<Vec<String>>) -> PyResult<Self> {
        let o = DbscanOptions { min_points, eps, dimensions: dimensions.unwrap_or_else(|| DbscanOptions::default().dimensions) };
        Ok(Self { inner: py.detach(|| filters::dbscan(&self.inner, &o)).py()? })
    }

    /// Features of the covariance of each point's neighbours, as PDAL's
    /// `filters.covariancefeatures`: `linearity`, `planarity`, `scattering`, `verticality`,
    /// `omnivariance`, `anisotropy`, `eigenentropy`, `eigenvalue_sum`, `surface_variation`,
    /// `demantke_verticality` and `density`, as the feature set asks.
    ///
    /// Args:
    ///     knn: The neighbours of each point. 10 by default.
    ///     feature_set: `Dimensionality` (the default: linearity, planarity, scattering and
    ///         verticality), `All`, or feature names, as PDAL names them (or a list of them).
    ///     mode: How the eigenvalues are scaled: `raw`, `sqrt` (the default) or `normalized`.
    ///     stride: Take every `stride`-th of the nearest points. 1 by default.
    ///     radius: Take the points within this radius instead of the nearest.
    ///     min_k: With a radius, the fewest points of a neighbourhood computed. 3 by default.
    ///     optimized: Take each point's `optimal_knn` nearest points instead.
    ///
    /// Returns:
    ///     PointCloud: The points, with their features.
    #[pyo3(signature = (*, knn=10, feature_set=None, mode="sqrt", stride=1, radius=None, min_k=3, optimized=false))]
    #[allow(clippy::too_many_arguments)]
    fn covariance_features(
        &self,
        py: Python<'_>,
        knn: usize,
        feature_set: Option<&Bound<'_, PyAny>>,
        mode: &str,
        stride: usize,
        radius: Option<f64>,
        min_k: usize,
        optimized: bool,
    ) -> PyResult<Self> {
        let mut features = strings(feature_set)?;
        if features.is_empty() {
            features.push("Dimensionality".into());
        }
        let o = CovarianceFeaturesOptions {
            neighbours: Neighbourhood { knn, stride, radius, min_k },
            feature_set: features,
            mode: FeatureMode::parse(mode).py()?,
            optimized,
        };
        Ok(Self { inner: py.detach(|| filters::covariance_features(&self.inner, &o)).py()? })
    }

    /// The eigenvalues of the covariance of each point's neighbours, ascending, as PDAL's
    /// `filters.eigenvalues`: `eigenvalue0`, `eigenvalue1` and `eigenvalue2`.
    ///
    /// Args:
    ///     knn: The neighbours of each point. 8 by default.
    ///     normalize: Divide the eigenvalues by their sum.
    ///     stride: Take every `stride`-th of the nearest points. 1 by default.
    ///     radius: Take the points within this radius instead of the nearest.
    ///     min_k: With a radius, the fewest points of a neighbourhood computed. 3 by default.
    ///
    /// Returns:
    ///     PointCloud: The points, with their eigenvalues.
    #[pyo3(signature = (*, knn=8, normalize=false, stride=1, radius=None, min_k=3))]
    fn eigenvalues(&self, py: Python<'_>, knn: usize, normalize: bool, stride: usize, radius: Option<f64>, min_k: usize) -> PyResult<Self> {
        let o = EigenvaluesOptions { neighbours: Neighbourhood { knn, stride, radius, min_k }, normalize };
        Ok(Self { inner: py.detach(|| filters::eigenvalues(&self.inner, &o)).py()? })
    }

    /// Labels approximately coplanar points, as PDAL's `filters.approximatecoplanar`: 1 in
    /// `coplanar` where the eigenvalues of the covariance of the `knn` nearest points have
    /// `λ1 > thresh1 × λ0` and `thresh2 × λ1 > λ2`, else 0.
    ///
    /// Args:
    ///     knn: The nearest points (itself included) of each point. 8 by default.
    ///     thresh1: 25 by default.
    ///     thresh2: 6 by default.
    ///
    /// Returns:
    ///     PointCloud: The points, labeled.
    #[pyo3(signature = (*, knn=8, thresh1=25.0, thresh2=6.0))]
    fn approximate_coplanar(&self, py: Python<'_>, knn: usize, thresh1: f64, thresh2: f64) -> PyResult<Self> {
        Ok(Self { inner: py.detach(|| filters::approximate_coplanar(&self.inner, knn, thresh1, thresh2)).py()? })
    }

    /// The rank of each point's neighbourhood, as PDAL's `filters.estimaterank`, in `rank`: the
    /// singular values of the covariance of the `knn` nearest points at least `thresh` times the
    /// largest.
    ///
    /// Args:
    ///     knn: The nearest points (itself included) of each point. 8 by default.
    ///     thresh: 0.01 by default.
    ///
    /// Returns:
    ///     PointCloud: The points, with their ranks.
    #[pyo3(signature = (*, knn=8, thresh=0.01))]
    fn estimate_rank(&self, py: Python<'_>, knn: usize, thresh: f64) -> PyResult<Self> {
        Ok(Self { inner: py.detach(|| filters::estimate_rank(&self.inner, knn, thresh)).py()? })
    }

    /// Local outlier factors, as PDAL's `filters.lof`: `nndistance`,
    /// `local_reachability_distance` and `local_outlier_factor`.
    ///
    /// Args:
    ///     minpts: The neighbours of each point. 10 by default.
    ///
    /// Returns:
    ///     PointCloud: The points, with their factors.
    #[pyo3(signature = (minpts=10))]
    fn lof(&self, py: Python<'_>, minpts: usize) -> PyResult<Self> {
        Ok(Self { inner: py.detach(|| filters::lof(&self.inner, minpts)).py()? })
    }

    /// The distance to each point's nearest neighbours, as PDAL's `filters.nndistance`, in
    /// `nndistance`.
    ///
    /// Args:
    ///     k: The neighbours of each point. 10 by default.
    ///     mode: `kth` (the distance to the k-th, the default) or `avg` (the mean distance).
    ///
    /// Returns:
    ///     PointCloud: The points, with their distances.
    #[pyo3(signature = (k=10, *, mode="kth"))]
    fn nndistance(&self, py: Python<'_>, k: usize, mode: &str) -> PyResult<Self> {
        let mode = NnDistanceMode::parse(mode).py()?;
        Ok(Self { inner: py.detach(|| filters::nndistance(&self.inner, k, mode)).py()? })
    }

    /// The density of the points around each point, as PDAL's `filters.radialdensity`, in
    /// `radial_density`: the points within the radius over the sphere's volume.
    ///
    /// Args:
    ///     radius: 1 by default.
    ///
    /// Returns:
    ///     PointCloud: The points, with their densities.
    #[pyo3(signature = (radius=1.0))]
    fn radial_density(&self, py: Python<'_>, radius: f64) -> PyResult<Self> {
        Ok(Self { inner: py.detach(|| filters::radial_density(&self.inner, radius)).py()? })
    }

    /// The share of each point's nearest neighbours that do not have it among theirs, in
    /// percent, as PDAL's `filters.reciprocity`, in `reciprocity`.
    ///
    /// Args:
    ///     knn: The neighbours of each point. 8 by default.
    ///
    /// Returns:
    ///     PointCloud: The points, with their reciprocity.
    #[pyo3(signature = (knn=8))]
    fn reciprocity(&self, py: Python<'_>, knn: usize) -> PyResult<Self> {
        Ok(Self { inner: py.detach(|| filters::reciprocity(&self.inner, knn)).py()? })
    }

    /// How far each point is from the centre of the smallest ball enclosing its nearest
    /// neighbours, as PDAL's `filters.miniball`, in `miniball`.
    ///
    /// Args:
    ///     knn: The neighbours of each point. 8 by default.
    ///
    /// Returns:
    ///     PointCloud: The points, with their values.
    #[pyo3(signature = (knn=8))]
    fn miniball(&self, py: Python<'_>, knn: usize) -> PyResult<Self> {
        Ok(Self { inner: py.detach(|| filters::miniball(&self.inner, knn)).py()? })
    }

    /// Smoothed elevations, as PDAL's `filters.zsmooth`: a percentile of the `z` of the points
    /// within `radius` in x and y (the point itself excluded), in a dimension.
    ///
    /// Args:
    ///     dim: The dimension of the statistic (doubles).
    ///     radius: 1 by default.
    ///     medianpercent: 0 the lowest, 50 the median (the default), 100 the highest.
    ///
    /// Returns:
    ///     PointCloud: The points, with the statistic.
    #[pyo3(signature = (dim, *, radius=1.0, medianpercent=50.0))]
    fn zsmooth(&self, py: Python<'_>, dim: &str, radius: f64, medianpercent: f64) -> PyResult<Self> {
        Ok(Self { inner: py.detach(|| filters::zsmooth(&self.inner, dim, radius, medianpercent)).py()? })
    }

    /// Classifies ground points by skewness balancing, as PDAL's `filters.skewnessbalancing`;
    /// the points come back sorted by `z`, as PDAL returns them.
    ///
    /// Args:
    ///     ground_class: The class of ground points. 2 by default.
    ///     other_class: The class of other points. 1 by default.
    ///     only_ground: Change the class of ground points only.
    ///
    /// Returns:
    ///     PointCloud: The points, classified.
    #[pyo3(signature = (*, ground_class=2, other_class=1, only_ground=false))]
    fn skewness_balancing(&self, py: Python<'_>, ground_class: u8, other_class: u8, only_ground: bool) -> PyResult<Self> {
        Ok(Self { inner: py.detach(|| filters::skewness_balancing(&self.inner, ground_class, other_class, only_ground)).py()? })
    }

    /// The first `count` points, as PDAL's `filters.head`.
    ///
    /// Args:
    ///     count: 10 by default.
    ///     invert: Drop them instead.
    ///
    /// Returns:
    ///     PointCloud: The points kept.
    #[pyo3(signature = (count=10, *, invert=false))]
    fn head(&self, count: usize, invert: bool) -> Self {
        Self { inner: filters::head(&self.inner, count, invert) }
    }

    /// The last `count` points, as PDAL's `filters.tail`.
    ///
    /// Args:
    ///     count: 10 by default.
    ///     invert: Drop them instead.
    ///
    /// Returns:
    ///     PointCloud: The points kept.
    #[pyo3(signature = (count=10, *, invert=false))]
    fn tail(&self, count: usize, invert: bool) -> Self {
        Self { inner: filters::tail(&self.inner, count, invert) }
    }

    /// The points sorted by dimensions, as PDAL's `filters.sort`: by the first, then stably by
    /// each of the others in turn.
    ///
    /// Args:
    ///     dimensions: A dimension or a list of them.
    ///     order: `ASC` (the default) or `DESC`.
    ///
    /// Returns:
    ///     PointCloud: The points, sorted.
    #[pyo3(signature = (dimensions, *, order="ASC"))]
    fn sort(&self, py: Python<'_>, dimensions: &Bound<'_, PyAny>, order: &str) -> PyResult<Self> {
        let dims = strings(Some(dimensions))?;
        let descending = match order.to_ascii_uppercase().as_str() {
            "ASC" => false,
            "DESC" => true,
            _ => return Err(invalid(format!("invalid order '{order}': use ASC or DESC"))),
        };
        Ok(Self { inner: py.detach(|| filters::sort(&self.inner, &dims, descending)).py()? })
    }

    /// The point of the largest (or smallest) value of a dimension, as PDAL's `filters.locate`.
    ///
    /// Args:
    ///     dimension: The dimension.
    ///     minmax: `max` (the default) or `min`.
    ///
    /// Returns:
    ///     PointCloud: The point.
    #[pyo3(signature = (dimension, *, minmax="max"))]
    fn locate(&self, dimension: &str, minmax: &str) -> PyResult<Self> {
        let min = match minmax.to_ascii_lowercase().as_str() {
            "min" => true,
            "max" => false,
            _ => return Err(invalid(format!("invalid minmax '{minmax}': use min or max"))),
        };
        Ok(Self { inner: filters::locate(&self.inner, dimension, min).py()? })
    }

    /// The points within the interquartile fences of a dimension, as PDAL's `filters.iqr`.
    ///
    /// Args:
    ///     dimension: The dimension.
    ///     k: The fences are `k` interquartile ranges outside the quartiles. 1.5 by default.
    ///
    /// Returns:
    ///     PointCloud: The points kept.
    #[pyo3(signature = (dimension, *, k=1.5))]
    fn iqr(&self, dimension: &str, k: f64) -> PyResult<Self> {
        Ok(Self { inner: filters::iqr(&self.inner, dimension, k).py()? })
    }

    /// The points within `k` median absolute deviations of the median of a dimension, as
    /// PDAL's `filters.mad`.
    ///
    /// Args:
    ///     dimension: The dimension.
    ///     k: The number of deviations. 2 by default.
    ///     mad_multiplier: The factor of the median absolute deviation. 1.4862 by default.
    ///
    /// Returns:
    ///     PointCloud: The points kept.
    #[pyo3(signature = (dimension, *, k=2.0, mad_multiplier=1.4862))]
    fn mad(&self, dimension: &str, k: f64, mad_multiplier: f64) -> PyResult<Self> {
        Ok(Self { inner: filters::mad(&self.inner, dimension, k, mad_multiplier).py()? })
    }

    /// The points within a range of a raster's values, as PDAL's `filters.dem`; the raster is
    /// taken to be in the points' CRS.
    ///
    /// Args:
    ///     raster: A raster: a path or a `Raster`.
    ///     limits: The dimension and how far below and above the raster's value, such as
    ///         `"Z[20:20]"`.
    ///     band: The band, from 1. 1 by default.
    ///
    /// Returns:
    ///     PointCloud: The points kept.
    #[pyo3(signature = (raster, limits, *, band=1))]
    fn dem(&self, py: Python<'_>, raster: &Bound<'_, PyAny>, limits: &str, band: usize) -> PyResult<Self> {
        let ds = raster_arg(raster)?;
        let limits = DimRange::parse(limits).py()?;
        Ok(Self { inner: py.detach(|| filters::dem(&self.inner, &ds, &limits, band)).py()? })
    }

    /// Dimensions set from raster bands, as PDAL's `filters.colorization`; the raster is taken to
    /// be in the points' CRS, and points outside it keep their values.
    ///
    /// Args:
    ///     raster: A raster: a path or a `Raster`.
    ///     dimensions: `Name:band:scale` items, as PDAL writes them (a string, comma-separated,
    ///         or a list). `Red`, `Green` and `Blue` from bands 1, 2 and 3 by default.
    ///
    /// Returns:
    ///     PointCloud: The points, with the values.
    #[pyo3(signature = (raster, *, dimensions=None))]
    fn colorization(&self, py: Python<'_>, raster: &Bound<'_, PyAny>, dimensions: Option<&Bound<'_, PyAny>>) -> PyResult<Self> {
        let ds = raster_arg(raster)?;
        let spec = strings(dimensions)?.join(",");
        let dims = if spec.trim().is_empty() { BandDimension::rgb() } else { BandDimension::parse_list(&spec).py()? };
        Ok(Self { inner: py.detach(|| filters::colorization(&self.inner, &ds, &dims)).py()? })
    }

    /// A dimension set from an attribute of the polygons the points are in, as PDAL's
    /// `filters.overlay`: each point in a polygon takes the attribute's value (as an integer) of
    /// the first feature whose polygon has it.
    ///
    /// Args:
    ///     datasource: The polygons: a path, a `Layer` or a `VectorDataset`.
    ///     dimension: The dimension set, which the points must have.
    ///     column: The attribute. By default, the second, as PDAL takes it.
    ///     layer: The layer of a dataset. By default, the first.
    ///
    /// Returns:
    ///     PointCloud: The points, with the values.
    #[pyo3(signature = (datasource, dimension, *, column=None, layer=None))]
    fn overlay(
        &self,
        py: Python<'_>,
        datasource: &Bound<'_, PyAny>,
        dimension: String,
        column: Option<String>,
        layer: Option<String>,
    ) -> PyResult<Self> {
        let layer = layer_arg(py, datasource, layer)?;
        let o = OverlayOptions { dimension, column };
        Ok(Self { inner: py.detach(|| filters::overlay(&self.inner, &layer, &o)).py()? })
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
        .map(|f| {
            PointFormat::from_name(f).ok_or_else(|| invalid(format!("unknown point cloud format '{f}' (LAS, LAZ, COPC, PLY, E57, EPT)")))
        })
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

/// A string or a list of strings, as a list.
/// A raster given as a path or a `Raster`, whole in memory.
fn raster_arg(v: &Bound<'_, PyAny>) -> PyResult<terrio::RasterDataset> {
    if let Ok(path) = v.extract::<String>() {
        return terrio::open_raster(&path).py();
    }
    let mut r = v.extract::<PyRefMut<'_, crate::raster::PyRaster>>()?;
    r.with_dataset(|ds| Ok(ds.clone())).py()
}

/// A vector layer given as a path (its layer `layer`, or the first), a `Layer` or a
/// `VectorDataset` (its layer `layer`, or the first).
fn layer_arg(py: Python<'_>, v: &Bound<'_, PyAny>, layer: Option<String>) -> PyResult<terrio::Layer> {
    let pick = |layers: Vec<terrio::Layer>| -> PyResult<terrio::Layer> {
        match &layer {
            Some(name) => layers.into_iter().find(|l| &l.name == name).ok_or_else(|| invalid(format!("no layer named '{name}'"))),
            None => layers.into_iter().next().ok_or_else(|| invalid("the dataset has no layer")),
        }
    };
    if let Ok(path) = v.extract::<String>() {
        let options = terrio::VectorOpenOptions { layers: layer.iter().cloned().collect(), ..Default::default() };
        return pick(terrio::open_vector(&path, &options).py()?.layers);
    }
    if let Ok(l) = v.extract::<PyRef<'_, crate::vector::PyLayer>>() {
        return Ok(l.inner.clone());
    }
    let ds = v.extract::<PyRef<'_, crate::vector::PyVectorDataset>>()?;
    pick(ds.layer_copies(py))
}

fn strings(v: Option<&Bound<'_, PyAny>>) -> PyResult<Vec<String>> {
    match v {
        None => Ok(Vec::new()),
        Some(v) => match v.extract::<String>() {
            Ok(s) => Ok(vec![s]),
            Err(_) => v.extract::<Vec<String>>(),
        },
    }
}

/// Merges point clouds into one, as PDAL's `filters.merge`: the points of each in turn, with the
/// dimensions of all (zeros where a cloud has none).
///
/// Args:
///     clouds: The point clouds.
///
/// Returns:
///     PointCloud: The points of all.
#[pyfunction]
pub(crate) fn merge_points(clouds: Vec<PyRef<'_, PyPointCloud>>) -> PyResult<PyPointCloud> {
    let clouds: Vec<pointcloud::PointCloud> = clouds.iter().map(|c| c.inner.clone()).collect();
    Ok(PyPointCloud { inner: filters::merge(&clouds).py()? })
}

/// Registers the point cloud functions and classes.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyPointCloud>()?;
    m.add_class::<PyPointCloudFile>()?;
    m.add_class::<PyPointChunks>()?;
    m.add_class::<PyPointCloudWriter>()?;
    m.add_function(wrap_pyfunction!(grid_points, m)?)?;
    m.add_function(wrap_pyfunction!(open_points, m)?)?;
    m.add_function(wrap_pyfunction!(read_points, m)?)?;
    m.add_function(wrap_pyfunction!(read_points_bytes, m)?)?;
    m.add_function(wrap_pyfunction!(convert_points, m)?)?;
    m.add_function(wrap_pyfunction!(merge_points, m)?)?;
    m.add_function(wrap_pyfunction!(append_points, m)?)?;
    Ok(())
}
