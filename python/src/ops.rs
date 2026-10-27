//! Module-level functions: opening, writing, information, conversion and the processing
//! operations, with keyword arguments named after the command-line options.

use crate::raster::GcpTuple;
use crate::raster::{PyRaster, write_options};
use crate::vector::{PyLayer, PyVectorDataset, layers_from_object};
use crate::{PyResultExt, invalid};
use numpy::{PyArray1, PyArrayMethods};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyTuple};
use terrio::crs::{SpatialRef, TransformOptions, Transformer};
use terrio::formats::VectorDriver;
use terrio::ops::Resampling;
use terrio::ops::mosaic::{MosaicOptions, MosaicSource, ResolutionStrategy};
use terrio::ops::polygonize::PolygonizeOptions;
use terrio::ops::rasterize::{Grid, MergeAlg, RasterizeOptions};
use terrio::ops::translate::{Scale, Size, TranslateOptions};
use terrio::ops::vector_filter::{GeometryConversion, VectorTranslateOptions, WriteMode};
use terrio::ops::warp::{Cutline, Nodata, WarpOptions};
use terrio::{DataType, Dataset, RasterDataset, VectorOpenOptions, VectorWriteOptions, Window, WindowOptions};

/// Registers the functions in the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(open, m)?)?;
    m.add_function(wrap_pyfunction!(open_raster, m)?)?;
    m.add_function(wrap_pyfunction!(open_catalog, m)?)?;

    m.add_function(wrap_pyfunction!(open_vrt, m)?)?;
    m.add_function(wrap_pyfunction!(open_vector, m)?)?;
    m.add_function(wrap_pyfunction!(open_bytes, m)?)?;
    m.add_function(wrap_pyfunction!(list_layers, m)?)?;
    m.add_function(wrap_pyfunction!(info, m)?)?;
    m.add_function(wrap_pyfunction!(drivers, m)?)?;
    m.add_function(wrap_pyfunction!(write_raster, m)?)?;
    m.add_function(wrap_pyfunction!(write_netcdf_groups, m)?)?;
    m.add_function(wrap_pyfunction!(write_vector, m)?)?;
    m.add_function(wrap_pyfunction!(convert, m)?)?;
    m.add_function(wrap_pyfunction!(sql, m)?)?;
    m.add_function(wrap_pyfunction!(rasterize, m)?)?;
    m.add_function(wrap_pyfunction!(polygonize, m)?)?;
    m.add_function(wrap_pyfunction!(transform_points, m)?)?;
    m.add_function(wrap_pyfunction!(set_grid_search_paths, m)?)?;
    m.add_function(wrap_pyfunction!(set_network_enabled, m)?)?;
    m.add_function(wrap_pyfunction!(translate, m)?)?;
    m.add_function(wrap_pyfunction!(warp, m)?)?;
    m.add_function(wrap_pyfunction!(mosaic, m)?)?;
    Ok(())
}

/// Vector write options from a driver name.
pub(crate) fn vector_write_options(driver: Option<&str>) -> PyResult<VectorWriteOptions> {
    let driver = driver.map(|d| VectorDriver::from_name(d).ok_or_else(|| invalid(format!("unknown vector format '{d}'")))).transpose()?;
    Ok(VectorWriteOptions { driver, ..Default::default() })
}

/// Wraps a dataset in its Python class.
fn dataset_to_py(py: Python<'_>, ds: Dataset) -> PyResult<Py<PyAny>> {
    Ok(match ds {
        Dataset::Raster(r) => Py::new(py, PyRaster::from_dataset(r))?.into_any(),
        Dataset::Vector(v) => Py::new(py, PyVectorDataset::new(py, v)?)?.into_any(),
    })
}

pub(crate) fn srs(s: Option<&str>) -> PyResult<Option<SpatialRef>> {
    s.map(SpatialRef::parse).transpose().py()
}

pub(crate) fn write_mode(append: bool, overwrite: bool) -> PyResult<WriteMode> {
    match (append, overwrite) {
        (true, true) => Err(invalid("append and overwrite exclude each other")),
        (true, false) => Ok(WriteMode::Append),
        (false, true) => Ok(WriteMode::Overwrite),
        (false, false) => Ok(WriteMode::Create),
    }
}

pub(crate) fn data_type(name: &str) -> PyResult<DataType> {
    DataType::from_str(name).ok_or_else(|| invalid(format!("unknown data type '{name}'")))
}

/// Opens a dataset, raster or vector, detecting its format.
///
/// The whole dataset is read into memory; use `open_raster(path, lazy=True)` to read GeoTIFFs by
/// window.
///
/// Args:
///     path: A local path, an `http(s)://`, `s3://`, `gs://` or `az://` URL, a zip archive
///         (`archive.zip` or `archive.zip!inner/path`), or a GDAL `/vsi...` path.
///
/// Returns:
///     Raster | VectorDataset: The dataset.
///
/// Raises:
///     NotFoundError: If the file does not exist.
///     UnsupportedError: If the format is not recognized.
#[pyfunction]
fn open(py: Python<'_>, path: &str) -> PyResult<Py<PyAny>> {
    // Warnings are issued whether the open succeeds or not, so none is left for a later call.
    let ds = crate::with_warnings(py, || terrio::open(path))?;
    dataset_to_py(py, ds)
}

/// Opens a serialized VRT with source names mapped to file bytes or paths.
/// Sources are opened once; pixel windows remain lazy. This also supports VRTs
/// referring to uploaded GeoTIFFs without writing those files beside the XML.
#[pyfunction]
fn open_vrt(py: Python<'_>, xml: &str, sources: &Bound<'_, PyDict>) -> PyResult<PyRaster> {
    let description = terrio::ops::vrt::VirtualRaster::from_xml(xml).py()?;
    let mut resolved = std::collections::HashMap::new();
    for (name, value) in sources.iter() {
        let name = name.extract::<String>()?;
        let reader = if let Ok(bytes) = value.cast::<PyBytes>() {
            let bytes = bytes.as_bytes().to_vec();
            py.detach(|| terrio::memory::open_raster_lazy_bytes(bytes, &name)).py()?
        } else {
            let path = value.extract::<std::path::PathBuf>()?;
            let path = path.to_str().ok_or_else(|| invalid("source paths must be UTF-8"))?;
            py.detach(|| terrio::open_raster_lazy(path)).py()?
        };
        resolved.insert(name, reader);
    }
    let inner = py.detach(|| description.into_lazy(std::path::Path::new(""), resolved)).py()?;
    Ok(PyRaster { inner })
}

/// Opens a raster.
///
/// Args:
///     path: The raster's path or URL, as for `open`.
///     lazy: Read only the headers now and pixels on demand, by window. GeoTIFFs (and COGs,
///         local or remote, fetched with HTTP range requests) are read in place; other formats
///         are read whole either way.
///
/// Returns:
///     Raster: The raster.
///
/// Raises:
///     UnsupportedError: If the dataset is not a raster.
///
/// Warns:
///     UserWarning: If the raster declares a CRS terrio cannot represent (it then has none).
#[pyfunction]
#[pyo3(signature = (path, lazy=false))]
fn open_raster(py: Python<'_>, path: &str, lazy: bool) -> PyResult<PyRaster> {
    if lazy {
        let r = crate::with_warnings(py, || terrio::open_raster_lazy(path))?;
        return Ok(PyRaster { inner: r });
    }
    let ds = crate::with_warnings(py, || terrio::open_raster(path))?;
    Ok(PyRaster::from_dataset(ds))
}

/// Opens a vector dataset.
///
/// Args:
///     path: The dataset's path or URL, as for `open`.
///     layers: Only these layers (names). `None` reads them all.
///     bbox: Only the features whose bounding box intersects this rectangle `(min_x, min_y,
///         max_x, max_y)`, in the layer's CRS. GeoPackages find them through their R-tree
///         index and GeoParquet files through their bounding-box columns, without reading the
///         other features; other formats are read whole, then filtered.
///
/// Returns:
///     VectorDataset: The dataset.
///
/// Raises:
///     UnsupportedError: If the dataset is not a vector dataset.
///     InvalidArgumentError: If a layer does not exist.
#[pyfunction]
#[pyo3(signature = (path, layers=None, bbox=None))]
fn open_vector(py: Python<'_>, path: &str, layers: Option<Vec<String>>, bbox: Option<(f64, f64, f64, f64)>) -> PyResult<PyVectorDataset> {
    let options = VectorOpenOptions { layers: layers.unwrap_or_default(), spatial_filter: bbox, ..Default::default() };
    let mut ds = py.detach(|| terrio::open_vector(path, &options)).py()?;
    // Formats without a spatial index are read whole: filter them the same way.
    if let Some((x0, y0, x1, y1)) = bbox {
        for layer in &mut ds.layers {
            layer.features.retain(|f| {
                f.geometry.as_ref().and_then(|g| g.bbox()).is_some_and(|b| b.min_x <= x1 && b.max_x >= x0 && b.min_y <= y1 && b.max_y >= y0)
            });
            layer.extent = None;
        }
    }
    PyVectorDataset::new(py, ds)
}

