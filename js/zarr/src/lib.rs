// SPDX-License-Identifier: MIT
//! `terrio-js/zarr`: Zarr arrays (v2 and v3, sharded or not) read by window over HTTP with
//! terrio's Zarr driver, and nothing else, for pages that want a small module. The CRS is the EPSG
//! code the store names; `attributes()` has the array's own attributes.
//!
//! ```js
//! import init, { openZarr } from "terrio-js/zarr";
//! await init();
//! const a = await openZarr("https://example.com/store.zarr", { array: "b04" });
//! a.width, a.height, a.count, a.dtype, a.nodata, a.transform, a.epsg, a.attributes();
//! const [band1] = await a.read({ window: [0, 0, 512, 512], bands: [1] }); // typed arrays
//! ```

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use terrio::sparse::{SparseFile, SparseObjects};
use terrio_js_fetch::{Fetcher, MAX_PARALLEL, Remote, get, server_runtime, typed_bands};
use wasm_bindgen::prelude::*;

fn err(e: terrio::Error) -> JsError {
    JsError::new(&e.to_string())
}

struct Inner {
    remote: Remote,
    array: RefCell<terrio::ZarrArray>,
}

/// A Zarr array read over HTTP: see [`open_zarr`]. Its reads return promises.
#[wasm_bindgen]
pub struct ZarrArray {
    inner: Rc<Inner>,
}

/// Opens an array of the Zarr store at `url`. Options: `array` (its path in the store, when the
/// URL is a group: a group's single array otherwise needs consolidated metadata), `cacheSize`
/// (the bytes of objects kept between reads, 256 MB by default), `fetch` and `headers` (for
/// requests), `allowMissingMetadata403` (anonymous S3 stores that answer 403 for absent keys).
#[wasm_bindgen(js_name = openZarr)]
pub async fn open_zarr(url: String, options: JsValue) -> Result<ZarrArray, JsError> {
    if !options.is_undefined() && !options.is_null() && !options.is_object() {
        return Err(JsError::new("openZarr: options must be an object"));
    }
    let (mut fetcher, rest) = Fetcher::from_options(&url, &options)?;
    if rest.is_object() {
        for key in js_sys::Object::keys(&rest.clone().into()).iter().filter_map(|k| k.as_string()) {
            if !["array", "cacheSize", "allowMissingMetadata403"].contains(&key.as_str()) {
                return Err(JsError::new(&format!("openZarr: unknown option `{key}`")));
            }
        }
    }
    let given = |key: &str| Some(get(&rest, key)).filter(|v| !v.is_undefined() && !v.is_null());
    let wrong = |key: &str, what: &str| JsError::new(&format!("openZarr: `{key}` must be {what}"));
    fetcher.allow_missing_metadata403 = match given("allowMissingMetadata403") {
        Some(v) => v.as_bool().ok_or_else(|| wrong("allowMissingMetadata403", "a boolean"))?,
        None => false,
    };
    let key = match given("array") {
        Some(v) => v.as_string().ok_or_else(|| wrong("array", "a string"))?,
        None => String::new(),
    };
    let cache_size = match given("cacheSize") {
        Some(v) => v.as_f64().filter(|n| *n >= 0.0 && n.is_finite()).ok_or_else(|| wrong("cacheSize", "a number of bytes"))? as u64,
        None => 256 << 20,
    };
    let path = url.split(['?', '#']).next().unwrap_or(&url).to_string();
    let objects = Arc::new(SparseObjects::new());
    // Shard reads split into requests that fill those in flight where they are not queued
    // (Node.js, Deno, Bun), as terrio-js does.
    if server_runtime() {
        objects.set_parallel_requests(MAX_PARALLEL);
    }
    let remote = Remote::new(SparseFile::new(0), Some(objects.clone()), fetcher, cache_size);
    let mut opened = None;
    remote
        .run(
            || false,
            || {
                opened = Some(terrio::ZarrArray::open_objects_epsg_only(objects.clone(), &path, &key)?);
                Ok(())
            },
        )
        .await?
        .map_err(err)?;
    let array = opened.ok_or_else(|| JsError::new("Zarr: not opened"))?;
    Ok(ZarrArray { inner: Rc::new(Inner { remote, array: RefCell::new(array) }) })
}

#[wasm_bindgen]
impl ZarrArray {
    fn with<T>(&self, f: impl FnOnce(&terrio::RasterDataset) -> T) -> T {
        f(self.inner.array.borrow().header())
    }

