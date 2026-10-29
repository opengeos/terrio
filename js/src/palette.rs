//! Shared palette algorithms for uploaded rasters and in-memory color tables.
use crate::ops::{file_bytes, invalid, split_options};
use crate::{Dataset, Inner, js_err, parse_options, to_js};
use serde::Deserialize;
use terrio::LazyRaster;
use terrio::ops::palette::{self, PaletteOptions};
use wasm_bindgen::prelude::*;

fn field(value: &JsValue, key: &str) -> Result<JsValue, JsError> {
    if value.is_null() || value.is_undefined() {
        return Ok(JsValue::UNDEFINED);
    }
    js_sys::Reflect::get(value, &JsValue::from_str(key)).map_err(|_| invalid("cannot read palette options"))
}
fn with_source<T>(value: &JsValue, convert: impl FnOnce(&mut LazyRaster) -> terrio::Result<T>) -> Result<T, JsError> {
    if let Some(inner) = Dataset::lookup(value) {
        let mut inner = inner.try_borrow_mut().map_err(|e| invalid(e.to_string()))?;
        return match &mut *inner {
            Inner::Raster(source) => convert(source).map_err(js_err),
            Inner::Vector(..) => Err(invalid("expected a raster source")),
        };
    }
    if let Some((bytes, name)) = file_bytes(value) {
        return convert(&mut terrio::memory::open_raster_lazy_bytes(bytes, &name).map_err(js_err)?).map_err(js_err);
    }
    Err(invalid("expected a raster Dataset, Uint8Array or {bytes, filename}"))
}
fn table(source: &JsValue, filename: Option<&str>) -> Result<Vec<[u8; 4]>, JsError> {
    if js_sys::Array::is_array(source) {
        return parse_options(source);
    }
    if Dataset::lookup(source).is_some() {
        return with_source(source, |r| palette::color_table(&r.header()));
    }
    if let Some((bytes, name)) = file_bytes(source) {
        return palette::color_table_from_bytes(bytes, filename.unwrap_or(&name)).map_err(js_err);
    }
    if let Some(text) = source.as_string() {
        return palette::parse_color_table(&text, filename.unwrap_or("palette.txt")).map_err(js_err);
    }
    if let Some(text) = field(source, "text")?.as_string() {
        let name = field(source, "filename")?.as_string();
        return palette::parse_color_table(&text, filename.or(name.as_deref()).unwrap_or("palette.txt")).map_err(js_err);
    }
    Err(invalid("expected a palette Dataset/file, color-file text or RGBA entries"))
}
/// Returns RGBA entries from a palette Dataset/file, color-file text or RGBA arrays.
/// filename hints TXT/CPT/QML/QLR formats for text or encoded bytes. Raster palettes
/// are read from headers, with source nodata transparency applied. Text-file
/// Uint8Array uploads need a filename hint or {bytes, filename}; plain strings
/// and {text, filename} accept color-file text directly.
#[wasm_bindgen(js_name=readColorTable)]
pub fn read_color_table(source: JsValue, filename: Option<String>) -> Result<JsValue, JsError> {
    to_js(&table(&source, filename.as_deref())?)
}
#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct RgbArgs {
    color_count: usize,
    bit_depth: u8,
    dither: bool,
    output_nodata: Option<u8>,
}
impl Default for RgbArgs {
    fn default() -> Self {
        Self { color_count: 256, bit_depth: 5, dither: true, output_nodata: None }
    }
}
/// Converts RGB Byte sources to indexed color. Options: colorCount (2..256),
/// bitDepth (5 or 8), dither (true), colorMap (palette Dataset/file/text/RGBA entries),
/// outputNodata (Byte index), mask (flat Uint8Array/array, zero invalid).
/// Color-file byte uploads need {bytes, filename}; plain strings accept text.
/// The full raster is materialized. Shared RGB nodata takes precedence over
/// dataset/alpha/caller masks. Supplied palettes need outputNodata to mark invalid pixels.
#[wasm_bindgen(js_name=rgbToPalette)]
pub fn rgb_to_palette(src: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    if field(&options, "outputNodata")?.as_f64().is_some_and(|v| !v.is_finite()) {
        return Err(invalid("outputNodata must be a finite Byte index"));
    }
    let (scalars, _) = split_options(&options, &["colorMap", "mask"])?;
    let args: RgbArgs = parse_options(&scalars)?;
    let color_map = field(&options, "colorMap")?;
    let mask = field(&options, "mask")?;
    let mask = if mask.is_null() || mask.is_undefined() {
        None
    } else if let Some(values) = mask.dyn_ref::<js_sys::Uint8Array>() {
        Some(values.to_vec())
    } else {
        Some(parse_options::<Vec<u8>>(&mask)?)
    };
    let options = PaletteOptions {
        color_count: args.color_count,
        bit_depth: args.bit_depth,
        dither: args.dither,
        output_nodata: args.output_nodata,
        mask,
        color_table: if color_map.is_null() || color_map.is_undefined() { None } else { Some(table(&color_map, None)?) },
    };
    Ok(Dataset::from_raster(with_source(&src, |r| palette::rgb_to_palette_lazy(r, &options))?))
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ExpandArgs {
    rgba: bool,
}
/// Expands one indexed band to RGB or RGBA (options: rgba), retaining the dataset mask.
/// The full source and output are materialized; the returned Dataset supports mask reads.
#[wasm_bindgen(js_name=paletteToRgb)]
pub fn palette_to_rgb(src: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let args: ExpandArgs = parse_options(&options)?;
    Ok(Dataset::from_lazy(with_source(&src, |r| palette::palette_to_rgb_lazy(r, args.rgba))?))
}
/// Attaches a palette without changing Byte/UInt16 indices, nodata or the dataset mask.
/// colorMap accepts a palette Dataset/file, color-file text or RGBA arrays.
#[wasm_bindgen(js_name=attachPalette)]
pub fn attach_palette(src: JsValue, color_map: JsValue) -> Result<Dataset, JsError> {
    let colors = table(&color_map, None)?;
    Ok(Dataset::from_lazy(with_source(&src, |r| palette::attach_palette_lazy(r, &colors))?))
}
