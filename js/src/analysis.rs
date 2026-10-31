//! Raster-to-vector analysis and topology adapters around the shared Rust engines.
use crate::ops::{input_layers, input_raster, split_options};
use crate::{Dataset, js_err, parse_options};
use serde::Deserialize;
use std::collections::BTreeMap;
use terrio::formats::VectorDriver;
use terrio::ops::{contour as lines, footprint as area, topology};
use wasm_bindgen::prelude::*;

#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct ContourOptions {
    band: usize,
    interval: Option<f64>,
    offset: f64,
    levels: Vec<f64>,
    nodata: Option<f64>,
    ignore_nodata: bool,
    with_z: bool,
    field_name: String,
    layer_name: String,
}
impl Default for ContourOptions {
    fn default() -> Self {
        let o = lines::ContourOptions::default();
        Self {
            band: o.band,
            interval: o.interval,
            offset: o.offset,
            levels: o.levels,
            nodata: o.nodata,
            ignore_nodata: o.ignore_nodata,
            with_z: o.with_z,
            field_name: o.field_name,
            layer_name: o.layer_name,
        }
    }
}
/// Contour an uploaded/shared raster. Options mirror the CLI in camelCase.
/// The source raster and generated lines are held in memory.
#[wasm_bindgen]
pub fn contour(src: &JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let a: ContourOptions = parse_options(&options)?;
    let options = lines::ContourOptions {
        band: a.band,
        interval: a.interval,
        offset: a.offset,
        levels: a.levels,
        nodata: a.nodata,
        ignore_nodata: a.ignore_nodata,
        with_z: a.with_z,
        field_name: a.field_name,
        layer_name: a.layer_name,
    };
    let layer = lines::contour(&input_raster(src)?, &options).map_err(js_err)?;
    Ok(Dataset::from_layers(vec![layer], VectorDriver::GeoJSON))
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct FootprintOptions {
    bands: Vec<usize>,
    intersection: bool,
    singlepart: bool,
    attributes: BTreeMap<String, String>,
}
/// Exact valid-data footprint, with optional mask Dataset/upload and explicit text attributes.
/// No source-path field is added automatically; input pixels are materialized.
#[wasm_bindgen]
pub fn footprint(src: &JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let (options, values) = split_options(&options, &["mask"])?;
    let a: FootprintOptions = parse_options(&options)?;
    let options = area::FootprintOptions {
        bands: a.bands,
        intersection: a.intersection,
        mask: values[0].as_ref().map(input_raster).transpose()?,
        singlepart: a.singlepart,
        attributes: a.attributes.into_iter().collect(),
    };
    let layer = area::footprint(&input_raster(src)?, &options).map_err(js_err)?;
    Ok(Dataset::from_layers(vec![layer], VectorDriver::GeoJSON))
}

fn vector_op(src: &JsValue, apply: impl Fn(&terrio::Layer) -> terrio::Result<terrio::Layer>) -> Result<Dataset, JsError> {
    let out = input_layers(src)?.iter().map(apply).collect::<terrio::Result<Vec<_>>>().map_err(js_err)?;
    Ok(Dataset::from_layers(out, VectorDriver::GeoJSON))
}
#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct BufferOptions {
    quadrant_segments: usize,
}
impl Default for BufferOptions {
    fn default() -> Self {
        Self { quadrant_segments: topology::BufferOptions::default().quadrant_segments }
    }
}
/// Planar buffers in CRS units. Options: quadrantSegments (default 30).
#[wasm_bindgen]
pub fn buffer(src: &JsValue, distance: f64, options: JsValue) -> Result<Dataset, JsError> {
    let a: BufferOptions = parse_options(&options)?;
    vector_op(src, |layer| topology::buffer(layer, &topology::BufferOptions { distance, quadrant_segments: a.quadrant_segments }))
}
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct DissolveOptions {
    fields: Vec<String>,
}
/// Dissolve each input layer, optionally grouped by options.fields.
#[wasm_bindgen]
pub fn dissolve(src: &JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let a: DissolveOptions = parse_options(&options)?;
    vector_op(src, |layer| topology::dissolve(layer, &a.fields))
}
/// Return layers annotated with is_valid and valid_reason, preserving source geometry.
#[wasm_bindgen(js_name=checkGeometry)]
pub fn check_geometry(src: &JsValue) -> Result<Dataset, JsError> {
    vector_op(src, |layer| Ok(topology::validity(layer)))
}
/// Repair invalid geometries, retaining attributes, IDs and already-valid Z/M values.
#[wasm_bindgen(js_name=makeValid)]
pub fn make_valid(src: &JsValue) -> Result<Dataset, JsError> {
    vector_op(src, topology::make_valid)
}
