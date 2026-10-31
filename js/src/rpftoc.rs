//! Metadata-only RPF TOC creation for named uploaded CADRG frames.
use crate::{js_err, parse_options};
use terrio::ops::rpftoc::{self, RpfTocOptions};
use wasm_bindgen::prelude::*;
#[derive(serde::Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct Options {
    scale: Option<u32>,
    producer_id: String,
    producer_name: String,
    country_code: String,
    classification: char,
}
impl Default for Options {
    fn default() -> Self {
        let o = RpfTocOptions::default();
        Self {
            scale: o.scale,
            producer_id: o.producer_id,
            producer_name: o.producer_name,
            country_code: o.country_code,
            classification: o.classification,
        }
    }
}
/// Encode A.TOC from [{filename: relative path, bytes: Uint8Array}], without pixel decoding.
/// Preserve those frame files beside the returned TOC. Options: scale, producerId,
/// producerName, countryCode, classification. Metadata is validated before returning bytes.
#[wasm_bindgen(js_name=rpftocCreate)]
pub fn create(frames: JsValue, options: JsValue) -> Result<Vec<u8>, JsError> {
    if !js_sys::Array::is_array(&frames) {
        return Err(JsError::new("frames must be an array of {filename, bytes}"));
    }
    if options.is_object() {
        let scale = js_sys::Reflect::get(&options, &JsValue::from_str("scale")).map_err(|_| JsError::new("invalid scale option"))?;
        if scale.is_null() || scale.as_f64().is_some_and(|v| !v.is_finite()) {
            return Err(JsError::new("scale must be a finite reciprocal scale, or omitted"));
        }
    }
    let o: Options = parse_options(&options)?;
    let options = RpfTocOptions {
        scale: o.scale,
        producer_id: o.producer_id,
        producer_name: o.producer_name,
        country_code: o.country_code,
        classification: o.classification,
    };
    let array = js_sys::Array::from(&frames);
    if array.length() > 100_000 {
        return Err(JsError::new("at most 100000 frames are accepted"));
    }
    let mut metadata = Vec::new();
    for item in array.iter() {
        let name = js_sys::Reflect::get(&item, &JsValue::from_str("filename"))
            .map_err(|_| JsError::new("invalid frame filename"))?
            .as_string()
            .ok_or_else(|| JsError::new("frame filename must be a relative string"))?;
        let data = js_sys::Reflect::get(&item, &JsValue::from_str("bytes")).map_err(|_| JsError::new("invalid frame bytes"))?;
        let data = data.dyn_into::<js_sys::Uint8Array>().map_err(|_| JsError::new("frame bytes must be Uint8Array"))?;
        // Only the bounded metadata ranges are copied from the uploaded array.
        let mut reader = ArrayReader { bytes: data, position: 0 };
        metadata.push(rpftoc::read_frame(&name, &mut reader).map_err(js_err)?);
    }
    rpftoc::encode(&metadata, &options).map_err(js_err)
}
struct ArrayReader {
    bytes: js_sys::Uint8Array,
    position: u64,
}
impl std::io::Read for ArrayReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let start = self.position.min(u64::from(self.bytes.length())) as u32;
        let n = out.len().min((self.bytes.length() - start) as usize);
        self.bytes.subarray(start, start + n as u32).copy_to(&mut out[..n]);
        self.position += n as u64;
        Ok(n)
    }
}
impl std::io::Seek for ArrayReader {
    fn seek(&mut self, from: std::io::SeekFrom) -> std::io::Result<u64> {
        let position = match from {
            std::io::SeekFrom::Start(n) => Some(n),
            std::io::SeekFrom::End(n) => u64::from(self.bytes.length()).checked_add_signed(n),
            std::io::SeekFrom::Current(n) => self.position.checked_add_signed(n),
        };
        self.position = position.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid frame offset"))?;
        Ok(self.position)
    }
}
