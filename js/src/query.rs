//! Raster queries: `compare` (GDAL's `gdal raster compare`), `pixelInfo` (`gdal raster
//! pixel-info`, `gdallocationinfo`) and `xyz` (`gdal2xyz.py`).
//!
//! Rasters are `Dataset`s (read by window: only the strips or blocks needed are decoded) or the
//! bytes of a file.

use crate::ops::{file_bytes, input_layers, invalid};
use crate::{Dataset, Inner, js_err, parse_options};
use serde::Deserialize;
use serde_json::Value;
use terrio::LazyRaster;
use terrio::ops::compare::CompareOptions;
use terrio::ops::pixel_info::{IncludeFields, PixelInfoOptions, Position, PositionCrs};
use terrio::ops::xyz::XyzOptions;
use wasm_bindgen::prelude::*;

/// Runs `f` on a raster: a `Dataset`, or a file's bytes.
fn with_raster<T>(v: &JsValue, f: impl FnOnce(&mut LazyRaster) -> terrio::Result<T>) -> Result<T, JsError> {
    if let Some(inner) = Dataset::lookup(v) {
        return match &mut *inner.borrow_mut() {
            Inner::Raster(r) => f(r).map_err(js_err),
            Inner::Vector(..) => Err(invalid("expected a raster, got a vector dataset")),
        };
    }
    if let Some((bytes, name)) = file_bytes(v) {
        let mut r = terrio::memory::open_raster_lazy_bytes(bytes, &name).map_err(js_err)?;
        return f(&mut r).map_err(js_err);
    }
    Err(invalid("expected a raster: a Dataset, a Uint8Array or { bytes, filename }"))
}

/// Uses an already opened byte input or borrows a registered Dataset.
fn with_prepared_raster<T>(
    value: &JsValue,
    prepared: &mut Option<LazyRaster>,
    apply: impl FnOnce(&mut LazyRaster) -> terrio::Result<T>,
) -> Result<T, JsError> {
    match prepared {
        Some(raster) => apply(raster).map_err(js_err),
        None => with_raster(value, apply),
    }
}

/// Converts plain JSON, preserving user strings verbatim.
fn value_to_js(v: &Value) -> JsValue {
    marked_value_to_js(v, None)
}

