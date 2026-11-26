//! Pixel-wise raster operations: `unscale`, `nodataToAlpha`, `reclassify` and `asFeatures`,
//! with camelCase options after the command-line options.

use crate::ops::{input_raster, invalid};
use crate::{Dataset, js_err, parse_options};
use serde::Deserialize;
use terrio::formats::VectorDriver;
use terrio::ops::as_features::{AsFeaturesOptions, PixelGeometry};
use terrio::ops::parse_data_type;
use terrio::ops::pixel::{NodataToAlphaOptions, ReclassifyOptions, UnscaleOptions, mapping_from_file_text};
use wasm_bindgen::prelude::*;

/// Options of [`unscale`].
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct UnscaleArgs {
    ot: Option<String>,
}

/// Applies band scales and offsets (`gdal raster unscale`). Options: `ot`, the output data type
/// (Float32, or Float64 for Float64 sources, by default). Nodata pixels keep the nodata value.
#[wasm_bindgen]
pub fn unscale(src: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let a: UnscaleArgs = parse_options(&options)?;
    let o = UnscaleOptions { output_type: a.ot.as_deref().map(parse_data_type).transpose().map_err(js_err)? };
    let ds = input_raster(&src)?;
    Ok(Dataset::from_raster(terrio::ops::pixel::unscale(&ds, &o).map_err(js_err)?))
}

/// A nodata value: a number, or a string holding one. Strings carry the non-finite values JSON
/// cannot hold (`"nan"`, `"inf"`, `"-inf"`, as [`crate::parse_options`] writes raw `NaN`,
/// `Infinity` and `-Infinity`); any spelling Rust's `f64` parser reads is accepted, case
/// insensitively (`"5"`, `"1e3"`, `"infinity"`, `"+inf"`).
#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum NodataValue {
    Number(f64),
    Text(String),
}

/// The error of a nodata string that is not a number.
const NODATA_TEXT: &str = "nodata must be a number or 'nan', 'inf', '-inf'";

impl NodataValue {
    /// The value, or the error of a string that is not a number.
    fn value(self) -> Result<f64, &'static str> {
        match self {
            Self::Number(value) => Ok(value),
            Self::Text(text) => text.trim().to_ascii_lowercase().parse().map_err(|_| NODATA_TEXT),
        }
    }

    /// The value, or a JavaScript error for a string that is not a number.
    pub(crate) fn number(self) -> Result<f64, JsError> {
        self.value().map_err(invalid)
    }

    /// Parses a nodata string (`"nan"`, `"inf"`, `"-inf"` or a number).
    pub(crate) fn parse(text: &str) -> Result<f64, JsError> {
        Self::Text(text.to_owned()).number()
    }

    /// Parses a nodata word: `None` for `"none"`, otherwise the value of [`Self::parse`].
    pub(crate) fn parse_or_none(text: &str) -> Result<Option<f64>, JsError> {
        if text.trim().eq_ignore_ascii_case("none") { Ok(None) } else { Self::parse(text).map(Some) }
    }
}

/// Serde helpers reading nodata fields as [`NodataValue`]s, so that they accept `"nan"`, `"inf"`
/// and `"-inf"` (and the raw non-finite numbers [`crate::parse_options`] turns into them).
pub(crate) mod nodata_field {
    use super::NodataValue;
    use serde::{Deserialize, Deserializer, de::Error};

