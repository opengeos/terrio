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
    // CLI and Python; a labelled row also reuses the source of an earlier plain item for that
    // raster. Each plain item adds its own source, so a repeated plain item is read again, as in
    // Python: raster -> source index.
    let row_sources = js_sys::Map::new();
    for (item, prefix) in items.iter().zip(&prefixes) {
        if !is_row(item) {
            let bands = a.bands.iter().map(|&band| ZonalBand::new(band, format!("{prefix}_b{band}"))).collect();
            if !row_sources.has(item) {
                row_sources.set(item, &JsValue::from(sources.len() as u32));
            }
            sources.push(ZonalSource { raster: crate::ops::input_lazy_raster(item)?.into(), bands });
            continue;
        }
        let raster = get(item, "raster").ok_or_else(|| JsError::new("each input object needs a 'raster' (a Dataset or a file's bytes)"))?;
        if let Some(key) = js_sys::Object::keys(item.unchecked_ref::<js_sys::Object>())
            .iter()
            .filter_map(|k| k.as_string())
            .find(|k| !matches!(k.as_str(), "raster" | "time" | "label" | "band"))
        {
            return Err(JsError::new(&format!("unknown input key '{key}' (expected raster, time, label, band)")));
        }
        // Text, a number or a boolean as `String(v)` writes it, or a Date in ISO 8601, its date
        // alone at midnight UTC (as the CLI and Python name it).
        let text = |key: &str| -> Result<Option<String>, JsError> {
            let Some(v) = get(item, key) else { return Ok(None) };
            if let Some(date) = v.dyn_ref::<js_sys::Date>() {
                if date.get_time().is_nan() {
                    return Err(JsError::new(&format!("'{key}' is an invalid Date")));
                }
                return Ok(Some(terrio::ops::zonal::time_text(&String::from(date.to_iso_string()))));
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
        match row_sources.get(&raster).as_f64() {
            Some(i) => sources[i as usize].bands.extend(bands),
            None => {
                row_sources.set(&raster, &JsValue::from(sources.len() as u32));
                sources.push(ZonalSource { raster: crate::ops::input_lazy_raster(&raster)?.into(), bands });
            }
        }
    }
    vector_op(zones, |layer| terrio::ops::zonal::zonal_stats(layer, &mut sources, &options))
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct ReducerOptions {
    stats: Option<Vec<String>>,
    all_touched: bool,
    fractional: bool,
    layer: Option<String>,
    strip_rows: Option<usize>,
}

/// Zonal statistics of arrays you produce strip by strip, equal to `zonalStats` over a raster
/// holding the same values: the zones are computed once for the grid, then each `stream()`
/// reduces one band, fed the `windows()` in order.
/// Options: stats (default count, min, max, mean, sum, stddev; not the weighted ones), allTouched,
/// fractional, layer (of a dataset with several), stripRows (rows per window; give a tiled source's
/// block height to read each block once). Not tSrs or decimalPlaces: the results are unrounded
/// arrays.
#[wasm_bindgen(js_name = ZonalReducer)]
pub struct JsZonalReducer {
    inner: terrio::ops::zonal::ZonalReducer,
    names: Vec<String>,
}

#[wasm_bindgen(js_class = ZonalReducer)]
impl JsZonalReducer {
    /// `transform` is the grid's GDAL-style geotransform; `crs` may be omitted only if the zones
    /// have none.
    #[wasm_bindgen(constructor)]
    pub fn new(
        zones: &JsValue,
        width: f64,
        height: f64,
        transform: Vec<f64>,
        crs: Option<String>,
        options: JsValue,
    ) -> Result<JsZonalReducer, JsError> {
        use terrio::ops::rasterize::Grid;
        use terrio::ops::zonal::{Statistic, ZonalOptions, ZonalReducer};
        let (width, height) = (whole("width", width)?, whole("height", height)?);
        let a: ReducerOptions = parse_options(&options)?;
        let geo_transform: [f64; 6] =
            transform.try_into().map_err(|_| JsError::new("transform must be a GDAL geotransform of 6 numbers"))?;
        let stats = match &a.stats {
            Some(names) => names.iter().map(|s| Statistic::parse(s)).collect::<terrio::Result<Vec<_>>>().map_err(js_err)?,
            None => ZonalOptions::default().stats,
        };
        // Canonical names (`stddev` for `std`), as `zonalStats` names its fields.
        let names: Vec<String> = stats.iter().map(|s| s.name()).collect();
        // A named layer is selected as readers select it (a single-layer file also answers to its
        // file name).
        let mut layers = match &a.layer {
            Some(name) => crate::ops::selected_input_layers(zones, std::slice::from_ref(name))?,
            None => input_layers(zones)?,
        };
        if layers.len() != 1 {
            return Err(JsError::new(&format!(
                "the zones must have one layer, or name one with the layer option (they have {})",
                layers.len()
            )));
        }
        let zones = layers.remove(0);
        let options = ZonalOptions { fractional: a.fractional, stats, all_touched: a.all_touched, ..Default::default() };
        let crs = crs.as_deref().map(terrio::crs::SpatialRef::parse).transpose().map_err(js_err)?.unwrap_or_default();
        let grid = Grid { cols: width, rows: height, geo_transform, crs };
        let inner = ZonalReducer::new(&zones, grid, &options, a.strip_rows).map_err(js_err)?;
        Ok(Self { inner, names })
    }

    /// The windows to supply, in order, as `[x, y, width, height]`: strips of `stripRows` rows
    /// holding zone pixels, cropped to the rows and columns of those pixels.
    pub fn windows(&self) -> js_sys::Array {
        self.inner
            .windows()
            .iter()
            .map(|w| JsValue::from(js_sys::Array::of4(&w.x.into(), &w.y.into(), &w.width.into(), &w.height.into())))
            .collect()
    }

    /// The number of zone features.
    #[wasm_bindgen(getter, js_name = featureCount)]
    pub fn feature_count(&self) -> usize {
        self.inner.feature_count()
    }

    /// The statistics computed, by their canonical names (`stddev` for `std`), as fields name them.
    #[wasm_bindgen(getter)]
    pub fn stats(&self) -> Vec<String> {
        self.names.clone()
    }

    /// A reduction of one band over `windows()`; pixels equal to `nodata`, or not finite, are
    /// left out.
    pub fn stream(&self, nodata: Option<f64>) -> JsZonalStream {
        JsZonalStream { stream: Some(self.inner.stream(nodata)), names: self.names.clone() }
    }
}

/// `v` as a whole number (a size or a window index), taken as a number rather than a usize: wasm-bindgen would wrap -1 to
/// 4294967295 and truncate 300.7.
fn whole(name: &str, v: f64) -> Result<usize, JsError> {
    if v.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&v) {
        Ok(v as usize)
    } else {
        Err(JsError::new(&format!("{name} must be a whole number from 0 to {}", u32::MAX)))
    }
}

/// One band reduced by a `ZonalReducer`: `add` each window's pixels in order, then `finish`.
#[wasm_bindgen(js_name = ZonalStream)]
pub struct JsZonalStream {
    stream: Option<terrio::ops::zonal::ZonalStream>,
    names: Vec<String>,
}

#[wasm_bindgen(js_class = ZonalStream)]
impl JsZonalStream {
    /// Adds window number `window` (of `ZonalReducer.windows()`): a typed array of its
    /// `width * height` pixels, row after row. Windows must come in order; a wrong window or
    /// length throws and leaves the stream unchanged.
    pub fn add(&mut self, window: f64, data: &JsValue) -> Result<(), JsError> {
        let window = whole("window", window)?;
        let stream = self.stream.as_mut().ok_or_else(|| JsError::new("the stream is finished"))?;
        stream.add(window, &crate::raster_data(data)?).map_err(js_err)
    }

    /// The statistics, as `{name: Float64Array}` with one value per zone feature, in layer
    /// order; NaN where a zone has no valid pixels (`count` and `variety` are then 0). With
    /// windows missing, throws and the stream takes them still.
    pub fn finish(&mut self) -> Result<js_sys::Object, JsError> {
        use terrio::FieldValue;
        // Taken only once it is complete: after a missing-windows error, the stream takes them.
        self.stream.as_ref().ok_or_else(|| JsError::new("the stream is finished"))?.check_complete().map_err(js_err)?;
        let stream = self.stream.take().expect("checked above");
        let out = js_sys::Object::new();
        for (name, column) in self.names.iter().zip(stream.finish().map_err(js_err)?) {
            let values: Vec<f64> = column
                .iter()
                .map(|v| match v {
                    FieldValue::Float(x) => *x,
                    FieldValue::Integer(i) => *i as f64,
                    _ => f64::NAN,
                })
                .collect();
            js_sys::Reflect::set(&out, &JsValue::from_str(name), &js_sys::Float64Array::from(&values[..]))
                .map_err(|_| JsError::new("cannot build the result"))?;
        }
        Ok(out)
    }
}

/// Recursively split multipart geometries/collections in every layer; IDs restart at one.
#[wasm_bindgen]
pub fn singlepart(src: &JsValue) -> Result<Dataset, JsError> {
    vector_op(src, |layer| Ok(terrio::ops::geometry::singleparts(layer)))
}
