//! Incremental XYZ/TMS tile generation for browser and Node callers.
use terrio::ops::raster_tile::{self as tile, FolderSink, TileOptions};
use terrio::ops::warp::WarpSource;
use wasm_bindgen::prelude::*;

/// Plan folder tiles without reading pixels. Grid fields accept camelCase.
#[wasm_bindgen(js_name = planRasterTiles)]
pub fn plan_raster_tiles(input: &JsValue, options: JsValue) -> Result<JsValue, JsError> {
    let options: TileOptions = crate::parse_options(&options)?;
    let mut source = crate::ops::tile_input(input)?;
    crate::to_js(&tile::plan(&source.info().map_err(crate::js_err)?.raster, &options).map_err(crate::js_err)?.0)
}

/// Generate one encoded tile at a time, calling onTile(relativePath, Uint8Array).
/// The callback is synchronous. A thrown exception stops generation. Use the callback
/// to save/download each tile; Terrio does not retain the encoded pyramid in WASM memory.
#[wasm_bindgen(js_name = rasterTiles)]
pub fn raster_tiles(input: &JsValue, options: JsValue, on_tile: &js_sys::Function) -> Result<JsValue, JsValue> {
    let options: TileOptions = crate::parse_options(&options).map_err(JsValue::from)?;
    let mut source = crate::ops::tile_input(input).map_err(JsValue::from)?;
    struct Callback<'a> {
        function: &'a js_sys::Function,
        exception: Option<JsValue>,
    }
    impl FolderSink for Callback<'_> {
        fn put(&mut self, name: &str, bytes: &[u8]) -> terrio::Result<()> {
            if let Err(exception) = self.function.call2(&JsValue::NULL, &JsValue::from_str(name), &js_sys::Uint8Array::from(bytes)) {
                self.exception = Some(exception);
                return Err(terrio::Error::InvalidArgument("tile callback failed".into()));
            }
            Ok(())
        }
    }
    let mut callback = Callback { function: on_tile, exception: None };
    let result = tile::generate(&mut source, &options, &mut callback);
    if let Some(exception) = callback.exception {
        return Err(exception);
    }
    let report = result.map_err(|error| JsValue::from(crate::js_err(error)))?;
    crate::to_js(&report).map_err(JsValue::from)
}

fn retile_inputs(value: &JsValue) -> Result<Vec<(String, crate::ops::WarpInput)>, JsError> {
    let values = if js_sys::Array::is_array(value) { js_sys::Array::from(value).iter().collect::<Vec<_>>() } else { vec![value.clone()] };
    values
        .iter()
        .enumerate()
        .map(|(i, value)| {
            let name = js_sys::Reflect::get(value, &JsValue::from_str("filename"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| format!("input_{}.tif", i + 1));
            Ok((name, crate::ops::tile_input(value)?))
        })
        .collect()
}

/// Plan GDAL-style retile files and recursive pyramid levels. Inputs are a
/// Dataset/byte upload or an array of them; options use camelCase or snake_case.
#[wasm_bindgen(js_name=planRetile)]
pub fn plan_retile(inputs: &JsValue, options: JsValue) -> Result<JsValue, JsError> {
    use terrio::ops::raster_retile::{self as re, RetileOptions, RetileSource};
    let options: RetileOptions = crate::parse_options(&options)?;
    let mut inputs = retile_inputs(inputs)?;
    let mut sources: Vec<_> = inputs.iter_mut().map(|(name, source)| RetileSource { name: name.clone(), source }).collect();
    crate::to_js(&re::plan(&mut sources, &options).map_err(crate::js_err)?)
}

/// Generate retile image files and optional index/CSV companions incrementally.
/// onFile(relativePath, Uint8Array) is synchronous; exceptions stop generation.
/// Optional readFile(relativePath) returns earlier bytes (or null), avoiding parent
/// regeneration after the bounded cache evicts a tile.
#[wasm_bindgen(js_name=retile)]
pub fn retile(
    inputs: &JsValue,
    options: JsValue,
    on_file: &js_sys::Function,
    read_file: Option<js_sys::Function>,
) -> Result<JsValue, JsValue> {
    use terrio::ops::raster_retile::{self as re, RetileOptions, RetileSource};
    let options: RetileOptions = crate::parse_options(&options).map_err(JsValue::from)?;
    let mut inputs = retile_inputs(inputs).map_err(JsValue::from)?;
    let mut sources: Vec<_> = inputs.iter_mut().map(|(name, source)| RetileSource { name: name.clone(), source }).collect();
    struct Callback<'a> {
        function: &'a js_sys::Function,
        read_file: Option<&'a js_sys::Function>,
        exception: Option<JsValue>,
    }
    impl FolderSink for Callback<'_> {
        fn get(&mut self, name: &str) -> terrio::Result<Option<Vec<u8>>> {
            let Some(read) = self.read_file else { return Ok(None) };
            let value = match read.call1(&JsValue::NULL, &JsValue::from_str(name)) {
                Ok(value) => value,
                Err(error) => {
                    self.exception = Some(error);
                    return Err(terrio::Error::InvalidArgument("retile read-back callback failed".into()));
                }
            };
            if value.is_null() || value.is_undefined() {
                return Ok(None);
            }
            if !value.is_instance_of::<js_sys::Uint8Array>() {
                return Err(terrio::Error::InvalidArgument("readFile must synchronously return a Uint8Array or null".into()));
            }
            let bytes = js_sys::Uint8Array::new(&value);
            if bytes.length() as usize > tile::RenderLimits::default().max_output_bytes + 16 * 1024 * 1024 {
                return Err(terrio::Error::InvalidArgument("parent tile exceeds its encoded-byte budget".into()));
            }
            Ok(Some(bytes.to_vec()))
        }
        fn put(&mut self, name: &str, bytes: &[u8]) -> terrio::Result<()> {
            if let Err(exception) = self.function.call2(&JsValue::NULL, &JsValue::from_str(name), &js_sys::Uint8Array::from(bytes)) {
                self.exception = Some(exception);
                return Err(terrio::Error::InvalidArgument("retile callback failed".into()));
            }
            Ok(())
        }
    }
    let mut sink = Callback { function: on_file, read_file: read_file.as_ref(), exception: None };
    let result = re::generate(&mut sources, &options, &mut sink);
    if let Some(exception) = sink.exception {
        return Err(exception);
    }
    crate::to_js(&result.map_err(|e| JsValue::from(crate::js_err(e)))?).map_err(JsValue::from)
}
