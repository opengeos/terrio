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
    m.add_function(wrap_pyfunction!(clip_vector, m)?)?;
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

/// Clips vector features to a bbox, polygon geometry, or vector/raster template.
///
/// Args:
///     src: Vector path, Layer, VectorDataset or GeoJSON; choose one layer with layer.
///     bbox: xmin,ymin,xmax,ymax in the input CRS unless crs is supplied.
///     geometry: Polygon/multipolygon WKT or GeoJSON text. GeoJSON defaults to WGS84.
///     like: Vector/raster path, a Raster, or in-memory vector data. Polygon features
///         are unioned; raster bounds use metadata without reading lazy raster pixels.
///     crs: CRS of bbox/geometry, transforming the clip region rather than input data.
///     layer: Input layer to use when src has several.
///     like_layer: Template layer to union.
///     like_where: OGR SQL WHERE subset for template features; exclusive with like_sql.
///     like_sql: Supported SELECT subset for the vector template.
///
/// Returns:
///     Layer: Clipped features with retained attributes and CRS, and sequential IDs.
///         Points/line cuts retain/interpolate Z/M; new polygon topology is XY.
#[pyfunction]
#[pyo3(signature=(src, *, bbox=None, geometry=None, like=None, crs=None, layer=None, like_layer=None, like_where=None, like_sql=None))]
#[allow(clippy::too_many_arguments)]
fn clip_vector(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    bbox: Option<[f64; 4]>,
    geometry: Option<String>,
    like: Option<&Bound<'_, PyAny>>,
    crs: Option<String>,
    layer: Option<String>,
    like_layer: Option<String>,
    like_where: Option<String>,
    like_sql: Option<String>,
) -> PyResult<PyLayer> {
    use terrio::ops::vector_clip::{ClipRegion, ClipTemplateOptions};
    if usize::from(bbox.is_some()) + usize::from(geometry.is_some()) + usize::from(like.is_some()) != 1 {
        return Err(invalid("provide exactly one of bbox, geometry or like"));
    }
    if like.is_none() && (like_layer.is_some() || like_where.is_some() || like_sql.is_some()) {
        return Err(invalid("like_layer/where/sql require like"));
    }
    if like.is_some() && crs.is_some() {
        return Err(invalid("crs applies only to bbox/geometry"));
    }
    let crs = crs.as_deref().map(terrio::crs::SpatialRef::parse).transpose().py()?;
    let template = ClipTemplateOptions { layer: like_layer, where_clause: like_where, sql: like_sql };
    let region = if let Some(bbox) = bbox {
        ClipRegion::bbox(bbox, crs).py()?
    } else if let Some(text) = geometry {
        ClipRegion::parse(&text, crs).py()?
    } else {
        let like = like.unwrap();
        if let Ok(raster) = like.cast::<crate::raster::PyRaster>() {
            if template.layer.is_some() || template.where_clause.is_some() || template.sql.is_some() {
                return Err(invalid("like_layer/where/sql require a vector template"));
            }
            ClipRegion::from_raster(&raster.borrow().inner.header()).py()?
        } else if let Some(layers) = clip_inline_geojson(like) {
            ClipRegion::from_layers(&layers?, &template).py()?
        } else if let Ok(path) = like.extract::<std::path::PathBuf>() {
            let path = path.to_str().ok_or_else(|| invalid("template path must be UTF-8"))?;
            native_clip_template(py, path, &template)?
        } else {
            ClipRegion::from_layers(&input_layers(py, like, None)?, &template).py()?
        }
    };
    let input = if let Some(layers) = clip_inline_geojson(src) {
        let mut layers = layers?;
        if let Some(name) = layer {
            layers = terrio::ops::vector_filter::select_layers(layers, &[name]).py()?;
        }
        if layers.len() != 1 {
            return Err(invalid("clip_vector needs one input layer; select it with layer"));
        }
        layers.remove(0)
    } else if let Ok(path) = src.extract::<std::path::PathBuf>() {
        let path = path.to_str().ok_or_else(|| invalid("input path must be UTF-8"))?;
        let mut layers = py
            .detach(|| {
                terrio::ops::vector_filter::open_layers(
                    path,
                    &terrio::VectorOpenOptions { layers: layer.into_iter().collect(), ..Default::default() },
                )
            })
            .py()?;
        if layers.len() != 1 {
            return Err(invalid("clip_vector needs one input layer; select it with layer"));
        }
        layers.remove(0)
    } else {
        one_layer(py, src, layer)?
    };
    Ok(PyLayer { inner: py.detach(|| terrio::ops::vector_clip::clip(&input, &region)).py()? })
}

fn clip_inline_geojson(value: &Bound<'_, PyAny>) -> Option<PyResult<Vec<Layer>>> {
    let text = value.extract::<String>().ok()?;
    if !text.trim_start().starts_with('{') || std::path::Path::new(&text).is_file() {
        return None;
    }
    Some(terrio::memory::open_vector_bytes(text.into_bytes(), "input.geojson", &Default::default()).map(|v| v.layers).py())
}

#[cfg(not(target_family = "wasm"))]
fn native_clip_template(
    py: Python<'_>,
    path: &str,
    template: &terrio::ops::vector_clip::ClipTemplateOptions,
) -> PyResult<terrio::ops::vector_clip::ClipRegion> {
    py.detach(|| terrio::ops::vector_clip::ClipRegion::from_path(path, template)).py()
}
#[cfg(target_family = "wasm")]
fn native_clip_template(
    _: Python<'_>,
    _: &str,
    _: &terrio::ops::vector_clip::ClipTemplateOptions,
) -> PyResult<terrio::ops::vector_clip::ClipRegion> {
    Err(invalid("template paths are unavailable in WebAssembly; pass a Raster or vector data"))
}
