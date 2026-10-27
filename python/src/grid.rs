//! Gridding scattered vector points (`terrio vector-grid`, GDAL's `gdal_grid`) and linear
//! referencing (`terrio lineref`, GDAL's `ogrlineref`).

use crate::ops::{data_type, srs, vector_write_options};
use crate::raster::{PyRaster, write_options};
use crate::vector::{PyLayer, PyVectorDataset, layers_from_object};
use crate::{PyResultExt, invalid};
use pyo3::prelude::*;
use terrio::ops::lineref::LineRefOptions;
use terrio::ops::vector_filter::WriteMode;
use terrio::ops::vector_grid::{GridAlgorithm, GridParams, VectorGridOptions};
use terrio::{DataType, RasterDataset};

/// Registers the functions in the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(vector_grid, m)?)?;
    m.add_function(wrap_pyfunction!(lineref_create, m)?)?;
    m.add_function(wrap_pyfunction!(lineref_position, m)?)?;
    m.add_function(wrap_pyfunction!(lineref_coordinates, m)?)?;
    m.add_function(wrap_pyfunction!(lineref_subline, m)?)?;
    Ok(())
}

/// Opens the layers given to an operation: a path, or layers in memory.
fn input_layers(py: Python<'_>, src: &Bound<'_, PyAny>, layers: Option<Vec<String>>) -> PyResult<Vec<terrio::Layer>> {
    match src.extract::<String>() {
        Ok(path) => {
            let options = terrio::VectorOpenOptions { layers: layers.unwrap_or_default(), ..Default::default() };
            py.detach(|| terrio::ops::vector_filter::open_layers(&path, &options)).py()
        }
        Err(_) => {
            let all = layers_from_object(src, None, None)?;
            Ok(match layers {
                Some(names) => terrio::ops::vector_filter::select_layers(all, &names).py()?,
                None => all,
            })
        }
    }
}

/// Grids scattered vector points into a raster, like `terrio vector-grid` (GDAL's
/// `gdal vector grid` and `gdal_grid`).
///
/// Args:
///     src: The points: a path, a `Layer`, a `VectorDataset`, a list of layers, or GeoJSON. The
///         vertices of lines and polygons count as points.
///     dst: Also write the raster to this path.
///     algorithm: `"invdist"` (the default), `"invdistnn"`, `"average"`, `"nearest"`,
///         `"linear"`, `"minimum"`, `"maximum"`, `"range"`, `"count"`, `"average_distance"` or
///         `"average_distance_pts"`, or a GDAL algorithm string such as
///         `"invdist:power=3:radius=500"`; the keyword parameters override it.
///     zfield: Take the values from this field rather than from the geometries' Z.
///     zoffset: Add this to the values (before `zmultiply`).
///     zmultiply: Multiply the values by this.
///     layers: The layers to grid, one band each. Needed when the dataset has several.
///     sql: Grid the result of `SELECT fields FROM layer [WHERE ...]` instead.
///     where: Only grid the features matching this OGR SQL expression.
///     bbox: Only grid the points strictly inside `(min_x, min_y, max_x, max_y)`.
///     extent: The grid's extent `(min_x, min_y, max_x, max_y)`. By default, the layer's.
///     size: The size in pixels `(width, height)`; 256 x 256 by default.
///     resolution: The pixel size `(xres, yres)`, with `extent`.
///     ot: The output data type (`"Float64"` by default).
///     crs: The output CRS (by default, the layer's). Points are not reprojected.
///     power: The weighting power (invdist, invdistnn).
///     smoothing: The smoothing (invdist, invdistnn).
///     radius: The search radius: both semi-axes of the ellipse, or invdistnn's and linear's.
///     radius1: The first semi-axis of the search ellipse, with `radius2`.
///     radius2: The second semi-axis of the search ellipse, with `radius1`.
///     angle: The rotation of the search ellipse, in degrees counter-clockwise.
///     min_points: The fewest points for a value.
///     max_points: The most points used.
///     min_points_per_quadrant: The fewest points in each quadrant around the node.
///     max_points_per_quadrant: The most points used from each quadrant.
///     nodata: The value of nodes without enough points (0 by default); declared as the bands'
///         nodata when given, as gdal_grid does.
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///
/// Returns:
///     Raster: The raster, in memory, one band per layer.
#[pyfunction]
#[pyo3(signature = (
    src, dst=None, *, algorithm="invdist", zfield=None, zoffset=0.0, zmultiply=1.0, layers=None, sql=None, r#where=None,
    bbox=None, extent=None, size=None, resolution=None, ot=None, crs=None, power=None, smoothing=None, radius=None,
    radius1=None, radius2=None, angle=None, min_points=None, max_points=None, min_points_per_quadrant=None,
    max_points_per_quadrant=None, nodata=None, format=None, compress=None
))]
#[allow(clippy::too_many_arguments)]
fn vector_grid(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    algorithm: &str,
    zfield: Option<String>,
    zoffset: f64,
    zmultiply: f64,
    layers: Option<Vec<String>>,
    sql: Option<String>,
    r#where: Option<String>,
    bbox: Option<(f64, f64, f64, f64)>,
    extent: Option<(f64, f64, f64, f64)>,
    size: Option<(usize, usize)>,
    resolution: Option<(f64, f64)>,
    ot: Option<&str>,
    crs: Option<&str>,
    power: Option<f64>,
    smoothing: Option<f64>,
    radius: Option<f64>,
    radius1: Option<f64>,
    radius2: Option<f64>,
    angle: Option<f64>,
    min_points: Option<u32>,
    max_points: Option<u32>,
    min_points_per_quadrant: Option<u32>,
    max_points_per_quadrant: Option<u32>,
    nodata: Option<f64>,
    format: Option<&str>,
    compress: Option<String>,
) -> PyResult<PyRaster> {
    if layers.as_ref().is_some_and(Vec::is_empty) {
        return Err(invalid("give at least one layer name"));
    }
    let params = GridParams {
        power,
        smoothing,
        radius,
        radius1,
        radius2,
        angle,
        min_points,
        max_points,
        min_points_per_quadrant,
        max_points_per_quadrant,
        nodata,
    };
    let algorithm = GridAlgorithm::with_params(algorithm, &params).py()?;
    let named = layers.is_some();
    if named && sql.is_some() {
        return Err(invalid("give layers or sql, not both"));
    }
    let all = input_layers(py, src, layers)?;
    let options = VectorGridOptions {
        algorithm,
        z_field: zfield,
        z_offset: zoffset,
        z_multiply: zmultiply,
        where_clause: r#where,
        bbox,
        extent,
        size,
        resolution,
        data_type: ot.map(data_type).transpose()?.unwrap_or(DataType::F64),
        crs: srs(crs)?,
    };
    let write = dst.map(|_| write_options(format, compress, None, false)).transpose()?;
    let out = crate::with_warnings(py, || -> terrio::Result<RasterDataset> {
        let out = terrio::ops::vector_grid::vector_grid_from_layers(&all, sql.as_deref(), named, &options)?;
        if let (Some(path), Some(o)) = (dst, &write) {
            terrio::write_raster_dataset(&out, path, o)?;
        }
        Ok(out)
    })?;
    Ok(PyRaster::from_dataset(out))
}