/// Opens a dataset from its bytes, such as a download or an upload.
///
/// Args:
///     data: The file's contents.
///     filename: A file name whose extension tells the format, such as `"dem.tif"` or
///         `"parcels.zip"`. Its stem names vector layers.
///     lazy: For GeoTIFFs, decode pixels by window on demand rather than now.
///
/// Returns:
///     Raster | VectorDataset: The dataset.
#[pyfunction]
#[pyo3(signature = (data, filename, lazy=false))]
fn open_bytes(py: Python<'_>, data: Vec<u8>, filename: &str, lazy: bool) -> PyResult<Py<PyAny>> {
    if lazy {
        let r = py.detach(|| terrio::memory::open_raster_lazy_bytes(data, filename)).py()?;
        return Ok(Py::new(py, PyRaster { inner: r })?.into_any());
    }
    let ds = py.detach(|| terrio::memory::open_bytes(data, filename)).py()?;
    dataset_to_py(py, ds)
}

/// Lists the layers of a vector dataset, without reading the features when the format allows.
///
/// Args:
///     path: The dataset's path or URL.
///
/// Returns:
///     list[str]: The layer names.
#[pyfunction]
fn list_layers(py: Python<'_>, path: &str) -> PyResult<Vec<String>> {
    py.detach(|| terrio::list_layers(path)).py()
}

/// Summarizes a dataset, as `terrio info --json` does.
///
/// GeoTIFFs (local or remote) are read in place: only their headers are read unless statistics
/// are asked for.
///
/// Args:
///     path: The dataset's path or URL.
///     stats: Compute band statistics (reads every pixel).
///     layers: Only these layers of a vector dataset.
///     hist: Compute band histograms as `gdalinfo -hist` does (each band's `histogram`:
///         `count`, `min`, `max` and `buckets`).
///
/// Returns:
///     dict: The summary: driver, size, bands, CRS, extent... for rasters (with the
///     `subdatasets` of netCDF, HDF5 and GeoPackage files); driver and layers
///     (fields, geometry type, feature count, CRS, extent) for vector datasets; header, CRS,
///     dimensions, VLRs and the COPC octree for point clouds (LAS, LAZ, COPC), with
///     classification and return counts when `stats` is true.
#[pyfunction]
#[pyo3(signature = (path, stats=false, layers=None, hist=false))]
fn info<'py>(py: Python<'py>, path: &str, stats: bool, layers: Option<Vec<String>>, hist: bool) -> PyResult<Bound<'py, PyAny>> {
    let options = terrio::info::InfoOptions { statistics: stats, histogram: hist };
    let summary = py
        .detach(|| match layers {
            Some(layers) => {
                let ds = terrio::open_vector(path, &VectorOpenOptions { layers, ..Default::default() })?;
                Ok(terrio::info::info(&Dataset::Vector(ds), &options))
            }
            None => terrio::info::info_path(path, &options),
        })
        .py()?;
    let text = serde_json::to_string(&summary).map_err(|e| invalid(e.to_string()))?;
    py.import("json")?.call_method1("loads", (text,))
}

/// The formats terrio reads and writes.
///
/// Returns:
///     list[dict]: One entry per driver: `name`, `long_name`, `kind` (`"raster"`, `"vector"`
///     or `"pointcloud"`), `extensions`, `read` and `write`.
#[pyfunction]
fn drivers<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
    let text = serde_json::to_string(&terrio::formats::all_drivers()).map_err(|e| invalid(e.to_string()))?;
    py.import("json")?.call_method1("loads", (text,))
}

