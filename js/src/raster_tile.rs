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