/// The one layer of a source: the named one, or the first.
fn one_layer(py: Python<'_>, src: &Bound<'_, PyAny>, layer: Option<String>) -> PyResult<terrio::Layer> {
    input_layers(py, src, layer.map(|l| vec![l]))?.into_iter().next().ok_or_else(|| invalid("no layer"))
}

/// Borrow an in-memory layer for lookups; paths and serialized inputs are opened once.
fn with_one_layer<T>(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    name: Option<&str>,
    apply: impl FnOnce(&terrio::Layer) -> PyResult<T>,
) -> PyResult<T> {
    if let Ok(layer) = src.cast::<PyLayer>() {
        let borrowed = layer.borrow();
        if let Some(name) = name.filter(|name| *name != borrowed.inner.name) {
            return Err(invalid(format!("no layer named {name}")));
        }
        return apply(&borrowed.inner);
    }
    if let Ok(dataset) = src.cast::<PyVectorDataset>() {
        return dataset.borrow().with_layer(py, name, apply);
    }
    apply(&one_layer(py, src, name.map(str::to_owned))?)
}

/// Writes a layer to `dst`, when given.
fn write_layer(py: Python<'_>, layer: &terrio::Layer, dst: Option<&str>, format: Option<&str>) -> PyResult<()> {
    if let Some(path) = dst {
        let o = vector_write_options(format)?;
        py.detach(|| terrio::ops::vector_filter::write_vector_with_mode(std::slice::from_ref(layer), path, &o, WriteMode::Create)).py()?;
    }
    Ok(())
}