fn marked_value_to_js(v: &Value, marker: Option<&str>) -> JsValue {
    match v {
        Value::Null => JsValue::NULL,
        Value::Bool(b) => JsValue::from_bool(*b),
        Value::Number(n) => JsValue::from_f64(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => match marker.and_then(|marker| s.strip_prefix(marker)) {
            Some("NaN") => JsValue::from_f64(f64::NAN),
            Some("Infinity") => JsValue::from_f64(f64::INFINITY),
            Some("-Infinity") => JsValue::from_f64(f64::NEG_INFINITY),
            _ => JsValue::from_str(s),
        },
        Value::Array(a) => a.iter().map(|v| marked_value_to_js(v, marker)).collect::<js_sys::Array>().into(),
        Value::Object(o) => {
            let obj = js_sys::Object::new();
            for (k, v) in o {
                let _ = js_sys::Reflect::set(&obj, &JsValue::from_str(k), &marked_value_to_js(v, marker));
            }
            obj.into()
        }
    }
}

/// Options of [`compare`].
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct CompareArgs {
    skip_all_optional: bool,
    skip_binary: bool,
    skip_crs: bool,
    skip_geotransform: bool,
    skip_overview: bool,
    skip_metadata: bool,
    skip_rpc: bool,
    skip_geolocation: bool,
    skip_subdataset: bool,
}

/// Compares two rasters as `terrio compare` (GDAL's `gdal raster compare`) does. The binary
/// comparison is made when both are given as bytes (identical bytes have no difference); RPC and
/// subdataset comparisons need files and are not made. Options: `skipAllOptional`, `skipBinary`,
/// `skipCrs`, `skipGeotransform`, `skipOverview`, `skipMetadata`, `skipRpc`, `skipGeolocation`,
/// `skipSubdataset`. Returns `{ difference_count, differences: [{ kind, band?, message }],
/// bands: [{ band, data_type, pixels, pixels_differing, max_abs_diff }] }`.
#[wasm_bindgen]
pub fn compare(reference: JsValue, input: JsValue, options: JsValue) -> Result<JsValue, JsError> {
    let a: CompareArgs = parse_options(&options)?;
    let options = if a.skip_all_optional {
        CompareOptions::skip_all_optional()
    } else {
        CompareOptions {
            skip_binary: a.skip_binary,
            skip_crs: a.skip_crs,
            skip_geotransform: a.skip_geotransform,
            skip_overview: a.skip_overview,
            skip_metadata: a.skip_metadata,
            skip_rpc: a.skip_rpc,
            skip_geolocation: a.skip_geolocation,
            skip_subdataset: a.skip_subdataset,
        }
    };
    // Copy each byte input once, then move that same buffer into its lazy mount.
    let reference_bytes = if Dataset::lookup(&reference).is_none() {
        Some(file_bytes(&reference).ok_or_else(|| invalid("expected a raster: a Dataset, a Uint8Array or { bytes, filename }"))?)
    } else {
        None
    };
    let input_bytes = if Dataset::lookup(&input).is_none() {
        Some(file_bytes(&input).ok_or_else(|| invalid("expected a raster: a Dataset, a Uint8Array or { bytes, filename }"))?)
    } else {
        None
    };
    let mut binary = None;
    if !options.skip_binary
        && let (Some((ra, _)), Some((rb, _))) = (&reference_bytes, &input_bytes)
    {
        match terrio::ops::compare::compare_bytes(ra, rb) {
            None => return Ok(value_to_js(&terrio::ops::compare::Comparison::default().to_json())),
            Some(message) => binary = Some(message),
        }
    }
    let open = |(bytes, name): (Vec<u8>, String)| terrio::memory::open_raster_lazy_bytes(bytes, &name).map_err(js_err);
    let mut lazy_reference = reference_bytes.map(open).transpose()?;
    let mut lazy_input = input_bytes.map(open).transpose()?;
    let mut comparison = if let Some(i) = &mut lazy_input {
        with_prepared_raster(&reference, &mut lazy_reference, |r| terrio::ops::compare::compare_rasters(r, i, &options))?
    } else {
        // The input is a Dataset: borrowed after the reference, unless both are the same one.
        let same = Dataset::lookup(&reference).zip(Dataset::lookup(&input)).is_some_and(|(a, b)| std::rc::Rc::ptr_eq(&a, &b));
        if same {
            with_prepared_raster(&reference, &mut lazy_reference, |r| {
                let mut copy = r.shared();
                terrio::ops::compare::compare_rasters(r, &mut copy, &options)
            })?
        } else {
            let other = Dataset::lookup(&input).expect("a Dataset");
            let mut other = other.borrow_mut();
            let Inner::Raster(i) = &mut *other else { return Err(invalid("expected a raster, got a vector dataset")) };
            with_prepared_raster(&reference, &mut lazy_reference, |r| terrio::ops::compare::compare_rasters(r, i, &options))?
        }
    };
    if let Some(message) = binary {
        comparison.differences.insert(0, terrio::ops::compare::Difference { kind: "binary", band: None, subdataset: None, message });
    }
    Ok(value_to_js(&comparison.to_json()))
}

/// Options of [`pixel_info`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct PixelInfoArgs {
    position_crs: Option<String>,
    layer: Option<String>,
    include_fields: Vec<String>,
    bands: Vec<usize>,
    overview: Option<usize>,
    resampling: String,
    promote_pixel_value_to_z: bool,
    format: String,
    field_sep: String,
}

impl Default for PixelInfoArgs {
    fn default() -> Self {
        Self {
            position_crs: None,
            layer: None,
            include_fields: Vec::new(),
            bands: Vec::new(),
            overview: None,
            resampling: "nearest".into(),
            promote_pixel_value_to_z: false,
            format: "geojson".into(),
            field_sep: "\n".into(),
        }
    }
}