/// Writes a raster to a file.
///
/// Args:
///     raster: The raster.
///     path: The output path. Its extension tells the format unless `driver` is given.
///     driver: The format's short name, such as `"GTiff"`, `"COG"` or `"AAIGrid"`.
///     compress: GeoTIFF/COG compression: `"NONE"`, `"LZW"`, `"DEFLATE"`, `"PACKBITS"`, `"JPEG"`
///         or `"WEBP"`.
///     tile_size: Write GeoTIFF tiles of this size.
///     bigtiff: Write a BigTIFF.
#[pyfunction]
#[pyo3(signature = (raster, path, driver=None, compress=None, tile_size=None, bigtiff=false))]
fn write_raster(
    py: Python<'_>,
    raster: &mut PyRaster,
    path: &str,
    driver: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<()> {
    let options = write_options(driver, compress, tile_size, bigtiff)?;
    py.detach(|| raster.with_dataset(|ds| terrio::write_raster_dataset(ds, path, &options))).py()
}

/// Writes rasters into one netCDF-4 file, each in its group.
///
/// Each raster goes in its group, with its own dimensions, coordinate variables and grid mapping,
/// as the netCDF library lays out groups. The rasters read back as
/// `NETCDF:"file.nc":/group/Band1`, in terrio and in GDAL.
///
/// Args:
///     rasters: `(group, raster)` pairs, or a dict from group to raster. A group is a path such as
///         `"/terrain/dem"` (groups are created as needed), or `"/"` for the root group.
///     path: The output netCDF file.
///     options: netCDF creation options, as GDAL names them: `FORMAT` (`NC4`, the default, which
///         groups need), `COMPRESS` (`DEFLATE`), `ZLEVEL` and `CHUNKING`.
///
/// Raises:
///     InvalidArgumentError: If there are no rasters, or two of them are in the same group.
#[pyfunction]
#[pyo3(signature = (rasters, path, options=None))]
fn write_netcdf_groups(
    py: Python<'_>,
    rasters: &Bound<'_, PyAny>,
    path: &str,
    options: Option<std::collections::BTreeMap<String, String>>,
) -> PyResult<()> {
    let pairs: Vec<(String, Bound<'_, PyAny>)> = match rasters.cast::<pyo3::types::PyDict>() {
        Ok(d) => d.iter().map(|(k, v)| Ok((k.extract::<String>()?, v))).collect::<PyResult<_>>()?,
        Err(_) => rasters.try_iter()?.map(|item| item?.extract::<(String, Bound<'_, PyAny>)>()).collect::<PyResult<_>>()?,
    };
    let mut datasets: Vec<(String, terrio::RasterDataset)> = Vec::with_capacity(pairs.len());
    for (group, r) in pairs {
        let mut raster = r.extract::<PyRefMut<'_, PyRaster>>()?;
        let ds = raster.with_dataset(|ds| Ok(ds.clone())).py()?;
        datasets.push((group, ds));
    }
    let o = terrio::RasterWriteOptions { creation_options: options.unwrap_or_default().into_iter().collect(), ..Default::default() };
    let refs: Vec<(&str, &terrio::RasterDataset)> = datasets.iter().map(|(g, ds)| (g.as_str(), ds)).collect();
    py.detach(|| terrio::write_netcdf_groups(&refs, path, &o)).py()
}

/// Writes vector data to a file.
///
/// Args:
///     data: A `Layer`, a `VectorDataset`, a list of layers, or GeoJSON: a dict, a JSON string,
///         or any object with `__geo_interface__` (such as a GeoDataFrame).
///     path: The output path. Its extension tells the format unless `driver` is given.
///     driver: The format's short name, such as `"GPKG"`, `"FlatGeobuf"` or `"ESRI Shapefile"`.
///     layer_name: The layer name for GeoJSON and Arrow input. By default, the output file's stem.
///     crs: The CRS of GeoJSON and Arrow input. By default, that of a GeoDataFrame or of the
///         GeoArrow metadata, otherwise EPSG:4326 for GeoJSON.
///     append: Add the features to the layers of an existing output.
///     overwrite: Replace the layers of the same name in an existing output.
///
/// Raises:
///     InvalidArgumentError: If several layers go to a single-layer format.
#[pyfunction]
#[pyo3(signature = (data, path, driver=None, layer_name=None, crs=None, append=false, overwrite=false))]
#[allow(clippy::too_many_arguments)]
fn write_vector(
    py: Python<'_>,
    data: &Bound<'_, PyAny>,
    path: &str,
    driver: Option<&str>,
    layer_name: Option<&str>,
    crs: Option<&str>,
    append: bool,
    overwrite: bool,
) -> PyResult<()> {
    let stem = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("layer").to_string();
    let layers = layers_from_object(data, Some(layer_name.unwrap_or(&stem)), crs)?;
    let options = vector_write_options(driver)?;
    let mode = write_mode(append, overwrite)?;
    py.detach(|| terrio::ops::vector_filter::write_vector_with_mode(&layers, path, &options, mode)).py()
}

/// Converts a dataset to another format, optionally reprojecting, subsetting and filtering it,
/// like `terrio convert`.
///
/// Args:
///     src: The input dataset's path or URL.
///     dst: The output path.
///     format: The output format's short name (`"GTiff"`, `"COG"`, `"GPKG"`,
///         `"ESRI Shapefile"`...). By default, chosen from the output's extension.
///     layers: Only these layers of a vector dataset.
///     t_srs: Reproject into this CRS (rasters are warped with nearest neighbour; see `warp`).
///     compress: GeoTIFF/COG compression.
///     tile_size: GeoTIFF/COG tile size.
///     bigtiff: Write a BigTIFF.
///     srcwin: Convert only this raster window, `(col_off, row_off, width, height)`.
///     overview: Convert this raster overview (0 is the largest) instead of the full
///         resolution.
///     where: Keep the features matching this OGR SQL expression.
///     spat: Keep the features intersecting this rectangle `(min_x, min_y, max_x, max_y)`.
///     spat_srs: The CRS of `spat`. By default, that of the layer.
///     select: Keep only these fields, in this order.
///     limit: Keep at most this many features per layer.
///     sql: Read the documented OGR SQL subset, including expressions, DISTINCT, summaries and LEFT JOIN.
///     nln: The output layer name.
///     nlt: Convert geometries: `"PROMOTE_TO_MULTI"`, `"POINT"`, `"MULTIPOLYGON"`...
///     explodecollections: Write each part of multi-part geometries as its own feature.
///     simplify: Simplify geometries with this tolerance, preserving topology, before
///         reprojection (`ogr2ogr -simplify`).
///     segmentize: Add vertices so that no segment is longer than this, before reprojection
///         (`ogr2ogr -segmentize`).
///     s_srs: The CRS of the input, overriding what it declares.
///     a_srs: Assign this CRS to the output, without reprojecting.
///     append: Add the features to the layers of an existing output.
///     overwrite: Replace the layers of the same name in an existing output.
///
/// Raises:
///     InvalidArgumentError: If an option does not apply to the input.
#[pyfunction]
#[pyo3(signature = (
    src, dst, *, format=None, layers=None, t_srs=None, compress=None, tile_size=None, bigtiff=false, srcwin=None,
    overview=None, r#where=None, spat=None, spat_srs=None, select=None, limit=None, sql=None, nln=None, nlt=None,
    explodecollections=false, simplify=None, segmentize=None, s_srs=None, a_srs=None, append=false, overwrite=false, ct_opt=None
))]
#[allow(clippy::too_many_arguments)]
fn convert(
    py: Python<'_>,
    src: &str,
    dst: &str,
    format: Option<&str>,
    layers: Option<Vec<String>>,
    t_srs: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
    srcwin: Option<(usize, usize, usize, usize)>,
    overview: Option<usize>,
    r#where: Option<String>,
    spat: Option<(f64, f64, f64, f64)>,
    spat_srs: Option<&str>,
    select: Option<Vec<String>>,
    limit: Option<usize>,
    sql: Option<String>,
    nln: Option<String>,
    nlt: Option<&str>,
    explodecollections: bool,
    simplify: Option<f64>,
    segmentize: Option<f64>,
    s_srs: Option<&str>,
    a_srs: Option<&str>,
    append: bool,
    overwrite: bool,
    ct_opt: Option<std::collections::HashMap<String, String>>,
) -> PyResult<()> {
    use terrio::ops::vector_geom::GeometryOp;
    if simplify.is_some() && segmentize.is_some() {
        return Err(invalid("simplify and segmentize cannot be combined"));
    }
    // As ogr2ogr, values that are not positive leave the geometries unchanged.
    let geometry_op = GeometryOp::from_ogr2ogr(simplify, segmentize);
    let transform_options = TransformOptions::from_strict_options(&ct_opt.unwrap_or_default().into_iter().collect::<Vec<_>>()).py()?;
    let t_srs = srs(t_srs)?;
    let vector_options = VectorTranslateOptions {
        where_clause: r#where,
        sql,
        spatial_filter: spat,
        spatial_filter_srs: srs(spat_srs)?,
        select,
        limit,
        layer_name: nln,
        geometry_type: nlt.map(GeometryConversion::parse).transpose().py()?,
        explode_collections: explodecollections,
        geometry_op,
        strict_geometry_op: false,
        src_srs: srs(s_srs)?,
        dst_srs: t_srs.clone(),
        assign_srs: srs(a_srs)?,
        transform_options: transform_options.clone(),
    };
    let vector_set = vector_options.where_clause.is_some()
        || vector_options.sql.is_some()
        || vector_options.spatial_filter.is_some()
        || vector_options.select.is_some()
        || vector_options.limit.is_some()
        || vector_options.layer_name.is_some()
        || vector_options.geometry_type.is_some()
        || vector_options.explode_collections
        || simplify.is_some()
        || segmentize.is_some()
        || vector_options.src_srs.is_some()
        || vector_options.assign_srs.is_some()
        || append
        || overwrite;
    let mode = write_mode(append, overwrite)?;
    let raster_options = match format.map(terrio::formats::RasterDriver::from_name) {
        Some(None) => None,
        _ => Some(write_options(format, compress, tile_size, bigtiff)?),
    };
    let vector_write = match format.map(VectorDriver::from_name) {
        Some(None) => None,
        _ => Some(vector_write_options(format)?),
    };
    crate::with_warnings(py, || -> terrio::Result<()> {
        // Vector conversions between formats read and written as streams go a chunk of
        // features at a time, in bounded memory.
        if srcwin.is_none() && overview.is_none() && mode == WriteMode::Create && !terrio::tiles::is_archive(src) {
            let stream = terrio::stream::ConvertOptions {
                open: VectorOpenOptions {
                    layers: layers.clone().unwrap_or_default(),
                    spatial_filter: vector_options.read_filter(src),
                    ..Default::default()
                },
                translate: vector_options.clone(),
                write: vector_write.clone().unwrap_or_default(),
                ..Default::default()
            };
            if vector_write.is_some() && terrio::stream::can_convert_stream(src, dst, &stream) {
                return terrio::stream::convert(src, dst, &stream);
            }
        }
        let ds = if srcwin.is_some() || overview.is_some() {
            let mut r = terrio::open_raster_lazy(src)?;
            let window = match srcwin {
                Some((x, y, w, h)) => Window::new(x, y, w, h),
                None => {
                    let (w, h) = match overview {
                        Some(i) => r.overviews().get(i).copied().ok_or_else(|| {
                            terrio::Error::InvalidArgument(format!("no overview {i}: the raster has {} overview(s)", r.overviews().len()))
                        })?,
                        None => (r.width(), r.height()),
                    };
                    Window::full(w, h)
                }
            };
            Dataset::Raster(r.read_window(window, &WindowOptions { bands: Vec::new(), overview })?)
        } else {
            // Readers with a spatial index skip the features outside the `spat` rectangle.
            let spatial_filter = vector_options.read_filter(src);
            match layers {
                Some(layers) => {
                    Dataset::Vector(terrio::open_vector(src, &VectorOpenOptions { layers, spatial_filter, ..Default::default() })?)
                }
                None if spatial_filter.is_some() => {
                    Dataset::Vector(terrio::open_vector(src, &VectorOpenOptions { spatial_filter, ..Default::default() })?)
                }
                None => terrio::open(src)?,
            }
        };
        match ds {
            Dataset::Raster(r) => {
                if vector_set {
                    return Err(terrio::Error::InvalidArgument("the vector options apply to vector inputs only".into()));
                }
                let options = raster_options
                    .ok_or_else(|| terrio::Error::InvalidArgument(format!("unknown raster format '{}'", format.unwrap_or_default())))?;
                let mut r = r;
                if let Some(dst_srs) = t_srs {
                    let o = terrio::ops::warp::WarpOptions {
                        dst_srs: Some(dst_srs),
                        transform_options: transform_options.clone(),
                        ..Default::default()
                    };
                    r = terrio::ops::warp::warp(&[&r], &o)?;
                }
                terrio::write_raster_dataset(&r, dst, &options)
            }
            Dataset::Vector(v) => {
                let options = vector_write
                    .ok_or_else(|| terrio::Error::InvalidArgument(format!("unknown vector format '{}'", format.unwrap_or_default())))?;
                let layers = terrio::ops::vector_filter::translate(&v.layers, &vector_options)?;
                terrio::ops::vector_filter::write_vector_with_mode(&layers, dst, &options, mode)
            }
        }
    })
}

/// Opens the layers given to an operation: a path, or layers in memory.
pub(crate) fn input_layers(py: Python<'_>, src: &Bound<'_, PyAny>, layers: Option<Vec<String>>) -> PyResult<Vec<terrio::Layer>> {
    match src.extract::<String>() {
        Ok(path) => {
            let options = VectorOpenOptions { layers: layers.unwrap_or_default(), ..Default::default() };
            Ok(py.detach(|| terrio::open_vector(&path, &options)).py()?.layers)
        }
        Err(_) => {
            let all = layers_from_object(src, None, None)?;
            Ok(match layers {
                Some(names) => all.into_iter().filter(|l| names.contains(&l.name)).collect(),
                None => all,
            })
        }
    }
}

