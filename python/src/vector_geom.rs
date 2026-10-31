//! Vector geometry operations: `simplify`, `segmentize`, `convex_hull`, `concave_hull`,
//! `swap_xy`, `set_geom_type`, `combine` and `sort` (GDAL's `gdal vector` commands of those
//! names).

use crate::ops::input_layers;
use crate::vector::PyLayer;
use crate::{Detach, PyResultExt, invalid};
use pyo3::prelude::*;
use terrio::Layer;
use terrio::ops::vector_geom::{
    self, CombineOptions, ConcaveHullOptions, Dimension, ExtraFields, GeometryOp, GeometryTypeSpec, SetGeomTypeOptions, SortMethod,
};

/// Registers the functions in the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(simplify, m)?)?;
    m.add_function(wrap_pyfunction!(check_coverage, m)?)?;
    m.add_function(wrap_pyfunction!(simplify_coverage, m)?)?;
    m.add_function(wrap_pyfunction!(segmentize, m)?)?;
    m.add_function(wrap_pyfunction!(convex_hull, m)?)?;
    m.add_function(wrap_pyfunction!(concave_hull, m)?)?;
    m.add_function(wrap_pyfunction!(swap_xy, m)?)?;
    m.add_function(wrap_pyfunction!(set_geom_type, m)?)?;
    m.add_function(wrap_pyfunction!(combine, m)?)?;
    m.add_function(wrap_pyfunction!(sort, m)?)?;
    Ok(())
}

/// The single layer an operation works on: `src`'s only layer, or the one named `layer`.
fn one_layer(py: Python<'_>, src: &Bound<'_, PyAny>, layer: Option<String>) -> PyResult<Layer> {
    let mut layers = input_layers(py, src, layer.clone().map(|l| vec![l]))?;
    match layers.len() {
        1 => Ok(layers.remove(0)),
        0 => Err(invalid(match layer {
            Some(l) => format!("no layer named '{l}'"),
            None => "no input layer".to_string(),
        })),
        n => Err(invalid(format!("the input has {n} layers: choose one with layer="))),
    }
}

fn run(py: Python<'_>, src: &Bound<'_, PyAny>, layer: Option<String>, op: GeometryOp) -> PyResult<PyLayer> {
    let input = one_layer(py, src, layer)?;
    let inner = py.detach_result(|| vector_geom::apply(&input, &op)).py()?;
    Ok(PyLayer { inner })
}

/// Simplifies geometries, preserving their topology, as `gdal vector simplify` (GEOS's
/// TopologyPreservingSimplifier) does.
///
/// Args:
///     src: A vector path, VectorDataset, Layer, list of layers or GeoJSON.
///     tolerance: The distance tolerance, in CRS units (degrees for geographic CRSs).
///     layer: The layer to use, when the input has several.
///
/// Returns:
///     Layer: The simplified features. Z is kept and M dropped, as GDAL does.
#[pyfunction]
#[pyo3(signature = (src, tolerance, *, layer=None))]
fn simplify(py: Python<'_>, src: &Bound<'_, PyAny>, tolerance: f64, layer: Option<String>) -> PyResult<PyLayer> {
    run(py, src, layer, GeometryOp::Simplify { tolerance })
}

/// Adds vertices so that no segment is longer than `max_length`, as `gdal vector segmentize`.
///
/// Args:
///     src: A vector path, VectorDataset, Layer, list of layers or GeoJSON.
///     max_length: The maximum segment length, in CRS units.
///     layer: The layer to use, when the input has several.
///
/// Returns:
///     Layer: The segmentized features.
#[pyfunction]
#[pyo3(signature = (src, max_length, *, layer=None))]
fn segmentize(py: Python<'_>, src: &Bound<'_, PyAny>, max_length: f64, layer: Option<String>) -> PyResult<PyLayer> {
    run(py, src, layer, GeometryOp::Segmentize { max_length })
}