/// Queries pixel values at positions, as `terrio pixel-info` (GDAL's `gdal raster pixel-info` and
/// `gdallocationinfo`). `positions` is an array of `[x, y]` or `[x, y, text]` (column and line,
/// or coordinates in `positionCrs`), or the vector dataset whose points (centroids of other
/// geometries) are the positions: a `Dataset`, file bytes or GeoJSON. Options: `positionCrs`
/// (`pixel`, `dataset` or a CRS), `layer`, `includeFields`, `bands`, `overview`, `resampling`
/// (`nearest`, `bilinear`, `cubic`, `cubicspline`), `promotePixelValueToZ` and `format`:
/// `geojson` (an object, the default), `json` (an array), `csv`, `text` or `value` (strings).
#[wasm_bindgen(js_name = pixelInfo)]
pub fn pixel_info(src: JsValue, positions: JsValue, options: JsValue) -> Result<JsValue, JsError> {
    use terrio::ops::Resampling;
    let a: PixelInfoArgs = parse_options(&options)?;
    let resampling = match Resampling::parse(&a.resampling).map_err(js_err)? {
        r @ (Resampling::Nearest | Resampling::Bilinear | Resampling::Cubic | Resampling::CubicSpline) => r,
        _ => return Err(invalid("resampling must be nearest, bilinear, cubic or cubicspline")),
    };
    let o = PixelInfoOptions {
        bands: a.bands,
        overview: a.overview,
        position_crs: a.position_crs.as_deref().map(PositionCrs::parse).transpose().map_err(js_err)?,
        resampling,
        promote_pixel_value_to_z: a.promote_pixel_value_to_z,
        include_fields: IncludeFields::parse(&a.include_fields),
    };
    let info = if js_sys::Array::is_array(&positions) {
        let mut list = Vec::new();
        for p in js_sys::Array::from(&positions).iter() {
            let p = js_sys::Array::from(&p);
            let (x, y) = (p.get(0).as_f64(), p.get(1).as_f64());
            let (Some(x), Some(y)) = (x, y) else { return Err(invalid("positions are [x, y] or [x, y, text] arrays")) };
            list.push(Position { x, y, extra: p.get(2).as_string() });
        }
        with_raster(&src, |r| terrio::ops::pixel_info::pixel_info(r, &list, &o))?
    } else {
        let layers = input_layers(&positions)?;
        let layer = match &a.layer {
            Some(name) => layers.into_iter().find(|l| &l.name == name).ok_or_else(|| invalid(format!("no layer '{name}'")))?,
            None if layers.len() == 1 => layers.into_iter().next().expect("one layer"),
            None => return Err(invalid("the position dataset has several layers: choose one with layer")),
        };
        with_raster(&src, |r| terrio::ops::pixel_info::pixel_info_layer(r, &layer, &o))?
    };
    match a.format.to_ascii_lowercase().as_str() {
        "geojson" => {
            let (value, marker) = info.to_geojson_marked();
            Ok(marked_value_to_js(&value, Some(&marker)))
        }
        "json" => Ok(value_to_js(&info.to_json())),
        "csv" => Ok(JsValue::from_str(&info.to_csv())),
        "text" => Ok(JsValue::from_str(&info.to_text())),
        "value" | "valonly" => Ok(JsValue::from_str(&info.to_values(&a.field_sep))),
        other => Err(invalid(format!("unknown format '{other}': use geojson, json, csv, text or value"))),
    }
}

/// Options of [`xyz`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct XyzArgs {
    bands: Vec<usize>,
    all_bands: bool,
    skip: Vec<usize>,
    srcwin: Option<[usize; 4]>,
    skip_nodata: bool,
    src_nodata: Vec<f64>,
    dst_nodata: Vec<f64>,
    delimiter: String,
}

impl Default for XyzArgs {
    fn default() -> Self {
        Self {
            bands: Vec::new(),
            all_bands: false,
            skip: Vec::new(),
            srcwin: None,
            skip_nodata: false,
            src_nodata: Vec::new(),
            dst_nodata: Vec::new(),
            delimiter: " ".into(),
        }
    }
}

/// Raster pixels as XYZ text, as `terrio xyz` (GDAL's `gdal2xyz.py`). Options: `bands` (band 1
/// by default), `allBands`, `skip` (`[n]` or `[columns, rows]`), `srcwin` (`[x, y, width,
/// height]`), `skipNodata`, `srcNodata` and `dstNodata` (one value or one per band) and
/// `delimiter`.
#[wasm_bindgen]
pub fn xyz(src: JsValue, options: JsValue) -> Result<String, JsError> {
    let a: XyzArgs = parse_options(&options)?;
    let skip = match a.skip.as_slice() {
        [] => (1, 1),
        [n] => (*n, *n),
        [x, y] => (*x, *y),
        _ => return Err(invalid("skip is [n] or [columns, rows]")),
    };
    let o = XyzOptions {
        bands: if a.all_bands {
            Vec::new()
        } else if a.bands.is_empty() {
            vec![1]
        } else {
            a.bands
        },
        skip,
        srcwin: a.srcwin,
        skip_nodata: a.skip_nodata,
        src_nodata: (!a.src_nodata.is_empty()).then_some(a.src_nodata),
        dst_nodata: (!a.dst_nodata.is_empty()).then_some(a.dst_nodata),
        delimiter: a.delimiter,
    };
    with_raster(&src, |r| {
        let mut out = Vec::new();
        terrio::ops::xyz::write_xyz(r, &mut out, &o)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    })
}
