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

fn vector_op(src: &JsValue, apply: impl FnMut(&terrio::Layer) -> terrio::Result<terrio::Layer>) -> Result<Dataset, JsError> {
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
    fractional: bool,
    weight_band: usize,
    t_srs: Option<String>,
    decimal_places: Option<u8>,
}
impl Default for ZonalOptions {
    fn default() -> Self {
        Self {
            bands: vec![1],
            prefixes: None,
            stats: None,
            all_touched: false,
            fractional: false,
            weight_band: 1,
            t_srs: None,
            decimal_places: None,
        }
    }
}
/// Statistics for each polygon and raster band. Rasters are read lazily, by block.
/// Options: bands, prefixes, stats, allTouched, fractional, weights, weightBand, tSrs, decimalPlaces.
/// Rasters may also be labelled objects `{raster, time, label, band}`, whose fields are named
/// TIME_LABEL_STATISTIC.
/// Default prefixes raster1, raster2, etc. produce PREFIX_bN_STATISTIC fields.
#[wasm_bindgen(js_name=zonalStats)]
pub fn zonal_stats(zones: &JsValue, rasters: &JsValue, options: JsValue) -> Result<Dataset, JsError> {
    use terrio::ops::zonal::{Statistic, ZonalBand, ZonalSource, ZonalWeight};
    if options.is_object() {
        let raw = js_sys::Reflect::get(&options, &JsValue::from_str("decimalPlaces")).map_err(|_| JsError::new("invalid decimalPlaces"))?;
        if raw.as_f64().is_some_and(|x| !x.is_finite()) {
            return Err(JsError::new("decimalPlaces must be a finite integer"));
        }
    }
    let (rest, values) = split_options(&options, &["weights"])?;
    let a: ZonalOptions = parse_options(&rest)?;
    if a.prefixes.as_ref().is_some_and(|p| p.iter().any(|p| p.trim().is_empty())) {
        return Err(JsError::new("raster field prefixes cannot be empty"));
    }
    let stats = a
        .stats
        .map(|names| names.iter().map(|s| Statistic::parse(s)).collect::<terrio::Result<Vec<_>>>())
        .transpose()
        .map_err(js_err)?
        .unwrap_or_else(|| terrio::ops::zonal::ZonalOptions::default().stats);
    let weights = values[0].as_ref().filter(|_| stats.iter().any(|s| s.uses_weights())).map(crate::ops::input_lazy_raster).transpose()?;
    let options = terrio::ops::zonal::ZonalOptions {
        fractional: a.fractional,
        weights: weights.map(|raster| ZonalWeight::new(raster, a.weight_band)),
        stats,
        all_touched: a.all_touched,
        dst_srs: a.t_srs.as_deref().map(terrio::crs::SpatialRef::parse).transpose().map_err(js_err)?,
        decimal_places: a.decimal_places,
    };
    let items: Vec<JsValue> = if js_sys::Array::is_array(rasters) { js_sys::Array::from(rasters).to_vec() } else { vec![rasters.clone()] };
    if items.is_empty() {
        return Err(JsError::new("no input rasters"));
    }
    let get =
        |item: &JsValue, key: &str| js_sys::Reflect::get(item, &JsValue::from_str(key)).ok().filter(|v| !v.is_undefined() && !v.is_null());
    // A labelled row: a plain object with any of raster, time, label or band.
    let is_row = |item: &JsValue| {
        item.is_object()
            && crate::Dataset::lookup(item).is_none()
            && !item.is_instance_of::<js_sys::Uint8Array>()
            && ["raster", "time", "label", "band"].iter().any(|k| get(item, k).is_some())
    };
    let labelled = items.iter().any(is_row);
    if labelled && a.prefixes.is_some() {
        return Err(JsError::new("prefixes cannot be combined with labelled inputs (objects): their fields are named TIME_LABEL"));
    }
    let prefixes = a.prefixes.unwrap_or_else(|| (1..=items.len()).map(|i| format!("raster{i}")).collect());
    if prefixes.len() != items.len() {
        return Err(JsError::new("give one prefix per raster, or omit prefixes"));
    }
    let mut sources: Vec<ZonalSource> = Vec::new();
    // Labelled rows naming the same raster (`Object.is`) share one source, read once, as in the
    // CLI and Python: (raster, source index).
    let mut row_sources: Vec<(JsValue, usize)> = Vec::new();
    for (item, prefix) in items.iter().zip(&prefixes) {
        if !is_row(item) {
            let bands = a.bands.iter().map(|&band| ZonalBand::new(band, format!("{prefix}_b{band}"))).collect();
            sources.push(ZonalSource { raster: crate::ops::input_lazy_raster(item)?.into(), bands });
            continue;
        }
        let raster = get(item, "raster").ok_or_else(|| JsError::new("each input object needs a 'raster' (a Dataset or a file's bytes)"))?;
        // Text, a number or a boolean as `String(v)` writes it, or a Date in ISO 8601.
        let text = |key: &str| -> Result<Option<String>, JsError> {
            let Some(v) = get(item, key) else { return Ok(None) };
            if let Some(date) = v.dyn_ref::<js_sys::Date>() {
                return Ok(Some(date.to_iso_string().into()));
            }
            v.as_string()
                .or_else(|| v.as_f64().map(|n| n.to_string()))
                .or_else(|| v.as_bool().map(|b| b.to_string()))
                .map(Some)
                .ok_or_else(|| JsError::new(&format!("'{key}' must be a string, a number or a Date")))
        };
        let (time, label) = (text("time")?, text("label")?);
        let row_bands = match get(item, "band") {
            Some(b) => {
                vec![b.as_f64().filter(|b| b.fract() == 0.0 && *b >= 1.0).ok_or_else(|| JsError::new("band must be an integer from 1"))?
                    as usize]
            }
            None => a.bands.clone(),
        };
        let bands = terrio::ops::zonal::labelled_bands(&row_bands, time.as_deref(), label.as_deref(), |band| format!("{prefix}_b{band}"));
        match row_sources.iter().find(|(r, _)| js_sys::Object::is(r, &raster)) {
            Some(&(_, i)) => sources[i].bands.extend(bands),
            None => {
                row_sources.push((raster.clone(), sources.len()));
                sources.push(ZonalSource { raster: crate::ops::input_lazy_raster(&raster)?.into(), bands });
            }
        }
    }
    vector_op(zones, |layer| terrio::ops::zonal::zonal_stats(layer, &mut sources, &options))
}

/// Recursively split multipart geometries/collections in every layer; IDs restart at one.
#[wasm_bindgen]
pub fn singlepart(src: &JsValue) -> Result<Dataset, JsError> {
    vector_op(src, |layer| Ok(terrio::ops::geometry::singleparts(layer)))
}