/// The convex hull of each geometry, as `gdal vector convex-hull`.
///
/// Args:
///     src: A vector path, VectorDataset, Layer, list of layers or GeoJSON.
///     layer: The layer to use, when the input has several.
///
/// Returns:
///     Layer: Polygons (lines or points for degenerate input), with the source attributes.
#[pyfunction]
#[pyo3(signature = (src, *, layer=None))]
fn convex_hull(py: Python<'_>, src: &Bound<'_, PyAny>, layer: Option<String>) -> PyResult<PyLayer> {
    run(py, src, layer, GeometryOp::ConvexHull)
}

/// The concave hull of each geometry (of its polygons for polygonal input), as `gdal vector
/// concave-hull`.
///
/// Args:
///     src: A vector path, VectorDataset, Layer, list of layers or GeoJSON.
///     ratio: The concavity, from 0 (most concave) to 1 (the convex hull).
///     allow_holes: Whether hulls may have holes.
///     tight: For polygons, whether the hull follows the polygons' outer boundaries.
///     layer: The layer to use, when the input has several.
///
/// Returns:
///     Layer: The hulls, with the source attributes.
#[pyfunction]
#[pyo3(signature = (src, ratio, *, allow_holes=false, tight=false, layer=None))]
fn concave_hull(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    ratio: f64,
    allow_holes: bool,
    tight: bool,
    layer: Option<String>,
) -> PyResult<PyLayer> {
    run(py, src, layer, GeometryOp::ConcaveHull(ConcaveHullOptions { ratio, allow_holes, tight }))
}

/// Swaps the X and Y coordinates of every geometry, as `gdal vector swap-xy`.
///
/// Args:
///     src: A vector path, VectorDataset, Layer, list of layers or GeoJSON.
///     layer: The layer to use, when the input has several.
///
/// Returns:
///     Layer: The features with X and Y swapped.
#[pyfunction]
#[pyo3(signature = (src, *, layer=None))]
fn swap_xy(py: Python<'_>, src: &Bound<'_, PyAny>, layer: Option<String>) -> PyResult<PyLayer> {
    run(py, src, layer, GeometryOp::SwapXy)
}

/// Changes geometry types and dimensions, as `gdal vector set-geom-type`.
///
/// Args:
///     src: A vector path, VectorDataset, Layer, list of layers or GeoJSON.
///     geometry_type: Force this type (`"MULTIPOLYGON"`, `"POINTZ"`, `"GEOMETRY"`...).
///     multi: Make single-part types multi-part.
///     single: Make multi-part types single-part.
///     linear: Make curve types linear (a no-op: terrio's geometries are linear).
///     dim: Force the dimensions: `"XY"`, `"XYZ"`, `"XYM"` or `"XYZM"`.
///     layer_only: Only change the layer's declared type.
///     feature_only: Only change the features' geometries.
///     skip: Drop the features that cannot be converted to the target type.
///     layer: The layer to use, when the input has several.
///
/// Returns:
///     Layer: The converted features.
#[pyfunction]
#[pyo3(signature = (src, *, geometry_type=None, multi=false, single=false, linear=false, dim=None, layer_only=false, feature_only=false, skip=false, layer=None))]
#[allow(clippy::too_many_arguments)]
fn set_geom_type(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    geometry_type: Option<&str>,
    multi: bool,
    single: bool,
    linear: bool,
    dim: Option<&str>,
    layer_only: bool,
    feature_only: bool,
    skip: bool,
    layer: Option<String>,
) -> PyResult<PyLayer> {
    let options = SetGeomTypeOptions {
        geometry_type: geometry_type.map(GeometryTypeSpec::parse).transpose().py()?,
        multi,
        single,
        linear,
        curve: false,
        dim: dim.map(Dimension::parse).transpose().py()?,
        layer_only,
        feature_only,
        skip,
    };
    run(py, src, layer, GeometryOp::SetGeomType(options))
}

