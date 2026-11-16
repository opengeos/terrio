// SPDX-License-Identifier: MIT
//! `terrio-js/cog`: GeoTIFFs and COGs read by window and overview over HTTP with terrio's GeoTIFF
//! reader, and nothing else, for pages that want a small module. The CRS is the EPSG code the
//! GeoKeys name; no masks are applied.
//!
//! ```js
//! import init, { openCog } from "terrio-js/cog";
//! await init();
//! const r = await openCog("https://example.com/dem.tif");
//! r.width, r.height, r.count, r.dtype, r.nodata, r.transform, r.epsg, r.overviews;
//! const [band1] = await r.read({ window: [0, 0, 512, 512], bands: [1] }); // typed arrays
//! ```

use std::cell::RefCell;
use std::rc::Rc;
use terrio::{LazyRaster, Window, WindowOptions};
use terrio_js_fetch::{Fetcher, Remote, get, typed_bands};
use wasm_bindgen::prelude::*;

/// The largest integer a JavaScript number holds exactly.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

fn err(e: terrio::Error) -> JsError {
    JsError::new(&e.to_string())
}

struct Inner {
    remote: Remote,
    raster: RefCell<LazyRaster>,
}

/// A GeoTIFF or COG read over HTTP: see [`open_cog`]. Its reads return promises.
#[wasm_bindgen]
pub struct CogRaster {
    inner: Rc<Inner>,
}

/// Opens the GeoTIFF or COG at `url`. Options: `blockSize` (the bytes of each range request's
/// blocks, 64 KiB by default), `cacheSize` (the bytes kept between reads, 256 MB by default),
/// `fetch` and `headers` (for requests).
#[wasm_bindgen(js_name = openCog)]
pub async fn open_cog(url: String, options: JsValue) -> Result<CogRaster, JsError> {
    if !options.is_undefined() && !options.is_null() && !options.is_object() {
        return Err(JsError::new("openCog: options must be an object"));
    }
    let (fetcher, rest) = Fetcher::from_options(&url, &options)?;
    if rest.is_object() {
        for key in js_sys::Object::keys(&rest.clone().into()).iter().filter_map(|k| k.as_string()) {
            if !["blockSize", "cacheSize"].contains(&key.as_str()) {
                return Err(JsError::new(&format!("openCog: unknown option `{key}`")));
            }
        }
    }
    let bytes = |key: &str, default: u64| -> Result<u64, JsError> {
        let v = get(&rest, key);
        if v.is_undefined() || v.is_null() {
            return Ok(default);
        }
        v.as_f64()
            .filter(|n| *n >= 0.0 && n.fract() == 0.0 && *n <= MAX_SAFE_INTEGER)
            .map(|n| n as u64)
            .ok_or_else(|| JsError::new(&format!("openCog: `{key}` must be a whole number of bytes")))
    };
    let block = bytes("blockSize", terrio::sparse::DEFAULT_BLOCK_SIZE)?;
    let cache_size = bytes("cacheSize", 256 << 20)?;
    let (file, _) = fetcher.open_file(block).await?;
    let remote = Remote::new(file, None, fetcher, cache_size);
    let opened = RefCell::new(None);
    let name = url.split(['?', '#']).next().unwrap_or(&url).to_string();
    remote
        .run(
            || opened.borrow().is_none(),
            || {
                let r = LazyRaster::from_sparse_epsg_only(remote.file.clone())?
                    .ok_or_else(|| terrio::Error::InvalidData(format!("'{name}' is not a TIFF")))?;
                *opened.borrow_mut() = Some(r);
                Ok(())
            },
        )
        .await?
        .map_err(err)?;
    let raster = opened.into_inner().ok_or_else(|| JsError::new("GeoTIFF: not opened"))?;
    Ok(CogRaster { inner: Rc::new(Inner { remote, raster: RefCell::new(raster) }) })
}

#[wasm_bindgen]
impl CogRaster {
    fn with<T>(&self, f: impl FnOnce(&LazyRaster) -> T) -> T {
        f(&self.inner.raster.borrow())
    }

    /// The width in pixels.
    #[wasm_bindgen(getter)]
    pub fn width(&self) -> usize {
        self.with(|r| r.width())
    }

    /// The height in pixels.
    #[wasm_bindgen(getter)]
    pub fn height(&self) -> usize {
        self.with(|r| r.height())
    }

