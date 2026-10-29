//! Metadata aggregation on filesystem paths, including uploaded /vsimem/ partitions.
use crate::{js_err, parse_options, to_js};
use serde::Deserialize;
use wasm_bindgen::prelude::*;
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Options {
    overwrite: bool,
}
/// Create a Parquet metadata-only file. Options: `overwrite`.
#[wasm_bindgen(js_name=parquetCreateMetadata)]
pub fn parquet_create_metadata(inputs: Vec<String>, output: &str, options: JsValue) -> Result<JsValue, JsError> {
    let options: Options = parse_options(&options)?;
    to_js(
        &terrio::parquet_metadata::create(&inputs, output, &terrio::parquet_metadata::MetadataOptions { overwrite: options.overwrite })
            .map_err(js_err)?,
    )
}