/// Combines geometries into collections, by groups of field values, as `gdal vector combine`.
///
/// Args:
///     src: A vector path, VectorDataset, Layer, list of layers or GeoJSON.
///     group_by: Group by these fields. By default, the layer becomes one feature.
///     keep_nested: Add multi-part geometries whole, as members of a geometry collection.
///     add_extra_fields: `"no"`, `"sometimes-identical"` or `"always-identical"`.
///     layer: The layer to use, when the input has several.
///
/// Returns:
///     Layer: One feature per group, with the group fields.
#[pyfunction]
#[pyo3(signature = (src, *, group_by=None, keep_nested=false, add_extra_fields="no", layer=None))]
fn combine(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    group_by: Option<Vec<String>>,
    keep_nested: bool,
    add_extra_fields: &str,
    layer: Option<String>,
) -> PyResult<PyLayer> {
    let options = CombineOptions {
        group_by: group_by.unwrap_or_default(),
        keep_nested,
        add_extra_fields: ExtraFields::parse(add_extra_fields).py()?,
    };
    let input = one_layer(py, src, layer)?;
    let inner = py.detach_result(|| vector_geom::combine(&input, &options)).py()?;
    Ok(PyLayer { inner })
}

/// Sorts features by the position of their geometries, as `gdal vector sort`.
///
/// Args:
///     src: A vector path, VectorDataset, Layer, list of layers or GeoJSON.
///     method: `"hilbert"` (Hilbert codes of bounding box centres) or `"strtree"`.
///     layer: The layer to use, when the input has several.
///
/// Returns:
///     Layer: The sorted features; null and empty geometries last.
#[pyfunction]
#[pyo3(signature = (src, *, method="hilbert", layer=None))]
fn sort(py: Python<'_>, src: &Bound<'_, PyAny>, method: &str, layer: Option<String>) -> PyResult<PyLayer> {
    let method = SortMethod::parse(method).py()?;
    let input = one_layer(py, src, layer)?;
    let inner = py.detach_value(|| vector_geom::sort(&input, method))?;
    Ok(PyLayer { inner })
}

/// Find invalid boundary segments in a polygon coverage.
///
/// Args:
///     src: Vector path, dataset, layer or GeoJSON. Polygons must individually be valid.
///     maximum_gap_width: Largest narrow-gap width to flag, in input CRS units; zero disables it.
///     include_valid: Emit empty MultiLineStrings for valid features, preserving correspondence.
///     layer: Select a single layer when the input has multiple layers.
///
/// Returns:
///     Layer: Invalid MultiLineStrings with CRS retained, empty attributes and sequential IDs.
#[pyfunction]
#[pyo3(signature = (src, *, maximum_gap_width=0.0, include_valid=false, layer=None))]
fn check_coverage(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    maximum_gap_width: f64,
    include_valid: bool,
    layer: Option<String>,
) -> PyResult<PyLayer> {
    let input = one_layer(py, src, layer)?;
    let options = terrio::ops::coverage::CoverageCheckOptions { maximum_gap_width, include_valid };
    let inner = py.detach_result(|| terrio::ops::coverage::check_coverage(&input, &options)).py()?;
    Ok(PyLayer { inner })
}

/// Simplify shared polygon coverage boundaries without creating gaps or overlaps.
///
/// Args:
///     src: A polygonal vector path, dataset, layer or GeoJSON forming a valid coverage.
///     tolerance: Square root of the triangle-area threshold, in input CRS units.
///     preserve_boundary: Simplify shared edges only, retaining exterior/hole boundaries.
///     layer: Select a single layer when the input has multiple layers.
///
/// Returns:
///     Layer: Simplified polygons with attributes and CRS retained and new sequential IDs.
#[pyfunction]
#[pyo3(signature = (src, tolerance, *, preserve_boundary=false, layer=None))]
fn simplify_coverage(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    tolerance: f64,
    preserve_boundary: bool,
    layer: Option<String>,
) -> PyResult<PyLayer> {
    let input = one_layer(py, src, layer)?;
    let options = terrio::ops::coverage_simplify::CoverageSimplifyOptions { tolerance, preserve_boundary };
    let inner = py.detach_result(|| terrio::ops::coverage_simplify::simplify_coverage(&input, &options)).py()?;
    Ok(PyLayer { inner })
}