    /// The bands.
    #[wasm_bindgen(getter)]
    pub fn count(&self) -> usize {
        self.with(|r| r.band_count())
    }

    /// The sample type: `uint8`, `int16`, `float32`...
    #[wasm_bindgen(getter)]
    pub fn dtype(&self) -> String {
        self.with(|r| r.data_type().as_str().into())
    }

    /// The nodata value of the first band, or `undefined`.
    #[wasm_bindgen(getter)]
    pub fn nodata(&self) -> Option<f64> {
        self.with(|r| r.nodata())
    }

    /// The GDAL-style geotransform `[x_origin, pixel_width, 0, y_origin, 0, -pixel_height]`.
    #[wasm_bindgen(getter)]
    pub fn transform(&self) -> Vec<f64> {
        self.with(|r| r.geo_transform().to_vec())
    }

    /// The EPSG code the GeoKeys name, or `undefined` (a user-defined CRS, or none).
    #[wasm_bindgen(getter)]
    pub fn epsg(&self) -> Option<u32> {
        self.with(|r| r.geotiff_epsg())
    }

    /// The overview sizes `[[width, height], ...]`, largest first.
    #[wasm_bindgen(getter)]
    pub fn overviews(&self) -> js_sys::Array {
        self.with(|r| {
            r.overviews().into_iter().map(|(w, h)| JsValue::from(js_sys::Array::of2(&(w as f64).into(), &(h as f64).into()))).collect()
        })
    }

    /// The requests sent and the bytes they returned: `{ requests, bytes }`.
    pub fn stats(&self) -> JsValue {
        let out = js_sys::Object::new();
        let f = &self.inner.remote.fetcher;
        let _ = js_sys::Reflect::set(&out, &"requests".into(), &(f.requests.get() as f64).into());
        let _ = js_sys::Reflect::set(&out, &"bytes".into(), &(f.bytes.get() as f64).into());
        out.into()
    }

    /// Reads a window of some bands: `{ window: [colOff, rowOff, width, height], bands: [1, 2],
    /// overview: 0 }`, by default the whole image at full resolution and every band; the window is
    /// in the overview's pixels. A promise of one typed array per band.
    pub fn read(&self, options: JsValue) -> js_sys::Promise {
        let inner = self.inner.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            if !options.is_undefined() && !options.is_null() {
                if !options.is_object() {
                    return Err(JsError::new("read: options must be an object").into());
                }
                for key in js_sys::Object::keys(&options.clone().into()).iter().filter_map(|k| k.as_string()) {
                    if !["window", "bands", "overview"].contains(&key.as_str()) {
                        return Err(JsError::new(&format!("read: unknown option `{key}`")).into());
                    }
                }
            }
            let int = |x: JsValue| x.as_f64().filter(|n| *n >= 0.0 && n.fract() == 0.0 && *n <= u32::MAX as f64).map(|n| n as usize);
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
                js_sys::Array::from(&v).iter().map(|x| int(x).ok_or_else(bad)).collect::<Result<Vec<_>, _>>().map(Some)
            };
            let overview = match get(&options, "overview") {
                v if v.is_undefined() || v.is_null() => None,
                v => Some(int(v).ok_or_else(|| JsError::new("read: `overview` must be a non-negative integer"))?),
            };
            let (cols, rows) = {
                let r = inner.raster.borrow();
                match overview {
                    None => (r.width(), r.height()),
                    Some(i) => r.overviews().get(i).copied().ok_or_else(|| JsError::new(&format!("read: no overview {i}")))?,
                }
            };
            let window = match numbers("window")?.as_deref() {
                Some(&[x, y, w, h]) => Window::new(x, y, w, h),
                Some(_) => return Err(JsError::new("read: `window` must be [colOff, rowOff, width, height]").into()),
                None => Window::new(0, 0, cols, rows),
            };
            // An empty list reads every band, as no list does.
            let wanted = numbers("bands")?.unwrap_or_default();
            let count = if wanted.is_empty() { inner.raster.borrow().band_count() } else { wanted.len() };
            let options = WindowOptions { bands: wanted, overview };
            let data =
                inner.remote.run(|| false, || inner.raster.borrow_mut().read_geotiff_samples(window, &options)).await?.map_err(err)?;
            Ok(typed_bands(data, count).into())
        })
    }
}