/// Cuts a route into parts of equal position length from reference points, like
/// `terrio lineref create` (GDAL's `ogrlineref -create`).
///
/// Args:
///     lines: The route lines: a path, a `Layer`, a `VectorDataset`, or GeoJSON. The first
///         feature is the route, or each route with `line_field`.
///     points: The reference points (milestones), with their positions in `position_field`.
///     position_field: The field of the points' positions.
///     step: The length of the parts, in position units.
///     dst: Also write the parts to this path.
///     lines_layer: The layer of `lines`. By default, the first.
///     points_layer: The layer of `points`. By default, the first.
///     line_field: The field identifying routes, with `point_field`.
///     point_field: The field identifying each point's route, with `line_field`.
///     id_field: The output field receiving route identifiers.
///     layer_name: The output layer name. By default, `dst`'s stem or `"parts"`.
///     format: The output format's short name, for `dst`.
///
/// Returns:
///     Layer: The parts, with `beg`, `end` and `scale` fields.
#[pyfunction]
#[pyo3(signature = (
    lines, points, position_field, step, dst=None, *, lines_layer=None, points_layer=None, line_field=None, point_field=None,
    id_field="uniq_uid", layer_name=None, format=None
))]
#[allow(clippy::too_many_arguments)]
fn lineref_create(
    py: Python<'_>,
    lines: &Bound<'_, PyAny>,
    points: &Bound<'_, PyAny>,
    position_field: &str,
    step: f64,
    dst: Option<&str>,
    lines_layer: Option<String>,
    points_layer: Option<String>,
    line_field: Option<String>,
    point_field: Option<String>,
    id_field: &str,
    layer_name: Option<String>,
    format: Option<&str>,
) -> PyResult<PyLayer> {
    if line_field.is_some() != point_field.is_some() {
        return Err(invalid("line_field and point_field go together"));
    }
    let lines = one_layer(py, lines, lines_layer)?;
    let points = one_layer(py, points, points_layer)?;
    let stem = dst.and_then(|d| std::path::Path::new(d).file_stem()).and_then(|s| s.to_str()).unwrap_or("parts").to_string();
    let options = LineRefOptions {
        position_field: position_field.to_string(),
        step,
        line_id_field: line_field,
        point_id_field: point_field,
        output_id_field: id_field.to_string(),
        layer_name: Some(layer_name.unwrap_or(stem)),
    };
    let inner = crate::with_warnings(py, || terrio::ops::lineref::create_reference(&lines, &points, &options))?;
    write_layer(py, &inner, dst, format)?;
    Ok(PyLayer { inner })
}

/// The position of a point on linear reference parts, like `terrio lineref get-pos` (GDAL's
/// `ogrlineref -get_pos`).
///
/// Args:
///     parts: The parts (from `lineref_create` or ogrlineref): a path, a `Layer`...
///     x: The X coordinate.
///     y: The Y coordinate.
///     layer: The layer of `parts`. By default, the first.
///
/// Returns:
///     float: The position.
#[pyfunction]
#[pyo3(signature = (parts, x, y, *, layer=None))]
fn lineref_position(py: Python<'_>, parts: &Bound<'_, PyAny>, x: f64, y: f64, layer: Option<String>) -> PyResult<f64> {
    with_one_layer(py, parts, layer.as_deref(), |parts| terrio::ops::lineref::position(parts, x, y).py())
}

/// The coordinates of a position on linear reference parts, like `terrio lineref get-coord`
/// (GDAL's `ogrlineref -get_coord`).
///
/// Args:
///     parts: The parts: a path, a `Layer`...
///     position: The position.
///     layer: The layer of `parts`. By default, the first.
///
/// Returns:
///     list[tuple[float, float, float]]: `(x, y, z)` on each part spanning the position.
#[pyfunction]
#[pyo3(signature = (parts, position, *, layer=None))]
fn lineref_coordinates(py: Python<'_>, parts: &Bound<'_, PyAny>, position: f64, layer: Option<String>) -> PyResult<Vec<(f64, f64, f64)>> {
    let coordinates = with_one_layer(py, parts, layer.as_deref(), |parts| terrio::ops::lineref::coordinates(parts, position).py())?;
    Ok(coordinates.into_iter().map(|[x, y, z]| (x, y, z)).collect())
}

/// The route between two positions, like `terrio lineref get-subline` (GDAL's
/// `ogrlineref -get_subline`).
///
/// Args:
///     parts: The parts: a path, a `Layer`...
///     begin: The first position.
///     end: The last position.
///     dst: Also write the line to this path.
///     layer: The layer of `parts`. By default, the first.
///     layer_name: The output layer name. By default, `dst`'s stem or `"subline"`.
///     format: The output format's short name, for `dst`.
///
/// Returns:
///     Layer: One feature, the line, with `beg`, `end` and `scale` fields.
#[pyfunction]
#[pyo3(signature = (parts, begin, end, dst=None, *, layer=None, layer_name=None, format=None))]
#[allow(clippy::too_many_arguments)]
fn lineref_subline(
    py: Python<'_>,
    parts: &Bound<'_, PyAny>,
    begin: f64,
    end: f64,
    dst: Option<&str>,
    layer: Option<String>,
    layer_name: Option<String>,
    format: Option<&str>,
) -> PyResult<PyLayer> {
    let stem = dst.and_then(|d| std::path::Path::new(d).file_stem()).and_then(|s| s.to_str()).unwrap_or("subline").to_string();
    let inner = with_one_layer(py, parts, layer.as_deref(), |parts| {
        terrio::ops::lineref::subline(parts, begin, end, &layer_name.unwrap_or(stem)).py()
    })?;
    write_layer(py, &inner, dst, format)?;
    Ok(PyLayer { inner })
}
