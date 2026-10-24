//! Native multidimensional scientific datasets and lazy typed array views.
use crate::{js_err, parse_options, to_js, typed_array};
use terrio::multidim::{self, ArrayInfo, View};
use wasm_bindgen::prelude::*;

/// A native scientific dataset containing groups, dimensions and lazy numeric arrays.
#[wasm_bindgen(js_name=MultidimDataset)]
pub struct MdDataset {
    inner: multidim::Dataset,
}
#[wasm_bindgen(js_class=MultidimDataset)]
impl MdDataset {
    /// Complete group/array metadata without reading numeric array payloads.
    pub fn info(&self) -> Result<JsValue, JsError> {
        to_js(&self.inner)
    }
    /// Numeric array paths, in group order.
    #[wasm_bindgen(getter,js_name=arrayPaths)]
    pub fn array_paths(&self) -> Vec<String> {
        self.inner.root.arrays_recursive().iter().map(|a| a.info.path.clone()).collect()
    }
    /// Return a lazy numeric array by its absolute path.
    pub fn array(&self, name: &str) -> Result<MdArray, JsError> {
        let array = self.inner.array(name).map_err(js_err)?;
        Ok(MdArray { view: array.view(), info: array.info })
    }
    /// Serialize a numeric snapshot as netCDF-4 bytes.
    #[wasm_bindgen(js_name=toBytes)]
    pub fn to_bytes(&self) -> Result<Vec<u8>, JsError> {
        self.inner.to_netcdf_bytes().map_err(js_err)
    }
    /// Serialize a numeric Zarr v2 snapshot as relative keys mapped to Uint8Array values.
    #[wasm_bindgen(js_name=toZarrStore)]
    pub fn to_zarr_store(&self) -> Result<JsValue, JsError> {
        let files = self.inner.to_zarr_store().map_err(js_err)?;
        let object = js_sys::Object::new();
        for (name, bytes) in files {
            js_sys::Reflect::set(&object, &JsValue::from_str(&name), &js_sys::Uint8Array::from(bytes.as_slice()))
                .map_err(|_| JsError::new("cannot assign Zarr store key"))?;
        }
        Ok(object.into())
    }
}
/// A numeric array with lazy dimension indexing, slicing and transposition.
#[wasm_bindgen(js_name=MultidimArray)]
pub struct MdArray {
    view: View,
    info: ArrayInfo,
}
#[wasm_bindgen(js_class=MultidimArray)]
impl MdArray {
    /// Absolute source-array path.
    #[wasm_bindgen(getter)]
    pub fn path(&self) -> String {
        self.info.path.clone()
    }
    /// Current shape after indexed axes are removed and transposition applied.
    #[wasm_bindgen(getter)]
    pub fn shape(&self) -> Vec<usize> {
        self.view.shape()
    }
    /// Names of the current axes.
    #[wasm_bindgen(getter,js_name=dimensionNames)]
    pub fn dimension_names(&self) -> Vec<String> {
        self.view.dimensions().iter().map(|d| d.name.clone()).collect()
    }
    /// Array metadata with the current view's dimensions and the source chunk layout.
    pub fn info(&self) -> Result<JsValue, JsError> {
        let mut info = self.info.clone();
        info.dimensions = self.view.dimensions();
        to_js(&info)
    }
    /// Select and remove one axis by dimension name, without reading data.
    pub fn isel(&self, dimension: &str, index: usize) -> Result<MdArray, JsError> {
        Ok(Self { view: self.view.isel(dimension, index).map_err(js_err)?, info: self.info.clone() })
    }
    /// Select a named-axis range; a positive step defaults to one. No payload is read yet.
    pub fn slice(&self, dimension: &str, start: usize, count: usize, step: Option<usize>) -> Result<MdArray, JsError> {
        Ok(Self { view: self.view.slice(dimension, start, count, step.unwrap_or(1)).map_err(js_err)?, info: self.info.clone() })
    }
    /// Reorder all remaining axes by name, or reverse their order when omitted.
    pub fn transpose(&self, dimensions: JsValue) -> Result<MdArray, JsError> {
        let current = self.view.dimensions();
        let order: Vec<usize> = if dimensions.is_undefined() || dimensions.is_null() {
            (0..current.len()).rev().collect()
        } else {
            let names: Vec<String> = parse_options(&dimensions)?;
            names
                .iter()
                .map(|name| {
                    current
                        .iter()
                        .position(|d| d.name == *name || d.path == *name)
                        .ok_or_else(|| JsError::new(&format!("unknown dimension '{name}'")))
                })
                .collect::<Result<_, _>>()?
        };
        Ok(Self { view: self.view.transpose(&order).map_err(js_err)?, info: self.info.clone() })
    }
    /// Fetch the selected values as a flat native TypedArray; use shape to interpret its axes.
    /// Int64 and UInt64 values use BigInt64Array and BigUint64Array without a Number conversion.
    pub fn read(&self) -> Result<JsValue, JsError> {
        let buffer = self.view.read().map_err(js_err)?;
        Ok(typed_array(buffer.data().map_err(js_err)?))
    }
}
/// Open netCDF/HDF5 bytes, {bytes,filename}, or a Zarr object of relative keys and Uint8Arrays.
#[wasm_bindgen(js_name=openMultidim)]
pub fn open_multidim(input: JsValue) -> Result<MdDataset, JsError> {
    let inner = if let Some(bytes) = input.dyn_ref::<js_sys::Uint8Array>() {
        multidim::open_bytes(&bytes.to_vec()).map_err(js_err)?
    } else {
        let bytes = js_sys::Reflect::get(&input, &JsValue::from_str("bytes"))
            .map_err(|_| JsError::new("expected scientific file bytes or a Zarr key/value object"))?;
        if let Some(bytes) = bytes.dyn_ref::<js_sys::Uint8Array>() {
            multidim::open_bytes(&bytes.to_vec()).map_err(js_err)?
        } else {
            if !input.is_object() {
                return Err(JsError::new("expected scientific file bytes or a Zarr key/value object"));
            }
            let mut files = Vec::new();
            for entry in js_sys::Object::entries(&js_sys::Object::from(input)).iter() {
                let entry = js_sys::Array::from(&entry);
                let name = entry.get(0).as_string().ok_or_else(|| JsError::new("Zarr keys must be strings"))?;
                let value = entry.get(1);
                let bytes = value.dyn_ref::<js_sys::Uint8Array>().ok_or_else(|| JsError::new("Zarr values must be Uint8Arrays"))?;
                files.push((name, bytes.to_vec()));
            }
            multidim::open_zarr_store(files).map_err(js_err)?
        }
    };
    Ok(MdDataset { inner })
}
