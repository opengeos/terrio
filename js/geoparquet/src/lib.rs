// SPDX-License-Identifier: MIT
//! `terrio-js/geoparquet`: GeoParquet files read over HTTP by row group, bounding box and column
//! with terrio's GeoParquet reader, out as GeoJSON, and nothing else, for pages that want a small
//! module. The CRS is the EPSG code the file's `crs` names; coordinates are not reprojected.
//!
//! ```js
//! import init, { openGeoParquet } from "terrio-js/geoparquet";
//! await init();
//! const f = await openGeoParquet("https://example.com/parcels.parquet");
//! f.rowGroups, f.epsg;
//! const fc = await f.read({ bbox: [2.2, 48.8, 2.5, 48.9], columns: ["id"] }); // GeoJSON
//! ```

// The GeoJSON of terrio-js's `toGeoJSON`, shared with it.
#[path = "../../src/geojson.rs"]
mod geojson;

use std::rc::Rc;
use terrio::sparse::SparseFile;
use terrio_js_fetch::{Fetcher, Remote, get};
use wasm_bindgen::prelude::*;

/// The largest integer a JavaScript number holds exactly.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

fn err(e: terrio::Error) -> JsError {
    JsError::new(&e.to_string())
}

/// Refuses options that are not an object, or keys not in `known`.
fn check_keys(options: &JsValue, known: &[&str], what: &str) -> Result<(), JsError> {
    if options.is_undefined() || options.is_null() {
        return Ok(());
    }
    if !options.is_object() {
        return Err(JsError::new(&format!("{what}: options must be an object")));
    }
    for key in js_sys::Object::keys(&options.clone().into()).iter().filter_map(|k| k.as_string()) {
        if !known.contains(&key.as_str()) {
            return Err(JsError::new(&format!("{what}: unknown option `{key}`")));
        }
    }
    Ok(())
}

struct Inner {
    remote: Remote,
    /// The file name, without the URL's query: its stem names the layer.
    name: String,
    groups: Vec<terrio::vector::RowGroupInfo>,
    epsg: Option<u32>,
}

/// A GeoParquet file read over HTTP: see [`open_geoparquet`]. Its reads return promises.
#[wasm_bindgen]
pub struct GeoParquetFile {
    inner: Rc<Inner>,
}

/// Opens the GeoParquet file at `url` from its end, where its footer is, and reads its
/// GeoParquet metadata (refusing a file whose metadata terrio does not read). Options: `blockSize`
/// (the bytes of each range request's blocks, 64 KiB by default), `cacheSize` (the bytes kept
/// between reads, 256 MB by default), `fetch` and `headers` (for requests).
#[wasm_bindgen(js_name = openGeoParquet)]
pub async fn open_geoparquet(url: String, options: JsValue) -> Result<GeoParquetFile, JsError> {
    if !options.is_undefined() && !options.is_null() && !options.is_object() {
        return Err(JsError::new("openGeoParquet: options must be an object"));
    }
    let (fetcher, rest) = Fetcher::from_options(&url, &options)?;
    check_keys(&rest, &["blockSize", "cacheSize"], "openGeoParquet")?;
    let bytes = |key: &str, default: u64| -> Result<u64, JsError> {
        let v = get(&rest, key);
        if v.is_undefined() || v.is_null() {
            return Ok(default);
        }
        v.as_f64()
            .filter(|n| *n >= 0.0 && n.fract() == 0.0 && *n <= MAX_SAFE_INTEGER)
            .map(|n| n as u64)
            .ok_or_else(|| JsError::new(&format!("openGeoParquet: `{key}` must be a whole number of bytes")))
    };
    let block = bytes("blockSize", terrio::sparse::DEFAULT_BLOCK_SIZE)?;
    let cache_size = bytes("cacheSize", 256 << 20)?;
    let path = url.split(['?', '#']).next().unwrap_or(&url);
    let name = path.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("remote").to_string();
    // From its end, or from its first block when the server refuses suffix ranges: the footer is
    // then fetched by the reads below, as it is for `openUrl`.
    let file = fetcher.open_file_tail(block).await?;
    let remote = Remote::new(file, None, fetcher, cache_size);
    let not_parquet = || terrio::Error::InvalidData(format!("'{name}' is not a Parquet file"));
    let opened = std::cell::RefCell::new(None);
    remote
        .run(
            || opened.borrow().is_none(),
            || {
                let file: &SparseFile = &remote.file;
                if file.len() < 4 || file.read_exact_at(file.len() - 4, 4)? != b"PAR1" {
                    return Err(not_parquet());
                }
                let groups = terrio::vector::geoparquet_row_groups_sparse(file)?;
                // No row group: the footer alone, for the layer's CRS.
                let layer = terrio::vector::read_geoparquet_sparse_epsg_only(&remote.file, &name, Some(&[]), None, None)?;
                *opened.borrow_mut() = Some((groups, layer.crs_epsg()));
                Ok(())
            },
        )
        .await?
        .map_err(err)?;
    let (groups, epsg) = opened.into_inner().ok_or_else(|| JsError::new("GeoParquet: not opened"))?;
    Ok(GeoParquetFile { inner: Rc::new(Inner { remote, name, groups, epsg }) })
}

