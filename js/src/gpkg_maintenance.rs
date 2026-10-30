//! GeoPackage maintenance for uploaded datasets.
use crate::{js_err, to_js};
use wasm_bindgen::prelude::*;
/// Repack an uploaded /vsimem/ GeoPackage in place and return physical storage counts.
#[wasm_bindgen(js_name=gpkgRepack)]
pub fn gpkg_repack(path: &str) -> Result<JsValue, JsError> {
    to_js(&terrio::gpkg_maintenance::repack(path).map_err(js_err)?)
}
/// Return a compacted copy of a self-contained GeoPackage image.
#[wasm_bindgen(js_name=gpkgRepackBytes)]
pub fn gpkg_repack_bytes(data: &[u8]) -> Result<Vec<u8>, JsError> {
    terrio::gpkg_maintenance::repack_bytes(data).map_err(js_err)
}
