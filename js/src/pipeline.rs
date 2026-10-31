//! Pipelines share the Rust engine; browser inputs and outputs remain in memory.
use crate::{Dataset, Inner, js_err};
use serde::Deserialize;
use std::collections::BTreeMap;
use terrio::ops::pipeline::{Pipeline, PipelineData, PipelineResult as CoreResult};
use wasm_bindgen::prelude::*;

/// A pipeline's final Dataset and deferred named output files.
#[wasm_bindgen]
pub struct PipelineResult {
    inner: CoreResult,
}
#[wasm_bindgen]
impl PipelineResult {
    /// Final dataset, or undefined when saving a GDALG definition.
    #[wasm_bindgen(getter)]
    pub fn dataset(&mut self) -> Option<Dataset> {
        self.inner.data.as_mut().map(|data| match data {
            PipelineData::Raster(r) => Dataset::from_lazy(r.shared()),
            PipelineData::Vector(layers) => Dataset::from_layers(layers.clone(), terrio::formats::VectorDriver::GeoJSON),
        })
    }
    /// Destinations requested by write and named materialize steps.
    #[wasm_bindgen(getter, js_name = outputNames)]
    pub fn output_names(&self) -> js_sys::Array {
        self.inner.output_names().iter().map(JsValue::from).collect()
    }
    /// Encode outputs as an object mapping filenames to Uint8Arrays.
    #[wasm_bindgen(js_name = outputBytes)]
    pub fn output_bytes(&mut self) -> Result<js_sys::Object, JsError> {
        let outputs = js_sys::Object::new();
        for (name, bytes) in self.inner.output_bytes().map_err(js_err)? {
            let descriptor = js_sys::Object::new();
            js_sys::Reflect::set(&descriptor, &"value".into(), &js_sys::Uint8Array::from(bytes.as_slice()))
                .map_err(|_| JsError::new("could not describe pipeline output"))?;
            for key in ["enumerable", "writable", "configurable"] {
                js_sys::Reflect::set(&descriptor, &key.into(), &JsValue::TRUE)
                    .map_err(|_| JsError::new("could not describe pipeline output"))?;
            }
            if !js_sys::Reflect::define_property(&outputs, &name.into(), &descriptor)
                .map_err(|_| JsError::new("could not define pipeline output"))?
            {
                return Err(JsError::new("could not define pipeline output"));
            }
        }
        Ok(outputs)
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct Options {
    base_file: Option<String>,
    input_kind: Option<String>,
}

fn input(value: &JsValue, kind: terrio::ops::pipeline::InputKind) -> Result<PipelineData, JsError> {
    if let Some(inner) = Dataset::lookup(value) {
        return Ok(match &mut *inner.borrow_mut() {
            Inner::Raster(r) => PipelineData::Raster(r.shared()),
            Inner::Vector(layers, _) => PipelineData::Vector(layers.clone()),
        });
    }
    if kind != terrio::ops::pipeline::InputKind::Vector
        && let Some((bytes, name)) = crate::ops::file_bytes(value)
    {
        match terrio::memory::open_raster_lazy_bytes(bytes, &name) {
            Ok(raster) => return Ok(PipelineData::Raster(raster)),
            Err(error) if kind == terrio::ops::pipeline::InputKind::Raster => return Err(js_err(error)),
            Err(_) => {}
        }
    }
    Ok(PipelineData::Vector(crate::ops::input_layers(value)?))
}

/// Evaluate a command or GDALG JSON text using aliases from inputs. Values may
/// be Datasets, uploaded {bytes, filename} objects, Uint8Arrays or GeoJSON.
/// No files are created; outputBytes() encodes explicit outputs on request.
#[wasm_bindgen]
pub fn pipeline(command: &str, inputs: JsValue, options: JsValue) -> Result<PipelineResult, JsError> {
    let options: Options = crate::parse_options(&options)?;
    let mut plan = if command.trim_start().starts_with('{') {
        Pipeline::from_gdalg(command, options.base_file.as_deref())
    } else {
        Pipeline::parse(command)
    }
    .map_err(js_err)?;
    if let Some(kind) = options.input_kind {
        plan = plan.with_kind(kind.parse().map_err(js_err)?);
    }
    let mut sources = BTreeMap::new();
    if !inputs.is_null() && !inputs.is_undefined() {
        if !inputs.is_object() || js_sys::Array::is_array(&inputs) {
            return Err(JsError::new("pipeline inputs must be an object mapping aliases to datasets"));
        }
        for key in js_sys::Object::keys(&js_sys::Object::from(inputs.clone())) {
            let name = key.as_string().ok_or_else(|| JsError::new("input alias must be a string"))?;
            let value = js_sys::Reflect::get(&inputs, &key).map_err(|_| JsError::new("could not read pipeline input"))?;
            sources.insert(name, value);
        }
    }
    let inner = plan
        .evaluate_with(|request| {
            let value = sources
                .get(if sources.contains_key(&request.source) { &request.source } else { &request.resolved })
                .ok_or_else(|| terrio::Error::InvalidArgument(format!("missing pipeline input alias: {}", request.source)))?;
            input(value, request.kind)
                .map(Into::into)
                .map_err(|error| terrio::Error::InvalidArgument(String::from(js_sys::Error::from(JsValue::from(error)).message())))
        })
        .map_err(js_err)?;
    Ok(PipelineResult { inner })
}