/// Runs a read-only OGR SQL subset query on vector layers.
///
/// Args:
///     src: A vector path, VectorDataset, Layer, list of layers or GeoJSON.
///     query: SELECT statement, including computed columns, DISTINCT, summaries or LEFT JOIN.
///     layers: Restrict the available input layers.
///
/// Returns:
///     Layer: The query result. DISTINCT and summaries discard geometry.
#[pyfunction]
#[pyo3(signature = (src, query, *, layers=None))]
fn sql(py: Python<'_>, src: &Bound<'_, PyAny>, query: &str, layers: Option<Vec<String>>) -> PyResult<PyLayer> {
    let inputs = input_layers(py, src, layers)?;
    let inner = py.detach(|| terrio::ops::sql::query(&inputs, query)).py()?;
    Ok(PyLayer { inner })
}

/// A raster path as UTF-8, accepting Python strings and os.PathLike objects.
/// Rasters return None so they keep their existing in-memory/lazy handling.
pub(crate) fn raster_path(src: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    if src.cast::<PyRaster>().is_ok() {
        return Ok(None);
    }
    let path = src.extract::<std::path::PathBuf>().map_err(|_| invalid("expected a path or a terrio.Raster"))?;
    path.into_os_string().into_string().map(Some).map_err(|_| invalid("raster paths must be UTF-8"))
}

/// Opens the raster given to an operation: a path, or a `Raster`.
pub(crate) fn input_raster(py: Python<'_>, src: &Bound<'_, PyAny>) -> PyResult<RasterDataset> {
    match raster_path(src)? {
        Some(path) => crate::with_warnings(py, || terrio::open_raster(&path)),
        None => {
            let mut r = src.cast::<PyRaster>().map_err(|_| invalid("expected a path or a terrio.Raster"))?.borrow_mut();
            r.with_dataset(|ds| Ok(ds.clone())).py()
        }
    }
}

/// Burns vector geometries into a raster, like `terrio rasterize` (GDAL's `gdal_rasterize`).
///
/// Args:
///     src: The vector data: a path, a `Layer`, a `VectorDataset`, a list of layers, or GeoJSON.
///     dst: Also write the raster to this path.
///     layers: The layers to burn, by name. Needed when the dataset has several.
///     sql: Burn the result of `SELECT fields FROM layer [WHERE ...]` instead of whole layers.
///     burn: The value to burn (a number, or one per output band).
///     attribute: Burn the values of this attribute field instead.
///     where: Only burn the features matching this OGR SQL expression.
///     tr: The pixel size `(xres, yres)`.
///     ts: The size in pixels `(width, height)`; one may be 0 to keep the aspect ratio.
///     te: The extent `(min_x, min_y, max_x, max_y)`. By default, that of the layers.
///     tap: Align the extent on multiples of the pixel size.
///     like: Use the grid of this raster (a path or a `Raster`).
///     ot: The output data type: `"Byte"`, `"Int16"`, `"UInt16"`, `"Int32"`, `"Float32"`,
///         `"Float64"` (the default)...
///     init: The initial pixel values (a number, or one per band). 0 by default.
///     nodata: The output nodata value.
///     all_touched: Burn every pixel touched by lines or polygons, not only those whose centre is
///         inside.
///     add: Add the burn values to the pixels instead of replacing them.
///     a_srs: The output CRS. Geometries are taken to be in it, without reprojecting.
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///
/// Returns:
///     Raster: The raster, in memory.
#[pyfunction]
#[pyo3(signature = (
    src, dst=None, *, layers=None, sql=None, burn=None, attribute=None, r#where=None, tr=None, ts=None, te=None,
    tap=false, like=None, ot=None, init=None, nodata=None, all_touched=false, add=false, a_srs=None, format=None,
    compress=None
))]
#[allow(clippy::too_many_arguments)]
fn rasterize(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    layers: Option<Vec<String>>,
    sql: Option<String>,
    burn: Option<&Bound<'_, PyAny>>,
    attribute: Option<String>,
    r#where: Option<String>,
    tr: Option<(f64, f64)>,
    ts: Option<(usize, usize)>,
    te: Option<(f64, f64, f64, f64)>,
    tap: bool,
    like: Option<&Bound<'_, PyAny>>,
    ot: Option<&str>,
    init: Option<&Bound<'_, PyAny>>,
    nodata: Option<f64>,
    all_touched: bool,
    add: bool,
    a_srs: Option<&str>,
    format: Option<&str>,
    compress: Option<String>,
) -> PyResult<PyRaster> {
    let numbers = |v: Option<&Bound<'_, PyAny>>| -> PyResult<Vec<f64>> {
        match v {
            None => Ok(Vec::new()),
            Some(v) => match v.extract::<f64>() {
                Ok(x) => Ok(vec![x]),
                Err(_) => v.extract::<Vec<f64>>(),
            },
        }
    };
    let burn_values = numbers(burn)?;
    if burn_values.is_empty() && attribute.is_none() {
        return Err(invalid("give burn or attribute"));
    }
    let named_layers = layers.is_some();
    let all = input_layers(py, src, layers)?;
    let to_burn = match &sql {
        Some(sql) => {
            let o = VectorTranslateOptions { sql: Some(sql.clone()), ..Default::default() };
            py.detach(|| terrio::ops::vector_filter::translate(&all, &o)).py()?
        }
        None if !named_layers && all.len() != 1 => {
            let names: Vec<&str> = all.iter().map(|l| l.name.as_str()).collect();
            return Err(invalid(format!("the dataset has {} layers: choose with layers or sql ({})", all.len(), names.join(", "))));
        }
        None => all,
    };
    let grid = like.map(|l| input_raster(py, l)).transpose()?.map(|r| Grid::like(&r));
    let options = RasterizeOptions {
        burn_values,
        attribute,
        where_clause: r#where,
        resolution: tr,
        size: ts,
        extent: te,
        target_aligned: tap,
        grid,
        data_type: ot.map(data_type).transpose()?,
        init: numbers(init)?,
        nodata,
        all_touched,
        merge: if add { MergeAlg::Add } else { MergeAlg::Replace },
        assign_srs: srs(a_srs)?,
    };
    let write = dst.map(|_| write_options(format, compress, None, false)).transpose()?;
    let out = py
        .detach(|| -> terrio::Result<RasterDataset> {
            let out = terrio::ops::rasterize::rasterize(&to_burn, &options)?;
            if let (Some(path), Some(o)) = (dst, &write) {
                terrio::write_raster_dataset(&out, path, o)?;
            }
            Ok(out)
        })
        .py()?;
    Ok(PyRaster::from_dataset(out))
}

/// Turns a raster band into polygons of connected pixels with the same value, like
/// `terrio polygonize` (GDAL's `gdal_polygonize`).
///
/// Args:
///     src: The raster: a path or a `Raster`.
///     dst: Also write the polygons to this path.
///     band: The band to polygonize, numbered from 1.
///     field: The name of the field receiving the pixel values.
///     layer_name: The layer name. By default, the output file's stem, or `"polygonized"`.
///     connectedness: 4 to connect pixels through their sides only, 8 to connect them through
///         their corners too.
///     nomask: Do not leave out nodata pixels.
///     mask: Leave out the pixels where the first band of this raster (a path or a `Raster`) is 0.
///     format: The output format's short name, for `dst`.
///     append: Add the polygons to the layer of the same name in an existing output.
///     overwrite: Replace the layer of the same name in an existing output.
///
/// Returns:
///     Layer: The polygons, with the pixel value in `field`.
#[pyfunction]
#[pyo3(signature = (
    src, dst=None, *, band=1, field="DN", layer_name=None, connectedness=4, nomask=false, mask=None, format=None,
    append=false, overwrite=false
))]
#[allow(clippy::too_many_arguments)]
fn polygonize(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    band: usize,
    field: &str,
    layer_name: Option<String>,
    connectedness: u8,
    nomask: bool,
    mask: Option<&Bound<'_, PyAny>>,
    format: Option<&str>,
    append: bool,
    overwrite: bool,
) -> PyResult<PyLayer> {
    if band == 0 {
        return Err(invalid("bands are numbered from 1"));
    }
    if connectedness != 4 && connectedness != 8 {
        return Err(invalid("connectedness is 4 or 8"));
    }
    let ds = input_raster(py, src)?;
    let mask = mask.map(|m| input_raster(py, m)).transpose()?;
    let stem = dst.and_then(|d| std::path::Path::new(d).file_stem()).and_then(|s| s.to_str()).unwrap_or("polygonized").to_string();
    let options = PolygonizeOptions {
        band: band - 1,
        field_name: field.to_string(),
        layer_name: layer_name.unwrap_or(stem),
        connectedness8: connectedness == 8,
        ignore_nodata: nomask,
        mask,
    };
    let write = dst.map(|_| vector_write_options(format)).transpose()?;
    let mode = write_mode(append, overwrite)?;
    let layer = py
        .detach(|| -> terrio::Result<terrio::Layer> {
            let layer = terrio::ops::polygonize::polygonize(&ds, &options)?;
            if let (Some(path), Some(o)) = (dst, &write) {
                terrio::ops::vector_filter::write_vector_with_mode(std::slice::from_ref(&layer), path, o, mode)?;
            }
            Ok(layer)
        })
        .py()?;
    Ok(PyLayer { inner: layer })
}

