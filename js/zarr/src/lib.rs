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
use terrio::raster::RasterData;
use terrio::sparse::{SparseFile, SparseObjects};
use terrio_js_fetch::{Fetcher, MAX_PARALLEL, Remote, get, server_runtime};
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
    let (mut fetcher, rest) = Fetcher::from_options(&url, &options)?;
    fetcher.allow_missing_metadata403 = get(&rest, "allowMissingMetadata403").as_bool().unwrap_or(false);
    let key = get(&rest, "array").as_string().unwrap_or_default();
    let cache_size = get(&rest, "cacheSize").as_f64().map_or(256 << 20, |v| v as u64);
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

/// Pixels of `bands` bands, band after band, as one typed array of their type per band.
fn bands(data: RasterData, count: usize) -> js_sys::Array {
    macro_rules! split {
        ($v:expr, $t:ty) => {{
            let n = $v.len() / count.max(1);
            $v.chunks(n.max(1)).take(count).map(|b| JsValue::from(<$t>::from(b))).collect()
        }};
    }
    match data {
        RasterData::U8(v) => split!(v, js_sys::Uint8Array),
        RasterData::I8(v) => split!(v, js_sys::Int8Array),
        RasterData::U16(v) => split!(v, js_sys::Uint16Array),
        RasterData::I16(v) => split!(v, js_sys::Int16Array),
        RasterData::U32(v) => split!(v, js_sys::Uint32Array),
        RasterData::I32(v) => split!(v, js_sys::Int32Array),
        RasterData::U64(v) => split!(v, js_sys::BigUint64Array),
        RasterData::I64(v) => split!(v, js_sys::BigInt64Array),
        RasterData::F32(v) => split!(v, js_sys::Float32Array),
        RasterData::F64(v) => split!(v, js_sys::Float64Array),
    }
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
            let numbers = |key: &str| -> Option<Vec<usize>> {
                let v = get(&options, key);
                (!v.is_undefined() && !v.is_null())
                    .then(|| js_sys::Array::from(&v).iter().map(|x| x.as_f64().unwrap_or(0.0) as usize).collect())
            };
            let window = match numbers("window").as_deref() {
                Some(&[x, y, w, h]) => terrio::Window::new(x, y, w, h),
                Some(_) => return Err(JsError::new("window: [colOff, rowOff, width, height]").into()),
                None => terrio::Window::new(0, 0, cols, rows),
            };
            let wanted = numbers("bands").unwrap_or_else(|| (1..=count).collect());
            let data = inner.remote.run(|| false, || inner.array.borrow_mut().read(window, &wanted)).await?.map_err(err)?;
            Ok(bands(data, wanted.len()).into())
        })
    }
}
