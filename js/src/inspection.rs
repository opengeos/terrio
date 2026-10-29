//! Header-only identification without copying an entire uploaded file into WASM.
use crate::to_js;
use wasm_bindgen::prelude::*;

/// Identifies Uint8Array or {bytes, filename} inputs from their first 16 KiB.
/// Only that prefix is copied into WASM. Unknown formats return null; this is
/// format identification, not payload-integrity validation.
#[wasm_bindgen(js_name=identify)]
pub fn identify(source: JsValue, filename: Option<String>) -> Result<JsValue, JsError> {
    let (array, name) = if source.is_instance_of::<js_sys::Uint8Array>() {
        (source.unchecked_into::<js_sys::Uint8Array>(), filename.unwrap_or_else(|| "data".into()))
    } else {
        let bytes = js_sys::Reflect::get(&source, &"bytes".into()).map_err(|_| JsError::new("expected Uint8Array or {bytes, filename}"))?;
        let array = bytes.dyn_into::<js_sys::Uint8Array>().map_err(|_| JsError::new("expected Uint8Array file bytes"))?;
        let name = filename
            .or_else(|| js_sys::Reflect::get(&source, &"filename".into()).ok().and_then(|name| name.as_string()))
            .unwrap_or_else(|| "data".into());
        (array, name)
    };
    let length = array.length();
    let prefix = array.subarray(0, length.min(terrio::inspection::IDENTIFICATION_HEADER_BYTES as u32)).to_vec();
    to_js(&terrio::inspection::identify_prefix(&prefix, &name, Some(length as u64)))
}

/// Reads all exposed uploaded content. Returns successful content counts;
/// reader failures throw. This cannot detect mutations without integrity checks.
#[wasm_bindgen(js_name=checkDataset)]
pub fn check_dataset(bytes: Vec<u8>, filename: &str, options: JsValue) -> Result<JsValue, JsError> {
    let options: terrio::check::CheckOptions = crate::parse_options(&options)?;
    let summary = terrio::check::check_bytes(bytes, filename, &options).map_err(crate::js_err)?;
    to_js(&summary)
}