/// Transforms coordinates from one CRS to another.
///
/// Coordinates are in GIS order whatever the CRS's axis order: x (easting, longitude) first.
/// Datum shift grids and geoid models are used when they can be found, as GDAL uses them (see
/// :func:`set_grid_search_paths`). With heights, between CRSs that both have heights
/// (geographic 3D CRSs such as ``"EPSG:4979"`` and compound CRSs such as ``"EPSG:4326+5773"``),
/// heights go through the datum transformation and geoid models; otherwise they are kept, as
/// GDAL keeps them between 2D CRSs.
///
/// Args:
///     xs: The x coordinates (a sequence or a NumPy array).
///     ys: The y coordinates.
///     src_crs: The source CRS: `"EPSG:4326"`, `"EPSG:4326+5773"`, WKT or a PROJ string.
///     dst_crs: The target CRS.
///     zs: Heights, or Cartesian Z for geocentric CRSs. Supply all three ordinates
///         for geocentric transforms; the 2D call uses input Z=0 and drops output Z.
///     s_coord_epoch: The coordinate epoch of the source coordinates (a decimal year), for a
///         dynamic source CRS (GDAL's ``-s_coord_epoch``).
///     t_coord_epoch: The coordinate epoch of the target coordinates, for a dynamic target CRS
///         (GDAL's ``-t_coord_epoch``).
///     promote_to_3d: Take the heights of 2D CRSs as ellipsoidal heights, so that datum
///         transformations change them, as GDAL does after ``PromoteTo3D``.
///
/// Returns:
///     tuple[numpy.ndarray, ...]: The transformed x and y coordinates, and heights when ``zs`` is
///     given, as float64.
///
/// Raises:
///     CRSError: If a CRS cannot be parsed or a point cannot be transformed.
#[pyfunction]
#[pyo3(signature = (xs, ys, src_crs, dst_crs, zs=None, *, s_coord_epoch=None, t_coord_epoch=None, allow_ballpark=None, only_best=false, promote_to_3d=false))]
#[allow(clippy::too_many_arguments)]
fn transform_points<'py>(
    py: Python<'py>,
    xs: &Bound<'py, PyAny>,
    ys: &Bound<'py, PyAny>,
    src_crs: &str,
    dst_crs: &str,
    zs: Option<&Bound<'py, PyAny>>,
    s_coord_epoch: Option<f64>,
    t_coord_epoch: Option<f64>,
    allow_ballpark: Option<bool>,
    only_best: bool,
    promote_to_3d: bool,
) -> PyResult<Bound<'py, PyTuple>> {
    let np = py.import("numpy")?;
    let coords = |v: &Bound<'py, PyAny>| -> PyResult<Vec<f64>> {
        let a = np.call_method1("ascontiguousarray", (v, "float64"))?.call_method0("ravel")?;
        Ok(a.cast_into::<PyArray1<f64>>()?.readonly().as_slice()?.to_vec())
    };
    let (xs, ys) = (coords(xs)?, coords(ys)?);
    if xs.len() != ys.len() {
        return Err(invalid(format!("xs and ys differ in length ({} and {})", xs.len(), ys.len())));
    }
    let zs = zs.map(coords).transpose()?;
    if let Some(z) = &zs
        && z.len() != xs.len()
    {
        return Err(invalid(format!("xs and zs differ in length ({} and {})", xs.len(), z.len())));
    }
    let src = SpatialRef::parse(src_crs).py()?;
    let dst = SpatialRef::parse(dst_crs).py()?;
    let opts = TransformOptions {
        source_coordinate_epoch: s_coord_epoch,
        target_coordinate_epoch: t_coord_epoch,
        allow_ballpark,
        only_best,
        promote_to_3d,
    };
    let (ox, oy, oz) = crate::with_warnings(py, || -> terrio::Result<(Vec<f64>, Vec<f64>, Vec<f64>)> {
        let t = Transformer::with_options(&src, &dst, &opts)?;
        let mut ox = Vec::with_capacity(xs.len());
        let mut oy = Vec::with_capacity(ys.len());
        let mut oz = Vec::with_capacity(zs.as_ref().map_or(0, Vec::len));
        for (i, (x, y)) in xs.iter().zip(&ys).enumerate() {
            match &zs {
                Some(z) => {
                    let (a, b, c) = t.transform_3d(*x, *y, z[i])?;
                    ox.push(a);
                    oy.push(b);
                    oz.push(c);
                }
                None => {
                    let (a, b) = t.transform(*x, *y)?;
                    ox.push(a);
                    oy.push(b);
                }
            }
        }
        Ok((ox, oy, oz))
    })?;
    let (ox, oy) = (PyArray1::from_vec(py, ox), PyArray1::from_vec(py, oy));
    if zs.is_some() { PyTuple::new(py, [ox, oy, PyArray1::from_vec(py, oz)]) } else { PyTuple::new(py, [ox, oy]) }
}

/// Sets the directories datum shift and geoid grids are looked up in, instead of those of the
/// environment (``TERRIO_GRID_DIR``, ``PROJ_DATA``, ``PROJ_LIB``, PROJ's user directory and
/// terrio's download cache), as PROJ's ``proj_context_set_search_paths``.
///
/// Args:
///     paths: The directories, in search order; ``None`` goes back to the environment's.
#[pyfunction]
#[pyo3(signature = (paths))]
fn set_grid_search_paths(paths: Option<Vec<std::path::PathBuf>>) {
    terrio::crs::set_grid_search_paths(paths);
}

/// Enables or disables downloading grids from the PROJ CDN (https://cdn.proj.org) into terrio's
/// cache, instead of following the ``TERRIO_NETWORK`` (or ``PROJ_NETWORK``) environment
/// variable.
///
/// Args:
///     enabled: Whether to download grids; ``None`` goes back to the environment variable.
#[pyfunction]
#[pyo3(signature = (enabled))]
fn set_network_enabled(enabled: Option<bool>) {
    terrio::crs::set_network_enabled(enabled);
}

/// Parses a nodata argument: `None` for the datasets' own, `"none"` to ignore them, a number, or
/// one number per band.
fn nodata_arg(v: Option<&Bound<'_, PyAny>>) -> PyResult<Nodata> {
    let Some(v) = v else { return Ok(Nodata::Auto) };
    if let Ok(s) = v.extract::<String>() {
        return Nodata::parse(&s).py();
    }
    if let Ok(x) = v.extract::<f64>() {
        return Ok(Nodata::Values(vec![x]));
    }
    Ok(Nodata::Values(v.extract::<Vec<f64>>().map_err(|_| invalid("nodata is a number, a list of numbers or 'none'"))?))
}

/// The rasters given to an operation: one path or `Raster`, or a sequence of them, with the
/// paths (or made-up names for rasters in memory).
pub(crate) fn input_rasters(py: Python<'_>, src: &Bound<'_, PyAny>) -> PyResult<Vec<(String, RasterDataset)>> {
    let items: Vec<Bound<'_, PyAny>> = if src.extract::<String>().is_ok() || src.cast::<PyRaster>().is_ok() {
        vec![src.clone()]
    } else {
        src.extract().map_err(|_| invalid("expected a path, a terrio.Raster, or a sequence of them"))?
    };
    if items.is_empty() {
        return Err(invalid("no input rasters"));
    }
    // Sources are told apart by name: rasters in memory get one of their own.
    let name = |k: usize, i: &Bound<'_, PyAny>| i.extract::<String>().unwrap_or_else(|_| format!("{MEMORY_PREFIX}{k}>"));
    items.iter().enumerate().map(|(k, i)| Ok((name(k, i), input_raster(py, i)?))).collect()
}

/// The start of the names given to rasters in memory.
const MEMORY_PREFIX: &str = "<terrio.Raster ";

/// Writes an operation's output when a path is given, and wraps it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_raster(
    py: Python<'_>,
    out: RasterDataset,
    dst: Option<&str>,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    if let Some(path) = dst {
        let o = write_options(format, compress, tile_size, bigtiff)?;
        py.detach(|| terrio::write_raster_dataset(&out, path, &o)).py()?;
    }
    Ok(PyRaster::from_dataset(out))
}

