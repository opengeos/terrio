//! Raster-to-vector analysis and topology adapters around the shared Rust engines.
use crate::ops::{input_layers, input_raster, split_options};
use crate::pixel::NodataValue;
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
    nodata: Option<NodataValue>,
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
            nodata: o.nodata.map(NodataValue::Number),
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
    // JSON.stringify otherwise turns explicit NaN/Infinity overrides into None.
    if options.is_object() {
        for name in ["nodata", "interval"] {
            let raw = js_sys::Reflect::get(&options, &JsValue::from_str(name)).map_err(|_| JsError::new("invalid contour option"))?;
            if raw.is_null() || raw.as_f64().is_some_and(|value| !value.is_finite()) {
                return Err(JsError::new(
                    "interval/nodata cannot be null or a non-finite number; use strings 'nan', 'inf' or '-inf' for nodata",
                ));
            }
        }
    }
    let a: ContourOptions = parse_options(&options)?;
    let options = lines::ContourOptions {
        band: a.band,
        interval: a.interval,
        offset: a.offset,
        levels: a.levels,
        nodata: a.nodata.map(NodataValue::number).transpose()?,
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
/// Validity uses finite samples and dataset nodata; alpha/external masks require an explicit mask.
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

#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct ZonalOptions {
    bands: Vec<usize>,
    prefixes: Option<Vec<String>>,
    stats: Option<Vec<String>>,
    all_touched: bool,
    t_srs: Option<String>,
    decimal_places: Option<u8>,
}
impl Default for ZonalOptions {
    fn default() -> Self {
        Self { bands: vec![1], prefixes: None, stats: None, all_touched: false, t_srs: None, decimal_places: None }
    }
}
/// Statistics for each polygon and raster band. Source pixels are materialized.
/// Options: bands, prefixes, stats, allTouched, tSrs, decimalPlaces.
/// Default prefixes raster1, raster2, etc. produce PREFIX_bN_STATISTIC fields.
#[wasm_bindgen(js_name=zonalStats)]
pub fn zonal_stats(zones: &JsValue, rasters: &JsValue, options: JsValue) -> Result<Dataset, JsError> {
    use terrio::ops::zonal::{Statistic, ZonalInput};
    if options.is_object() {
        let raw = js_sys::Reflect::get(&options, &JsValue::from_str("decimalPlaces")).map_err(|_| JsError::new("invalid decimalPlaces"))?;
        if raw.as_f64().is_some_and(|x| !x.is_finite()) {
            return Err(JsError::new("decimalPlaces must be a finite integer"));
        }
    }
    let a: ZonalOptions = parse_options(&options)?;
    let rasters = crate::ops::input_rasters(rasters)?;
    let prefixes = a.prefixes.unwrap_or_else(|| (1..=rasters.len()).map(|i| format!("raster{i}")).collect());
    if prefixes.iter().any(|p| p.trim().is_empty()) {
        return Err(JsError::new("raster field prefixes cannot be empty"));
    }
    if prefixes.len() != rasters.len() {
        return Err(JsError::new("give one prefix per raster, or omit prefixes"));
    }
    let options = terrio::ops::zonal::ZonalOptions {
        stats: a
            .stats
            .map(|names| names.iter().map(|s| Statistic::parse(s)).collect::<terrio::Result<Vec<_>>>())
            .transpose()
            .map_err(js_err)?
            .unwrap_or_else(|| terrio::ops::zonal::ZonalOptions::default().stats),
        all_touched: a.all_touched,
        dst_srs: a.t_srs.as_deref().map(terrio::crs::SpatialRef::parse).transpose().map_err(js_err)?,
        decimal_places: a.decimal_places,
    };
    let mut inputs = Vec::new();
    for (raster, prefix) in rasters.iter().zip(&prefixes) {
        for &band in &a.bands {
            inputs.push(ZonalInput { prefix: format!("{prefix}_b{band}"), raster, band });
        }
    }
    vector_op(zones, |layer| terrio::ops::zonal::zonal_stats(layer, &inputs, &options))
}

/// Recursively split multipart geometries/collections in every layer; IDs restart at one.
#[wasm_bindgen]
pub fn singlepart(src: &JsValue) -> Result<Dataset, JsError> {
    vector_op(src, |layer| Ok(terrio::ops::geometry::singleparts(layer)))
}