    /// The array's width in pixels.
    #[wasm_bindgen(getter)]
    pub fn width(&self) -> usize {
        self.with(|h| h.raster.cols)
    }

    /// The array's height in pixels.
    #[wasm_bindgen(getter)]
    pub fn height(&self) -> usize {
        self.with(|h| h.raster.rows)
    }

    /// Its bands: every combination of its other dimensions.
    #[wasm_bindgen(getter)]
    pub fn count(&self) -> usize {
        self.with(|h| h.raster.bands)
    }

    /// The sample type: `uint8`, `int16`, `float32`...
    #[wasm_bindgen(getter)]
    pub fn dtype(&self) -> String {
        self.with(|h| h.raster.data_type.as_str().into())
    }

    /// The fill value as nodata, or `undefined`.
    #[wasm_bindgen(getter)]
    pub fn nodata(&self) -> Option<f64> {
        self.with(|h| h.nodata())
    }

    /// The GDAL-style geotransform `[x_origin, pixel_width, 0, y_origin, 0, -pixel_height]`.
    #[wasm_bindgen(getter)]
    pub fn transform(&self) -> Vec<f64> {
        self.with(|h| h.geo_transform().to_vec())
    }

    /// The EPSG code the store names, or `undefined`.
    #[wasm_bindgen(getter)]
    pub fn epsg(&self) -> Option<u32> {
        self.with(|h| h.raster.crs.epsg)
    }

    /// The array's attributes.
    pub fn attributes(&self) -> Result<JsValue, JsError> {
        js_sys::JSON::parse(&self.inner.array.borrow().attributes()).map_err(terrio_js_fetch::js_error)
    }

    /// The requests sent and the bytes they returned: `{ requests, bytes }`.
    pub fn stats(&self) -> JsValue {
        let out = js_sys::Object::new();
        let f = &self.inner.remote.fetcher;
        let _ = js_sys::Reflect::set(&out, &"requests".into(), &(f.requests.get() as f64).into());
        let _ = js_sys::Reflect::set(&out, &"bytes".into(), &(f.bytes.get() as f64).into());
        out.into()
    }

    /// Reads a window of some bands: `{ window: [colOff, rowOff, width, height], bands: [1, 2] }`,
    /// by default the whole array and every band. A promise of one typed array per band.
    pub fn read(&self, options: JsValue) -> js_sys::Promise {
        let inner = self.inner.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let (cols, rows, count) = {
                let a = inner.array.borrow();
                (a.header().raster.cols, a.header().raster.rows, a.header().raster.bands)
            };
            if !options.is_undefined() && !options.is_null() {
                if !options.is_object() {
                    return Err(JsError::new("read: options must be an object").into());
                }
                for key in js_sys::Object::keys(&options.clone().into()).iter().filter_map(|k| k.as_string()) {
                    if !["window", "bands"].contains(&key.as_str()) {
                        return Err(JsError::new(&format!("read: unknown option `{key}`")).into());
                    }
                }
            }
            // Arrays of non-negative integers; anything else is an error, not coerced.
            let numbers = |key: &str| -> Result<Option<Vec<usize>>, JsError> {
                let v = get(&options, key);
                if v.is_undefined() || v.is_null() {
                    return Ok(None);
                }
                let bad = || JsError::new(&format!("read: `{key}` must be an array of non-negative integers"));
                if !js_sys::Array::is_array(&v) {
                    return Err(bad());
                }
                let n = |x: JsValue| x.as_f64().filter(|n| *n >= 0.0 && n.fract() == 0.0 && *n <= u32::MAX as f64).map(|n| n as usize);
                js_sys::Array::from(&v).iter().map(|x| n(x).ok_or_else(bad)).collect::<Result<Vec<_>, _>>().map(Some)
            };
            let window = match numbers("window")?.as_deref() {
                Some(&[x, y, w, h]) => terrio::Window::new(x, y, w, h),
                Some(_) => return Err(JsError::new("read: `window` must be [colOff, rowOff, width, height]").into()),
                None => terrio::Window::new(0, 0, cols, rows),
            };
            // An empty list reads every band, as no list does.
            let wanted = numbers("bands")?.filter(|b| !b.is_empty()).unwrap_or_else(|| (1..=count).collect());
            let data = inner.remote.run(|| false, || inner.array.borrow_mut().read(window, &wanted)).await?.map_err(err)?;
            Ok(typed_bands(data, wanted.len()).into())
        })
    }
}