/// Subsets, resizes, rescales or converts a raster, like `terrio translate` (GDAL's
/// `gdal_translate`).
///
/// Args:
///     src: The raster: a path or a `Raster`.
///     dst: Also write the result to this path.
///     bands: The bands to keep, numbered from 1. All by default.
///     ot: The output data type: `"Byte"`, `"Int16"`, `"UInt16"`, `"Int32"`, `"Float32"`...
///     srcwin: A window in pixels, `(col_off, row_off, width, height)`.
///     projwin: A window in georeferenced coordinates, `(ulx, uly, lrx, lry)`.
///     projwin_srs: The CRS of `projwin`.
///     outsize: The output size `(width, height)`, each in pixels or a percentage such as
///         `"50%"`; 0 keeps the aspect ratio.
///     tr: The output resolution `(xres, yres)`.
///     resampling: The resampling when resizing: `"near"`, `"bilinear"`, `"cubic"`,
///         `"cubicspline"`, `"lanczos"`, `"average"`, `"rms"` or `"mode"`.
///     scale: Rescale values: `True` (each band's range to the output type's),
///         `(src_min, src_max)` or `(src_min, src_max, dst_min, dst_max)`.
///     exponent: Apply a power function when rescaling.
///     a_nodata: Assign a nodata value, or `"none"` to remove it.
///     a_srs: Assign a CRS.
///     a_ullr: Assign the bounds `(ulx, uly, lrx, lry)`.
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///     tile_size: GeoTIFF/COG tile size, for `dst`.
///     bigtiff: Write a BigTIFF, for `dst`.
///
/// Returns:
///     Raster: The result, in memory.
#[pyfunction]
#[pyo3(signature = (
    src, dst=None, *, bands=None, ot=None, srcwin=None, projwin=None, projwin_srs=None, outsize=None, tr=None,
    resampling=None, scale=None, exponent=None, a_nodata=None, a_srs=None, a_ullr=None, format=None, compress=None,
    tile_size=None, bigtiff=false
))]
#[allow(clippy::too_many_arguments)]
fn translate(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    bands: Option<Vec<usize>>,
    ot: Option<&str>,
    srcwin: Option<(f64, f64, f64, f64)>,
    projwin: Option<(f64, f64, f64, f64)>,
    projwin_srs: Option<&str>,
    outsize: Option<(Bound<'_, PyAny>, Bound<'_, PyAny>)>,
    tr: Option<(f64, f64)>,
    resampling: Option<&str>,
    scale: Option<&Bound<'_, PyAny>>,
    exponent: Option<f64>,
    a_nodata: Option<&Bound<'_, PyAny>>,
    a_srs: Option<&str>,
    a_ullr: Option<(f64, f64, f64, f64)>,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    let size = |v: &Bound<'_, PyAny>| -> PyResult<Size> {
        match v.extract::<usize>() {
            Ok(n) => Ok(Size::Pixels(n)),
            Err(_) => Size::parse(&v.extract::<String>().map_err(|_| invalid("outsize values are ints or percentages"))?).py(),
        }
    };
    let out_size = outsize.map(|(w, h)| Ok::<_, PyErr>((size(&w)?, size(&h)?))).transpose()?;
    let scale = match scale {
        None => None,
        Some(v) if v.extract::<bool>().is_ok() => v.extract::<bool>()?.then(Scale::default),
        Some(v) => match v.extract::<Vec<f64>>().map_err(|_| invalid("scale is True or a tuple of 2 or 4 numbers"))?[..] {
            [a, b] => Some(Scale { src: Some((a, b)), dst: None }),
            [a, b, c, d] => Some(Scale { src: Some((a, b)), dst: Some((c, d)) }),
            _ => return Err(invalid("scale is True or a tuple of 2 or 4 numbers")),
        },
    };
    let assign_nodata = match a_nodata {
        None => None,
        Some(v) => match v.extract::<f64>() {
            Ok(x) => Some(Some(x)),
            Err(_) if v.extract::<String>().is_ok_and(|s| s.eq_ignore_ascii_case("none")) => Some(None),
            Err(_) => return Err(invalid("a_nodata is a number or 'none'")),
        },
    };
    let o = TranslateOptions {
        bands: bands.unwrap_or_default(),
        output_type: ot.map(terrio::ops::parse_data_type).transpose().py()?,
        src_win: srcwin,
        proj_win: projwin,
        proj_win_srs: srs(projwin_srs)?,
        out_size,
        resolution: tr,
        resampling: resampling.map(Resampling::parse).transpose().py()?,
        scale,
        exponent,
        assign_nodata,
        assign_srs: srs(a_srs)?,
        assign_ullr: a_ullr,
    };
    let ds = input_raster(py, src)?;
    let out = py.detach(|| terrio::ops::translate::translate(&ds, &o)).py()?;
    finish_raster(py, out, dst, format, compress, tile_size, bigtiff)
}

