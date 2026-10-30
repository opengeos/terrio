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

/// Validate uploaded GeoPackage bytes. Options: fullCheck, strict, unicodeCase.
#[wasm_bindgen(js_name=validateGpkg)]
pub fn validate_gpkg(bytes: &[u8], options: JsValue) -> Result<JsValue, JsError> {
    let options = validation_options(&options)?;
    to_js(&terrio::validate_gpkg_bytes(bytes, &options).map_err(js_err)?)
}
/// Validate an uploaded /vsimem/ GeoPackage path using the same options/report.
#[wasm_bindgen(js_name=validateGpkgPath)]
pub fn validate_gpkg_path(path: &str, options: JsValue) -> Result<JsValue, JsError> {
    let options = validation_options(&options)?;
    to_js(&terrio::validate_gpkg(path, &options).map_err(js_err)?)
}
fn validation_options(value: &JsValue) -> Result<terrio::GpkgValidationOptions, JsError> {
    #[derive(serde::Deserialize, Default)]
    #[serde(default, deny_unknown_fields, rename_all = "camelCase")]
    struct Options {
        full_check: bool,
        strict: bool,
        unicode_case: bool,
    }
    let options: Options = crate::parse_options(value)?;
    Ok(terrio::GpkgValidationOptions { full_check: options.full_check, strict: options.strict, unicode_case: options.unicode_case })
}
