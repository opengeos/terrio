//! Dataset operations on `/vsimem/` datasets and read-only archive sources.
use crate::{js_err, parse_options, to_js};
use serde::Deserialize;
use terrio::dataset_files::{self as files, FileOptions};
use wasm_bindgen::prelude::*;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SourceOptions {
    format: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TransferOptions {
    format: Option<String>,
    overwrite: bool,
    #[serde(rename = "allowExternalReferences")]
    allow_external_references: bool,
}
/// Lists owned files. Options: `format`. Referenced VRT assets are not owned.
#[wasm_bindgen(js_name=datasetFiles)]
pub fn dataset_files(source: &str, options: JsValue) -> Result<JsValue, JsError> {
    let options: SourceOptions = parse_options(&options)?;
    to_js(&files::file_list(source, options.format.as_deref()).map_err(js_err)?)
}
/// Copies a dataset file set without conversion. Options: `format`, `overwrite`, `allowExternalReferences`.
#[wasm_bindgen(js_name=datasetCopy)]
pub fn dataset_copy(source: &str, destination: &str, options: JsValue) -> Result<JsValue, JsError> {
    let options: TransferOptions = parse_options(&options)?;
    to_js(
        &files::copy(
            source,
            destination,
            &FileOptions {
                format: options.format,
                overwrite: options.overwrite,
                allow_external_references: options.allow_external_references,
            },
        )
        .map_err(js_err)?,
    )
}
/// Renames a dataset file set. Options: `format`, `overwrite`, `allowExternalReferences`.
#[wasm_bindgen(js_name=datasetRename)]
pub fn dataset_rename(source: &str, destination: &str, options: JsValue) -> Result<JsValue, JsError> {
    let options: TransferOptions = parse_options(&options)?;
    to_js(
        &files::rename(
            source,
            destination,
            &FileOptions {
                format: options.format,
                overwrite: options.overwrite,
                allow_external_references: options.allow_external_references,
            },
        )
        .map_err(js_err)?,
    )
}
/// Deletes owned files of one path or an array of paths. Returns one report per input.
/// Options: `format`. Referenced VRT/catalog source assets are retained.
#[wasm_bindgen(js_name=datasetDelete)]
pub fn dataset_delete(sources: JsValue, options: JsValue) -> Result<JsValue, JsError> {
    let options: SourceOptions = parse_options(&options)?;
    let paths = if let Some(path) = sources.as_string() {
        vec![path]
    } else if js_sys::Array::is_array(&sources) {
        js_sys::Array::from(&sources)
            .iter()
            .map(|value| value.as_string().ok_or_else(|| JsError::new("dataset sources must be paths")))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        return Err(JsError::new("dataset sources must be a path or an array of paths"));
    };
    to_js(&files::delete_many(&paths, options.format.as_deref()).map_err(js_err)?)
}