/// Reprojects, resamples, crops or mosaics rasters, like `terrio warp` (GDAL's `gdalwarp`).
///
/// Args:
///     src: The rasters: a path or a `Raster`, or a sequence of them.
///     dst: Also write the result to this path.
///     s_srs: The sources' CRS, overriding what they declare.
///     t_srs: The output CRS: `"EPSG:4326"`, WKT or a PROJ string. By default, the first
///         source's.
///     tr: The output resolution `(xres, yres)`, in output CRS units.
///     te: The output extent `(min_x, min_y, max_x, max_y)`, in output CRS units (or `te_srs`).
///     te_srs: The CRS of `te`.
///     ts: The output size `(width, height)`; 0 computes one from the other.
///     dst_transform: The output geotransform `(x0, a, b, y0, d, e)`, in GDAL's order, for a
///         rotated output grid (or any affine one). Needs `ts`, and excludes `te`, `tr` and
///         `tap`.
///     tap: Align the extent on multiples of `tr`.
///     resampling: `"near"`, `"bilinear"`, `"cubic"`, `"cubicspline"`, `"lanczos"`,
///         `"average"`, `"rms"`, `"mode"`, `"min"`, `"max"`, `"med"`, `"q1"`, `"q3"` or `"sum"`.
///     overview: Source pyramid selection: `"AUTO"` (default), `"AUTO-n"`, `"NONE"` or
///         a zero-based stored level. The output grid still uses full-resolution metadata.
///     srcnodata: The sources' nodata: a number, one per band, or `"none"` to ignore theirs.
///     dstnodata: The output nodata, or `"none"`. By default, the sources'.
///     ot: The output data type.
///     cutline: Polygons masking the sources: a vector path, a `Layer`, a `VectorDataset` or
///         GeoJSON.
///     crop_to_cutline: Set the output extent to the cutline's.
///     et: The maximum error of the approximate transformer, in pixels (0 for exact).
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///     tile_size: GeoTIFF/COG tile size, for `dst`.
///     bigtiff: Write a BigTIFF, for `dst`.
///     gcps: Ground control points georeferencing the (first) source in place of its
///         geotransform, in its CRS: `(pixel, line, x, y, z)` tuples.
///     order: The GCP polynomial order (1, 2 or 3; 0 picks GDAL's default), as `-order`.
///     gcp_method: How the GCPs georeference the source: `"auto"` (GDAL's choice: a homography
///         from 4 or 5 GCPs, else a polynomial), `"polynomial"`, `"tps"` (as `-tps`) or
///         `"homography"`.
///     rpc: GDAL's `RPC` metadata items georeferencing the (first) source instead, as
///         `gdalwarp -rpc` (its CRS then WGS 84, unless `s_srs`).
///     to: Transformer options, as `gdalwarp -to`: the RPC transformer's `RPC_HEIGHT`,
///         `RPC_HEIGHT_SCALE`, `RPC_DEM`, `RPC_DEMINTERPOLATION`, `RPC_DEM_MISSING_VALUE`,
///         `RPC_DEM_SRS`, `RPC_DEM_APPLY_VDATUM_SHIFT`, `RPC_PIXEL_ERROR_THRESHOLD` and
///         `RPC_MAX_ITERATIONS`.
///     geoloc: Require geolocation-array metadata on each source.
///     geolocation: First source coordinate rasters: `x`, `y`, optional `srs`,
///         `pixel_offset`, `line_offset`, `pixel_step`, `line_step`, `pixel_center`.
///     wo: Warp options, as `gdalwarp -wo`: `SOURCE_EXTRA` and `SAMPLE_STEPS` (`ALL`); others
///         are ignored.
///
/// Returns:
///     Raster: The result, in memory.
#[pyfunction]
#[pyo3(signature = (
    src, dst=None, *, s_srs=None, t_srs=None, tr=None, te=None, te_srs=None, ts=None, dst_transform=None, tap=false,
    resampling="near", overview=None, srcnodata=None, dstnodata=None, ot=None, cutline=None, crop_to_cutline=false, et=0.125,
    format=None, compress=None, tile_size=None, bigtiff=false, gcps=None, order=0, gcp_method="auto",
    rpc=None, to=None, wo=None, geoloc=false, geolocation=None, kernel_version="3.12"
))]
#[allow(clippy::too_many_arguments)]
fn warp(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    s_srs: Option<&str>,
    t_srs: Option<&str>,
    tr: Option<(f64, f64)>,
    te: Option<(f64, f64, f64, f64)>,
    te_srs: Option<&str>,
    ts: Option<(usize, usize)>,
    dst_transform: Option<[f64; 6]>,
    tap: bool,
    resampling: &str,
    overview: Option<&Bound<'_, PyAny>>,
    srcnodata: Option<&Bound<'_, PyAny>>,
    dstnodata: Option<&Bound<'_, PyAny>>,
    ot: Option<&str>,
    cutline: Option<&Bound<'_, PyAny>>,
    crop_to_cutline: bool,
    et: f64,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
    gcps: Option<Vec<GcpTuple>>,
    order: u32,
    gcp_method: &str,
    rpc: Option<std::collections::HashMap<String, String>>,
    to: Option<std::collections::HashMap<String, String>>,
    wo: Option<std::collections::HashMap<String, String>>,
    geoloc: bool,
    geolocation: Option<&Bound<'_, pyo3::types::PyDict>>,
    kernel_version: &str,
) -> PyResult<PyRaster> {
    if tap && tr.is_none() {
        return Err(invalid("tap needs tr"));
    }
    let cutline = match cutline {
        None => None,
        Some(c) => {
            let layers = input_layers(py, c, None)?;
            let ds = terrio::VectorDataset { layers, driver: VectorDriver::GeoJSON };
            Some(Cutline::from_vector(&ds).py()?)
        }
    };
    let mut o = WarpOptions {
        src_srs: srs(s_srs)?,
        dst_srs: srs(t_srs)?,
        resolution: tr,
        extent: te,
        extent_srs: srs(te_srs)?,
        size: ts,
        dst_transform,
        target_aligned_pixels: tap,
        resampling: Resampling::parse(resampling).py()?,
        overview: match overview {
            None => terrio::ops::overview::OverviewSelection::Auto,
            Some(v) if v.is_instance_of::<PyBool>() => return Err(invalid("overview must not be a boolean")),
            Some(v) => match v.extract::<usize>() {
                Ok(level) => terrio::ops::overview::OverviewSelection::Level(level),
                Err(_) => terrio::ops::overview::OverviewSelection::parse(
                    &v.extract::<String>().map_err(|_| invalid("overview is AUTO, AUTO-n, NONE or a level from 0"))?,
                )
                .py()?,
            },
        },
        src_nodata: nodata_arg(srcnodata)?,
        dst_nodata: nodata_arg(dstnodata)?,
        output_type: ot.map(terrio::ops::parse_data_type).transpose().py()?,
        cutline,
        crop_to_cutline,
        error_threshold: et,
        gcps: crate::raster::parse_gcps(gcps),
        order,
        gcp_method: terrio::ops::gcp::GcpMethod::parse(gcp_method).py()?,
        rpc: rpc.map(crate::rio::parse_rpc).transpose()?,
        transform_options: TransformOptions::from_options(&to.clone().unwrap_or_default().into_iter().collect::<Vec<_>>()).py()?,
        kernel_version: terrio::ops::warp::KernelVersion::parse(kernel_version).py()?,
        rpc_options: crate::rio::parse_rpc_options(to)?,
        geoloc,
        ..Default::default()
    };
    for (k, v) in wo.unwrap_or_default() {
        match k.to_ascii_uppercase().as_str() {
            "SOURCE_EXTRA" => o.source_extra = Some(v.trim().parse().map_err(|_| invalid("SOURCE_EXTRA must be an integer"))?),
            "SAMPLE_STEPS" => o.sample_steps_all = v.eq_ignore_ascii_case("ALL"),
            _ => {}
        }
    }
    enum Input {
        File(Box<terrio::LazyRaster>),
        Python(Py<PyRaster>),
    }
    impl terrio::ops::warp::WarpSource for Input {
        fn with_dataset(&self, visit: &mut dyn FnMut(&RasterDataset)) -> terrio::Result<bool> {
            match self {
                Self::File(r) => terrio::ops::warp::WarpSource::with_dataset(r.as_ref(), visit),
                Self::Python(obj) => Python::attach(|py| {
                    let r = obj.bind(py).try_borrow().map_err(|e| terrio::Error::InvalidArgument(e.to_string()))?;
                    terrio::ops::warp::WarpSource::with_dataset(&r.inner, visit)
                }),
            }
        }
        fn info(&mut self) -> terrio::Result<terrio::ops::warp::WarpSourceInfo> {
            match self {
                Self::File(r) => terrio::ops::warp::WarpSource::info(r.as_mut()),
                Self::Python(obj) => Python::attach(|py| {
                    let mut r = obj.bind(py).try_borrow_mut().map_err(|e| terrio::Error::InvalidArgument(e.to_string()))?;
                    terrio::ops::warp::WarpSource::info(&mut r.inner)
                }),
            }
        }
        fn read_window(&mut self, window: Window, options: &WindowOptions) -> terrio::Result<RasterDataset> {
            match self {
                Self::File(r) => r.read_window(window, options),
                Self::Python(obj) => Python::attach(|py| {
                    let mut r = obj.bind(py).try_borrow_mut().map_err(|e| terrio::Error::InvalidArgument(e.to_string()))?;
                    let lazy = &mut r.inner;
                    py.detach(|| lazy.read_window(window, options))
                }),
            }
        }
    }
    enum Arg {
        Path(String),
        Raster(Py<PyRaster>),
    }
    let items: Vec<Bound<'_, PyAny>> = if src.cast::<PyRaster>().is_ok() || src.extract::<std::path::PathBuf>().is_ok() {
        vec![src.clone()]
    } else {
        src.extract().map_err(|_| invalid("expected a raster path, Raster, or sequence of them"))?
    };
    let args = items
        .iter()
        .map(|v| {
            if let Ok(r) = v.extract::<Py<PyRaster>>() {
                return Ok(Arg::Raster(r));
            }
            let p = v.extract::<std::path::PathBuf>().map_err(|_| invalid("expected a raster path or Raster"))?;
            Ok(Arg::Path(p.into_os_string().into_string().map_err(|_| invalid("raster paths must be UTF-8"))?))
        })
        .collect::<PyResult<Vec<_>>>()?;
    if args.is_empty() {
        return Err(invalid("no input rasters"));
    }
    if let Some(path) = dst
        && (format.is_some_and(|f| f.eq_ignore_ascii_case("VRT"))
            || (format.is_none() && std::path::Path::new(path).extension().is_some_and(|e| e.eq_ignore_ascii_case("vrt"))))
    {
        if geolocation.is_some() {
            return Err(terrio::Error::Unsupported("serialized geolocation transformer".into())).py();
        }
        let source = match args.as_slice() {
            [Arg::Path(path)] => path.clone(),
            _ => return Err(invalid("serialized warped VRT output needs a single source path")),
        };
        let options = write_options(format, compress, tile_size, bigtiff)?;
        py.detach(|| terrio::ops::warp::warp_to_path(&[source], &o, path, &options)).py()?;
        return Ok(PyRaster { inner: py.detach(|| terrio::open_raster_lazy(path)).py()? });
    }
    let first_path = match &args[0] {
        Arg::Path(path) => Some(path.clone()),
        Arg::Raster(_) => None,
    };
    let mut inputs = args
        .into_iter()
        .map(|arg| match arg {
            Arg::Path(path) => py.detach(|| terrio::open_raster_lazy(&path)).py().map(Box::new).map(Input::File),
            Arg::Raster(raster) => Ok(Input::Python(raster)),
        })
        .collect::<PyResult<Vec<_>>>()?;
    if let Some(g) = geolocation {
        let (width, height) = match &inputs[0] {
            Input::File(reader) => (reader.width(), reader.height()),
            Input::Python(obj) => {
                let raster = obj.bind(py).try_borrow()?;
                (raster.inner.width(), raster.inner.height())
            }
        };
        for (key, _) in g.iter() {
            let key = key.extract::<String>()?;
            if !["x", "y", "srs", "x_band", "y_band", "pixel_offset", "line_offset", "pixel_step", "line_step", "pixel_center"]
                .contains(&key.as_str())
            {
                return Err(invalid(format!("unknown geolocation option '{key}'")));
            }
        }
        let x = g.get_item("x")?.ok_or_else(|| invalid("geolocation needs x and y rasters"))?;
        let y = g.get_item("y")?.ok_or_else(|| invalid("geolocation needs x and y rasters"))?;
        let xs = input_rasters(py, &x)?;
        let ys = input_rasters(py, &y)?;
        if xs.len() != 1 || ys.len() != 1 {
            return Err(invalid("geolocation needs one x and one y raster"));
        }
        let number = |key: &str, default: f64| -> PyResult<f64> {
            g.get_item(key)?.map(|v| v.extract::<f64>()).transpose().map(|v| v.unwrap_or(default))
        };
        let metadata = terrio::Geolocation {
            srs: g.get_item("srs")?.map(|v| v.extract::<String>()).transpose()?.unwrap_or_else(|| "EPSG:4326".into()),
            x_dataset: String::new(),
            y_dataset: String::new(),
            x_band: g.get_item("x_band")?.map(|v| v.extract::<usize>()).transpose()?.unwrap_or(1),
            y_band: g.get_item("y_band")?.map(|v| v.extract::<usize>()).transpose()?.unwrap_or(1),
            pixel_offset: number("pixel_offset", 0.0)?,
            line_offset: number("line_offset", 0.0)?,
            pixel_step: number("pixel_step", 1.0)?,
            line_step: number("line_step", 1.0)?,
            pixel_center: g.get_item("pixel_center")?.map(|v| v.extract::<bool>()).transpose()?.unwrap_or(false),
        };
        o.geolocation = Some(terrio::ops::geolocation::GeolocationTransformer::new(&metadata, &xs[0].1, &ys[0].1, width, height).py()?);
    }
    let out = crate::with_warnings(py, || {
        let prepared = if let Some(path) = first_path.as_deref() {
            let header = terrio::ops::warp::WarpSource::info(&mut inputs[0])?.raster;
            terrio::ops::warp::with_file_gcps(&o, path, &header)?
        } else {
            std::borrow::Cow::Borrowed(&o)
        };
        let mut refs = inputs.iter_mut().map(|r| r as &mut dyn terrio::ops::warp::WarpSource).collect::<Vec<_>>();
        terrio::ops::warp::warp_sources(&mut refs, &prepared)
    })?;
    finish_raster(py, out, dst, format, compress, tile_size, bigtiff)
}

