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
pub fn raster_tiles(input: &JsValue, options: JsValue, on_tile: &js_sys::Function) -> Result<JsValue, JsError> {
    let options: TileOptions = crate::parse_options(&options)?;
    let mut source = crate::ops::tile_input(input)?;
    struct Callback<'a>(&'a js_sys::Function);
    impl FolderSink for Callback<'_> {
        fn put(&mut self, name: &str, bytes: &[u8]) -> terrio::Result<()> {
            self.0
                .call2(&JsValue::NULL, &JsValue::from_str(name), &js_sys::Uint8Array::from(bytes))
                .map_err(|_| terrio::Error::InvalidArgument("tile callback threw an exception".into()))?;
            Ok(())
        }
    }
    crate::to_js(&tile::generate(&mut source, &options, &mut Callback(on_tile)).map_err(crate::js_err)?)
}