    /// A required nodata value.
    pub(crate) fn one<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        NodataValue::deserialize(d)?.value().map_err(D::Error::custom)
    }

    /// An optional nodata value (`null` reads as unset).
    pub(crate) fn option<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        Option::<NodataValue>::deserialize(d)?.map(NodataValue::value).transpose().map_err(D::Error::custom)
    }

    /// A list of nodata values, or one value as a list of one.
    pub(crate) fn list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<f64>, D::Error> {
        let values = match super::Numbers::deserialize(d)? {
            super::Numbers::One(v) => vec![v],
            super::Numbers::Many(v) => v,
        };
        values.into_iter().map(NodataValue::value).collect::<Result<_, _>>().map_err(D::Error::custom)
    }

    /// A JSON array of nodata values (one value is refused, for untagged enums that read it
    /// as another variant).
    pub(crate) fn array<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<f64>, D::Error> {
        Vec::<NodataValue>::deserialize(d)?.into_iter().map(NodataValue::value).collect::<Result<_, _>>().map_err(D::Error::custom)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Numbers {
    One(NodataValue),
    Many(Vec<NodataValue>),
}

/// Options of [`nodata_to_alpha`].
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct NodataToAlphaArgs {
    nodata: Option<Numbers>,
}

/// Replaces nodata values with an alpha band (`gdal raster nodata-to-alpha`). Options:
/// `nodata`, a value overriding the source's, or one per band (transparent where every band has
/// its value). Non-finite overrides are `NaN`, `Infinity`, `-Infinity` or the strings `"nan"`,
/// `"inf"`, `"-inf"`; `null` is refused rather than read as no override.
#[wasm_bindgen(js_name = nodataToAlpha)]
pub fn nodata_to_alpha(src: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    // JSON.stringify keeps null: refuse it so an explicit override never becomes the default.
    if options.is_object() {
        let raw = js_sys::Reflect::get(&options, &JsValue::from_str("nodata")).map_err(|_| invalid("invalid nodata option"))?;
        if raw.is_null() || js_sys::Array::is_array(&raw) && js_sys::Array::from(&raw).iter().any(|v| v.is_null()) {
            return Err(invalid("nodata cannot be null: give a number or 'nan', 'inf', '-inf'"));
        }
    }
    let a: NodataToAlphaArgs = parse_options(&options)?;
    let nodata = match a.nodata {
        None => Vec::new(),
        Some(Numbers::One(v)) => vec![v.number()?],
        Some(Numbers::Many(v)) => v.into_iter().map(NodataValue::number).collect::<Result<_, _>>()?,
    };
    let ds = input_raster(&src)?;
    Ok(Dataset::from_raster(terrio::ops::pixel::nodata_to_alpha(&ds, &NodataToAlphaOptions { nodata }).map_err(js_err)?))
}

/// Options of [`reclassify`].
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct ReclassifyArgs {
    ot: Option<String>,
    mapping_file: Option<String>,
}

/// Maps values or ranges to new values (`gdal raster reclassify`). `mapping` is GDAL's mapping
/// (`"[0,10)=1;[10,inf)=2;NO_DATA=NO_DATA;DEFAULT=PASS_THROUGH"`). Options: `ot`, the output data
/// type; `mappingFile`, the text of a mapping file (one entry per line, `#` comments) used
/// instead of `mapping`.
#[wasm_bindgen]
pub fn reclassify(src: JsValue, mapping: &str, options: JsValue) -> Result<Dataset, JsError> {
    let a: ReclassifyArgs = parse_options(&options)?;
    if a.mapping_file.is_some() && !mapping.is_empty() {
        return Err(invalid("give mapping or mappingFile, not both"));
    }
    let mapping = match a.mapping_file {
        Some(text) => mapping_from_file_text(&text).map_err(js_err)?,
        None if mapping.starts_with('@') => return Err(invalid("files cannot be read here: give their text as `mappingFile`")),
        None => mapping.to_string(),
    };
    let o = ReclassifyOptions { mapping, output_type: a.ot.as_deref().map(parse_data_type).transpose().map_err(js_err)? };
    let ds = input_raster(&src)?;
    Ok(Dataset::from_raster(terrio::ops::pixel::reclassify(&ds, &o).map_err(js_err)?))
}

/// Options of [`as_features`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct AsFeaturesArgs {
    bands: Vec<usize>,
    geometry_type: String,
    skip_nodata: bool,
    include_xy: bool,
    include_row_col: bool,
    layer_name: String,
}

impl Default for AsFeaturesArgs {
    fn default() -> Self {
        Self {
            bands: Vec::new(),
            geometry_type: "none".into(),
            skip_nodata: false,
            include_xy: false,
            include_row_col: false,
            layer_name: "pixels".into(),
        }
    }
}

/// One vector feature per pixel (`gdal raster as-features`). Options: `bands` (from 1, all by
/// default), `geometryType` (`none`, `point` or `polygon`), `skipNodata`, `includeXy`,
/// `includeRowCol`, `layerName` (`pixels`).
#[wasm_bindgen(js_name = asFeatures)]
pub fn as_features(src: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let a: AsFeaturesArgs = parse_options(&options)?;
    let o = AsFeaturesOptions {
        bands: a.bands,
        geometry: PixelGeometry::parse(&a.geometry_type).map_err(js_err)?,
        skip_nodata: a.skip_nodata,
        include_xy: a.include_xy,
        include_row_col: a.include_row_col,
        layer_name: a.layer_name,
    };
    let ds = input_raster(&src)?;
    let layer = terrio::ops::as_features::as_features(&ds, &o).map_err(js_err)?;
    Ok(Dataset::from_layers(vec![layer], VectorDriver::GeoJSON))
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BlendArgs {
    operator: String,
    opacity: u8,
}
impl Default for BlendArgs {
    fn default() -> Self {
        Self { operator: "src-over".into(), opacity: 100 }
    }
}
/// Blends equal-sized Byte images. Options: operator (GDAL name), opacity (0..100).
#[wasm_bindgen]
pub fn blend(src: JsValue, overlay: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let a: BlendArgs = parse_options(&options)?;
    let o = terrio::ops::blend::BlendOptions {
        operator: terrio::ops::blend::BlendOperator::parse(&a.operator).map_err(js_err)?,
        opacity: a.opacity,
    };
    let base = input_raster(&src)?;
    let overlay = input_raster(&overlay)?;
    Ok(Dataset::from_raster(terrio::ops::blend::blend(&base, &overlay, &o).map_err(js_err)?))
}