/// Mosaics rasters into one, like `terrio mosaic` (GDAL's `gdalbuildvrt` then
/// `gdal_translate`). A `.vrt` destination is written as a virtual mosaic referencing the
/// sources, which must then be paths.
///
/// Args:
///     srcs: The rasters: paths or `Raster`s.
///     dst: Also write the mosaic to this path (a `.vrt` writes a virtual mosaic).
///     resolution: `"highest"`, `"lowest"` or `"average"` of the sources' resolutions.
///     tr: The output resolution `(xres, yres)`, overriding `resolution`.
///     te: The output extent `(min_x, min_y, max_x, max_y)`.
///     tap: Align the extent on multiples of the resolution.
///     separate: Put each source in its own band instead of mosaicking them.
///     bands: The bands to use, numbered from 1.
///     nodata: Ignore source pixels with this value and use it as the output nodata.
///     srcnodata: The sources' nodata: a number, one per band, or `"none"`.
///     dstnodata: The output nodata, or `"none"`. By default, the sources'.
///     resampling: The resampling when sources are resampled or reprojected.
///     t_srs: The output CRS; sources in other CRSs are reprojected. By default, the first
///         source's.
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///     tile_size: GeoTIFF/COG tile size, for `dst`.
///     bigtiff: Write a BigTIFF, for `dst`.
///
/// Returns:
///     Raster | None: The mosaic, in memory; `None` when a `.vrt` was written.
#[pyfunction]
#[pyo3(signature = (
    srcs, dst=None, *, resolution="average", tr=None, te=None, tap=false, separate=false, bands=None, nodata=None,
    srcnodata=None, dstnodata=None, resampling="near", t_srs=None, format=None, compress=None, tile_size=None,
    bigtiff=false
))]
#[allow(clippy::too_many_arguments)]
fn mosaic(
    py: Python<'_>,
    srcs: &Bound<'_, PyAny>,
    dst: Option<&str>,
    resolution: &str,
    tr: Option<(f64, f64)>,
    te: Option<(f64, f64, f64, f64)>,
    tap: bool,
    separate: bool,
    bands: Option<Vec<usize>>,
    nodata: Option<&Bound<'_, PyAny>>,
    srcnodata: Option<&Bound<'_, PyAny>>,
    dstnodata: Option<&Bound<'_, PyAny>>,
    resampling: &str,
    t_srs: Option<&str>,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<Option<PyRaster>> {
    let resolution = match tr {
        Some((x, y)) => ResolutionStrategy::User(x, y),
        None => ResolutionStrategy::parse(resolution).py()?,
    };
    let nodata = nodata.map(|n| nodata_arg(Some(n))).transpose()?;
    let pick = |v: Option<&Bound<'_, PyAny>>| -> PyResult<Nodata> {
        match (v, &nodata) {
            (Some(v), _) => nodata_arg(Some(v)),
            (None, Some(n)) => Ok(n.clone()),
            (None, None) => Ok(Nodata::Auto),
        }
    };
    let o = MosaicOptions {
        resolution,
        extent: te,
        target_aligned_pixels: tap,
        separate,
        bands: bands.unwrap_or_default(),
        src_nodata: pick(srcnodata)?,
        dst_nodata: pick(dstnodata)?,
        resampling: Resampling::parse(resampling).py()?,
        dst_srs: srs(t_srs)?,
    };
    let inputs = input_rasters(py, srcs)?;
    if let Some(path) = dst.filter(|d| d.to_ascii_lowercase().ends_with(".vrt")) {
        if inputs.iter().any(|(p, _)| p.starts_with(MEMORY_PREFIX)) {
            return Err(invalid("a .vrt mosaic references its sources: give them as paths"));
        }
        py.detach(|| {
            let sources: Vec<MosaicSource> = inputs.iter().map(|(p, ds)| MosaicSource { path: p.clone(), ds }).collect();
            let dir = std::path::Path::new(path).parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
            Ok(std::fs::write(path, terrio::ops::mosaic::build_vrt(&sources, &o, Some(dir))?.to_xml())?)
        })
        .py()?;
        return Ok(None);
    }
    let out = py
        .detach(|| {
            let sources: Vec<MosaicSource> = inputs.iter().map(|(p, ds)| MosaicSource { path: p.clone(), ds }).collect();
            terrio::ops::mosaic::mosaic(&sources, &o)
        })
        .py()?;
    finish_raster(py, out, dst, format, compress, tile_size, bigtiff).map(Some)
}

/// Opens an indexed raster catalog lazily. `index` is a GTI/index path or a vector layer;
/// `sources` optionally maps location strings to in-memory rasters for WASM-style use.
#[pyfunction]
#[pyo3(signature = (index, sources=None, *, layer=None, location_field=None, sort_field=None, sort_descending=None, tr=None, te=None, srs=None, bands=None, ot=None, nodata=None, alpha_band=None, cache_size=None, resampling=None))]
#[allow(clippy::too_many_arguments)]
fn open_catalog(
    py: Python<'_>,
    index: &Bound<'_, PyAny>,
    sources: Option<&Bound<'_, PyAny>>,
    layer: Option<String>,
    location_field: Option<String>,
    sort_field: Option<String>,
    sort_descending: Option<bool>,
    tr: Option<(f64, f64)>,
    te: Option<(f64, f64, f64, f64)>,
    srs: Option<&str>,
    bands: Option<usize>,
    ot: Option<&str>,
    nodata: Option<f64>,
    alpha_band: Option<usize>,
    cache_size: Option<usize>,
    resampling: Option<&str>,
) -> PyResult<PyRaster> {
    let options = terrio::catalog::CatalogOptions {
        layer,
        location_field,
        sort_field,
        sort_descending,
        resolution: tr,
        extent: te,
        srs: srs.map(SpatialRef::parse).transpose().py()?,
        bands,
        data_type: ot.map(terrio::ops::parse_data_type).transpose().py()?,
        nodata,
        alpha_band,
        cache_size,
        resampling: resampling.map(Resampling::parse).transpose().py()?,
    };
    let catalog = if sources.is_none()
        && let Ok(path) = index.extract::<String>()
    {
        py.detach(|| terrio::catalog::RasterCatalog::open(&path, options)).py()?
    } else {
        let mut layers = input_layers(py, index, options.layer.clone().map(|name| vec![name]))?;
        if layers.len() != 1 {
            return Err(invalid("an in-memory catalog needs one index layer"));
        }
        let mut rasters = std::collections::HashMap::new();
        if let Some(sources) = sources {
            for item in sources.call_method0("items")?.try_iter()? {
                let (name, value): (String, Bound<'_, PyAny>) = item?.extract()?;
                let input = if let Some(path) = raster_path(&value)? {
                    terrio::catalog::CatalogInput::path(path)
                } else {
                    let mut raster = value.cast::<PyRaster>()?.try_borrow_mut()?;
                    terrio::catalog::CatalogInput::raster(raster.inner.shared())
                };
                rasters.insert(name, input);
            }
        }
        let layer = layers.remove(0);
        py.detach(|| terrio::catalog::RasterCatalog::from_sources(layer, rasters, options)).py()?
    };
    Ok(PyRaster { inner: catalog.into_lazy() })
}