#[wasm_bindgen]
impl GeoParquetFile {
    /// The row groups: `[{ rows, first_row, bbox, bytes }, ...]`, as `openUrl`'s `info()` gives
    /// them.
    #[wasm_bindgen(getter, js_name = rowGroups)]
    pub fn row_groups(&self) -> Result<JsValue, JsError> {
        let text = serde_json::to_string(&self.inner.groups).map_err(|e| JsError::new(&e.to_string()))?;
        js_sys::JSON::parse(&text).map_err(|_| JsError::new("rowGroups"))
    }

    /// The EPSG code the file's `crs` names, read from its text (4326 when it has none), or
    /// `undefined` (`null`, or a definition without a code): see the README.
    #[wasm_bindgen(getter)]
    pub fn epsg(&self) -> Option<u32> {
        self.inner.epsg
    }

    /// The requests sent and the bytes they returned: `{ requests, bytes }`.
    pub fn stats(&self) -> JsValue {
        let out = js_sys::Object::new();
        let f = &self.inner.remote.fetcher;
        let _ = js_sys::Reflect::set(&out, &"requests".into(), &(f.requests.get() as f64).into());
        let _ = js_sys::Reflect::set(&out, &"bytes".into(), &(f.bytes.get() as f64).into());
        out.into()
    }

    /// Reads features as a GeoJSON FeatureCollection, in the file's CRS: `{ rowGroups: [0, 2],
    /// bbox: [minX, minY, maxX, maxY], columns: ["id", "name"] }`, by default every row group and
    /// column. With `bbox`, row groups are skipped by their statistics and the features are those
    /// that intersect it, as `ogr2ogr -spat` keeps them; `columns` are matched without case.
    /// A promise of the features `openUrl(url).read(options)` then `toGeoJSON()` give, with the
    /// same requests for a URL whose file name ends in `.parquet`.
    pub fn read(&self, options: JsValue) -> js_sys::Promise {
        let inner = self.inner.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            check_keys(&options, &["rowGroups", "bbox", "columns"], "read")?;
            let array = |key: &str| -> Result<Option<js_sys::Array>, JsError> {
                let v = get(&options, key);
                if v.is_undefined() || v.is_null() {
                    return Ok(None);
                }
                if !js_sys::Array::is_array(&v) {
                    return Err(JsError::new(&format!("read: `{key}` must be an array")));
                }
                Ok(Some(js_sys::Array::from(&v)))
            };
            let row_groups = array("rowGroups")?
                .map(|a| {
                    a.iter()
                        .map(|x| x.as_f64().filter(|n| *n >= 0.0 && n.fract() == 0.0 && *n <= u32::MAX as f64).map(|n| n as usize))
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(|| JsError::new("read: `rowGroups` must be an array of non-negative integers"))
                })
                .transpose()?;
            let bbox = match array("bbox")? {
                None => None,
                Some(a) => match a.iter().map(|x| x.as_f64().filter(|n| n.is_finite())).collect::<Option<Vec<_>>>().as_deref() {
                    Some(&[a, b, c, d]) => Some((a, b, c, d)),
                    _ => return Err(JsError::new("read: `bbox` must be [minX, minY, maxX, maxY]").into()),
                },
            };
            let columns = array("columns")?
                .map(|a| {
                    a.iter()
                        .map(|x| x.as_string())
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(|| JsError::new("read: `columns` must be an array of strings"))
                })
                .transpose()?;
            let mut layer = inner
                .remote
                .run(
                    || false,
                    || {
                        terrio::vector::read_geoparquet_sparse_epsg_only(
                            &inner.remote.file,
                            &inner.name,
                            row_groups.as_deref(),
                            bbox,
                            columns.as_deref(),
                        )
                    },
                )
                .await?
                .map_err(err)?;
            // The features of the row groups kept that intersect the rectangle.
            if let Some(rect) = bbox {
                let filter = terrio::ops::vector_filter::SpatialFilter::rect(rect);
                layer.features.retain(|f| f.geometry.as_ref().is_some_and(|g| filter.intersects(g)));
            }
            Ok(geojson::feature_collection(&layer)?)
        })
    }
}
