//! `vectorGrid` (`terrio vector-grid`, GDAL's `gdal_grid`) and the linear referencing of
//! `terrio lineref` (GDAL's `ogrlineref`): `linerefCreate`, `linerefPosition`,
//! `linerefCoordinates` and `linerefSubline`.

use crate::ops::input_layers;
use crate::{Dataset, Inner, js_err, parse_options};
use serde::Deserialize;
use terrio::Layer;
use terrio::crs::SpatialRef;
use terrio::formats::VectorDriver;
use terrio::ops::lineref::LineRefOptions;
use terrio::ops::vector_grid::{GridAlgorithm, GridParams, VectorGridOptions};
use wasm_bindgen::prelude::*;

fn invalid(msg: impl std::fmt::Display) -> JsError {
    JsError::new(&msg.to_string())
}

/// Options of [`vector_grid`].
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct VectorGridArgs {
    algorithm: Option<String>,
    zfield: Option<String>,
    zoffset: Option<f64>,
    zmultiply: Option<f64>,
    layers: Option<Vec<String>>,
    sql: Option<String>,
    r#where: Option<String>,
    bbox: Option<[f64; 4]>,
    extent: Option<[f64; 4]>,
    size: Option<[usize; 2]>,
    resolution: Option<[f64; 2]>,
    ot: Option<String>,
    crs: Option<String>,
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
}

/// Grids scattered vector points into a raster, as `terrio vector-grid` (GDAL's
/// `gdal vector grid` and `gdal_grid`).
///
/// `src`: vector data (a `Dataset`, bytes or GeoJSON). Options: `algorithm` (`"invdist"`, the
/// default, `"invdistnn"`, `"average"`, `"nearest"`, `"linear"`, `"minimum"`, `"maximum"`,
/// `"range"`, `"count"`, `"average_distance"`, `"average_distance_pts"`, or a GDAL string such as
/// `"invdist:power=3"`), `zfield`, `zoffset`, `zmultiply`, `layers`, `sql`, `where`, `bbox`,
/// `extent`, `size`, `resolution`, `ot`, `crs`, and the parameters `power`, `smoothing`,
/// `radius`, `radius1`, `radius2`, `angle`, `minPoints`, `maxPoints`, `minPointsPerQuadrant`,
/// `maxPointsPerQuadrant`, `nodata`.
#[wasm_bindgen(js_name = vectorGrid)]
pub fn vector_grid(src: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let a: VectorGridArgs = parse_options(&options)?;
    let params = GridParams {
        power: a.power,
        smoothing: a.smoothing,
        radius: a.radius,
        radius1: a.radius1,
        radius2: a.radius2,
        angle: a.angle,
        min_points: a.min_points,
        max_points: a.max_points,
        min_points_per_quadrant: a.min_points_per_quadrant,
        max_points_per_quadrant: a.max_points_per_quadrant,
        nodata: a.nodata,
    };
    let algorithm = GridAlgorithm::with_params(a.algorithm.as_deref().unwrap_or("invdist"), &params).map_err(js_err)?;
    let all = input_layers(&src)?;
    let named = a.layers.is_some();
    let all: Vec<Layer> = match &a.layers {
        Some(names) => terrio::ops::vector_filter::select_layers(all, names).map_err(js_err)?,
        None => all,
    };
    let four = |v: Option<[f64; 4]>| v.map(|[a, b, c, d]| (a, b, c, d));
    let o = VectorGridOptions {
        algorithm,
        z_field: a.zfield,
        z_offset: a.zoffset.unwrap_or(0.0),
        z_multiply: a.zmultiply.unwrap_or(1.0),
        where_clause: a.r#where,
        bbox: four(a.bbox),
        extent: four(a.extent),
        size: a.size.map(|[w, h]| (w, h)),
        resolution: a.resolution.map(|[x, y]| (x, y)),
        data_type: a.ot.as_deref().map(terrio::ops::parse_data_type).transpose().map_err(js_err)?.unwrap_or(terrio::DataType::F64),
        crs: a.crs.as_deref().map(SpatialRef::parse).transpose().map_err(js_err)?,
    };
    Ok(Dataset::from_raster(terrio::ops::vector_grid::vector_grid_from_layers(&all, a.sql.as_deref(), named, &o).map_err(js_err)?))
}

/// The first layer of a vector source, or the named one.
fn one_layer(src: &JsValue, name: Option<&str>) -> Result<Layer, JsError> {
    let layers = input_layers(src)?;
    match name {
        Some(n) => layers.into_iter().find(|l| l.name == n).ok_or_else(|| invalid(format!("no layer named {n}"))),
        None => layers.into_iter().next().ok_or_else(|| invalid("no layer")),
    }
}

/// Borrow a Dataset's selected layer during a lookup instead of cloning all parts.
fn with_one_layer<T>(src: &JsValue, name: Option<&str>, apply: impl FnOnce(&Layer) -> Result<T, JsError>) -> Result<T, JsError> {
    if let Some(inner) = Dataset::lookup(src) {
        let borrowed = inner.borrow();
        let Inner::Vector(layers, _) = &*borrowed else {
            return Err(invalid("expected a vector dataset, got a raster"));
        };
        let layer = match name {
            Some(n) => layers.iter().find(|l| l.name == n).ok_or_else(|| invalid(format!("no layer named {n}")))?,
            None => layers.first().ok_or_else(|| invalid("no layer"))?,
        };
        return apply(layer);
    }
    apply(&one_layer(src, name)?)
}

/// Options of [`lineref_create`].
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct LinerefCreateArgs {
    position_field: String,
    step: f64,
    lines_layer: Option<String>,
    points_layer: Option<String>,
    line_field: Option<String>,
    point_field: Option<String>,
    id_field: Option<String>,
    layer_name: Option<String>,
}

/// Cuts a route into parts of equal position length from reference points, as
/// `terrio lineref create` (GDAL's `ogrlineref -create`).
///
/// `lines`, `points`: vector data (a `Dataset`, bytes or GeoJSON). Options: `positionField`
/// (required), `step` (required), `linesLayer`, `pointsLayer`, `lineField` with `pointField`,
/// `idField`, `layerName`. Returns the parts, with `beg`, `end` and `scale` fields.
#[wasm_bindgen(js_name = linerefCreate)]
pub fn lineref_create(lines: JsValue, points: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let a: LinerefCreateArgs = parse_options(&options)?;
    if a.line_field.is_some() != a.point_field.is_some() {
        return Err(invalid("lineField and pointField go together"));
    }
    let lines = one_layer(&lines, a.lines_layer.as_deref())?;
    let points = one_layer(&points, a.points_layer.as_deref())?;
    let o = LineRefOptions {
        position_field: a.position_field,
        step: a.step,
        line_id_field: a.line_field,
        point_id_field: a.point_field,
        output_id_field: a.id_field.unwrap_or_else(|| "uniq_uid".into()),
        layer_name: Some(a.layer_name.unwrap_or_else(|| "parts".into())),
    };
    let parts = terrio::ops::lineref::create_reference(&lines, &points, &o).map_err(js_err)?;
    Ok(Dataset::from_layers(vec![parts], VectorDriver::GeoJSON))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct LinerefLookupArgs {
    layer: Option<String>,
    layer_name: Option<String>,
}

/// The position of `(x, y)` on linear reference parts, as `terrio lineref get-pos` (GDAL's
/// `ogrlineref -get_pos`).
#[wasm_bindgen(js_name = linerefPosition)]
pub fn lineref_position(parts: JsValue, x: f64, y: f64, options: JsValue) -> Result<f64, JsError> {
    let a: LinerefLookupArgs = parse_options(&options)?;
    with_one_layer(&parts, a.layer.as_deref(), |layer| terrio::ops::lineref::position(layer, x, y).map_err(js_err))
}

/// The coordinates `[x, y, z]` of a position on each part spanning it, as
/// `terrio lineref get-coord` (GDAL's `ogrlineref -get_coord`).
#[wasm_bindgen(js_name = linerefCoordinates)]
pub fn lineref_coordinates(parts: JsValue, position: f64, options: JsValue) -> Result<JsValue, JsError> {
    let a: LinerefLookupArgs = parse_options(&options)?;
    let coords = with_one_layer(&parts, a.layer.as_deref(), |layer| terrio::ops::lineref::coordinates(layer, position).map_err(js_err))?;
    let out = js_sys::Array::new();
    for [x, y, z] in coords {
        out.push(&js_sys::Array::of3(&x.into(), &y.into(), &z.into()));
    }
    Ok(out.into())
}

/// The route between two positions, as `terrio lineref get-subline` (GDAL's
/// `ogrlineref -get_subline`): one line feature with `beg`, `end` and `scale` fields.
#[wasm_bindgen(js_name = linerefSubline)]
pub fn lineref_subline(parts: JsValue, begin: f64, end: f64, options: JsValue) -> Result<Dataset, JsError> {
    let a: LinerefLookupArgs = parse_options(&options)?;
    let out = with_one_layer(&parts, a.layer.as_deref(), |layer| {
        terrio::ops::lineref::subline(layer, begin, end, a.layer_name.as_deref().unwrap_or("subline")).map_err(js_err)
    })?;
    Ok(Dataset::from_layers(vec![out], VectorDriver::GeoJSON))
}
