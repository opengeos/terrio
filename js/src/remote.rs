//! Remote files read lazily with HTTP range requests through `fetch`: COGs by window and
//! overview, PMTiles tiles, COPC point clouds by bounds and level, FlatGeobuf by bounding box (with
//! its spatial index) and GeoParquet by row group.
//!
//! A read runs terrio's synchronous reader over the blocks fetched so far
//! ([`terrio::sparse::SparseFile`]); when it needs blocks not fetched yet, they are fetched (in
//! parallel requests, nearby ranges merged) and the read runs again, until it has all it needs.

use crate::{Dataset, crs_string, dtype_name, js_err, parse_options, to_js, typed_array};
use serde::Deserialize;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use terrio::formats::VectorDriver;
use terrio::pointcloud::PointCloudReader;
use terrio::sparse::SparseFile;
use terrio::tiles::TileArchive;
use terrio::vector::{FlatGeobufInfo, RowGroupInfo};
use terrio::{LazyRaster, Window, WindowOptions};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// The most rounds of fetches a read may take.
const MAX_ROUNDS: usize = 64;

/// The range requests sent at once: browsers queue more, and servers may rate-limit them.
const MAX_PARALLEL: usize = 12;

/// Options of [`open_url`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct OpenUrlOptions {
    /// The block size in bytes: reads fetch whole blocks.
    block_size: u64,
    /// The bytes kept in the block cache between reads.
    cache_size: u64,
    /// The format, when the URL's extension does not tell it: `GTiff`, `PMTiles`, `COPC`,
    /// `FlatGeobuf` or `Parquet`.
    format: Option<String>,
}

impl Default for OpenUrlOptions {
    fn default() -> Self {
        Self { block_size: terrio::sparse::DEFAULT_BLOCK_SIZE, cache_size: 256 << 20, format: None }
    }
}

/// Sends range requests with `fetch`, counting them.
struct Fetcher {
    url: String,
    fetch: js_sys::Function,
    headers: JsValue,
    requests: Cell<u64>,
    bytes: Cell<u64>,
}

/// What a range request returned.
struct Fetched {
    status: u16,
    /// The file's length, from `Content-Range`.
    total: Option<u64>,
    data: Vec<u8>,
}

fn get(v: &JsValue, key: &str) -> JsValue {
    js_sys::Reflect::get(v, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

/// Calls a method of a JavaScript object.
fn call(obj: &JsValue, method: &str, args: &[JsValue]) -> Result<JsValue, JsError> {
    let f: js_sys::Function = get(obj, method).dyn_into().map_err(|_| JsError::new(&format!("no method {method}")))?;
    let a = js_sys::Array::new();
    for x in args {
        a.push(x);
    }
    f.apply(obj, &a).map_err(js_error)
}

/// A JavaScript exception as an error.
fn js_error(e: JsValue) -> JsError {
    let msg = e.dyn_ref::<js_sys::Error>().map(|e| String::from(e.message())).or_else(|| e.as_string()).unwrap_or_else(|| format!("{e:?}"));
    JsError::new(&msg)
}

impl Fetcher {
    /// Starts a request for bytes `first..=last` (all of them with `None`), or a `HEAD` request.
    fn request(&self, range: Option<(u64, u64)>, head: bool) -> Result<js_sys::Promise, JsError> {
        let headers = js_sys::Object::new();
        if self.headers.is_object() {
            js_sys::Object::assign(&headers, &js_sys::Object::from(self.headers.clone()));
        }
        if let Some((a, b)) = range {
            js_sys::Reflect::set(&headers, &"Range".into(), &format!("bytes={a}-{b}").into()).map_err(js_error)?;
        }
        let init = js_sys::Object::new();
        js_sys::Reflect::set(&init, &"headers".into(), &headers).map_err(js_error)?;
        if head {
            js_sys::Reflect::set(&init, &"method".into(), &"HEAD".into()).map_err(js_error)?;
        }
        let p = self.fetch.call2(&JsValue::UNDEFINED, &JsValue::from_str(&self.url), &init).map_err(js_error)?;
        p.dyn_into::<js_sys::Promise>().map_err(|_| JsError::new("fetch did not return a promise"))
    }

    /// Waits for a response and reads it.
    async fn response(&self, promise: js_sys::Promise) -> Result<Fetched, JsError> {
        let resp = JsFuture::from(promise).await.map_err(js_error)?;
        let status = get(&resp, "status").as_f64().unwrap_or(0.0) as u16;
        let header = |name: &str| -> Option<String> { call(&get(&resp, "headers"), "get", &[name.into()]).ok()?.as_string() };
        let total = header("content-range").and_then(|r| r.rsplit('/').next()?.trim().parse().ok());
        let buf = JsFuture::from(call(&resp, "arrayBuffer", &[])?.dyn_into::<js_sys::Promise>().map_err(|_| JsError::new("no body"))?)
            .await
            .map_err(js_error)?;
        let data = js_sys::Uint8Array::new(&buf).to_vec();
        self.requests.set(self.requests.get() + 1);
        self.bytes.set(self.bytes.get() + data.len() as u64);
        if !(200..300).contains(&status) {
            return Err(JsError::new(&format!("{}: HTTP status {status}", self.url)));
        }
        Ok(Fetched { status, total, data })
    }

    /// The length of the file, from a `HEAD` request's `Content-Length`.
    async fn head_len(&self) -> Result<Option<u64>, JsError> {
        let resp = JsFuture::from(self.request(None, true)?).await.map_err(js_error)?;
        let len = call(&get(&resp, "headers"), "get", &["content-length".into()])?.as_string();
        Ok(len.and_then(|l| l.trim().parse().ok()))
    }

    /// Fetches byte ranges `(offset, length)` into the file, [`MAX_PARALLEL`] requests at a time.
    async fn fetch_into(&self, file: &SparseFile, ranges: &[(u64, u64)]) -> Result<(), JsError> {
        for batch in ranges.chunks(MAX_PARALLEL) {
            self.fetch_batch(file, batch).await?;
        }
        Ok(())
    }

    /// Fetches byte ranges in parallel into the file.
    async fn fetch_batch(&self, file: &SparseFile, ranges: &[(u64, u64)]) -> Result<(), JsError> {
        let promises: Vec<js_sys::Promise> =
            ranges.iter().map(|&(o, n)| self.request(Some((o, o + n - 1)), false)).collect::<Result<_, _>>()?;
        for (&(offset, len), p) in ranges.iter().zip(promises) {
            let f = self.response(p).await?;
            match f.status {
                206 if f.data.len() as u64 == len => file.insert(offset, &f.data),
                200 if f.data.len() as u64 == file.len() => file.insert(0, &f.data),
                status => {
                    return Err(JsError::new(&format!(
                        "{}: bytes {offset}-{}: unexpected response (status {status}, {} bytes)",
                        self.url,
                        offset + len - 1,
                        f.data.len()
                    )));
                }
            }
        }
        Ok(())
    }
}

/// What a remote file is read as.
enum Reader {
    Raster(LazyRaster),
    Tiles(TileArchive, Option<LazyRaster>),
    Points(Box<PointCloudReader>),
    FlatGeobuf(FlatGeobufInfo),
    Parquet(Vec<RowGroupInfo>),
}

struct State {
    file: SparseFile,
    fetcher: Fetcher,
    name: String,
    format: &'static str,
    cache_size: u64,
    reader: RefCell<Option<Reader>>,
    /// The end of the last read queued: reads run one at a time, as they share the blocks
    /// fetched, the blocks missed and the cache's trimming.
    queue: RefCell<js_sys::Promise>,
}

/// A read's turn, which lets the next queued read run when dropped.
struct Turn(js_sys::Function);

impl Drop for Turn {
    fn drop(&mut self) {
        let _ = self.0.call0(&JsValue::NULL);
    }
}

impl State {
    /// Waits for the reads queued before this one to finish.
    async fn turn(&self) -> Turn {
        let mut resolve = None;
        let mine = js_sys::Promise::new(&mut |res, _| resolve = Some(res));
        let previous = self.queue.replace(mine);
        let _ = wasm_bindgen_futures::JsFuture::from(previous).await;
        Turn(resolve.expect("a promise's executor runs at once"))
    }

    /// Runs a read until it no longer misses blocks, fetching them in between. Reads of one
    /// dataset run one after another, even when their promises are awaited together.
    async fn run<T>(&self, mut read: impl FnMut(&mut Option<Reader>) -> terrio::Result<T>) -> Result<T, JsError> {
        let _turn = self.turn().await;
        for _ in 0..MAX_ROUNDS {
            let result = read(&mut self.reader.borrow_mut());
            let missing = self.file.missing();
            if missing.is_empty() {
                self.file.trim(self.cache_size);
                return result.map_err(js_err);
            }
            self.fetcher.fetch_into(&self.file, &missing).await?;
        }
        Err(JsError::new(&format!("{}: the read still needed more bytes after {MAX_ROUNDS} rounds of requests", self.fetcher.url)))
    }
}

/// A remote file read lazily with HTTP range requests: see [`open_url`]. Its reads return
/// promises.
#[wasm_bindgen]
pub struct RemoteDataset {
    state: Rc<State>,
}

/// The format of a file, from its name or first bytes.
fn detect(name: &str, head: &[u8], forced: Option<&str>) -> Result<&'static str, JsError> {
    if let Some(f) = forced {
        return match f.to_ascii_lowercase().as_str() {
            "gtiff" | "cog" | "geotiff" => Ok("GTiff"),
            "pmtiles" => Ok("PMTiles"),
            "copc" | "las" | "laz" => Ok("COPC"),
            "flatgeobuf" | "fgb" => Ok("FlatGeobuf"),
            "parquet" | "geoparquet" => Ok("Parquet"),
            _ => Err(JsError::new(&format!("format '{f}' is not read remotely (GTiff, PMTiles, COPC, FlatGeobuf, Parquet)"))),
        };
    }
    if matches!(head.get(..4), Some(b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+")) {
        return Ok("GTiff");
    }
    if head.starts_with(b"PMTiles") {
        return Ok("PMTiles");
    }
    if head.starts_with(b"LASF") {
        return Ok("COPC");
    }
    if head.starts_with(b"fgb") {
        return Ok("FlatGeobuf");
    }
    let lower = name.to_ascii_lowercase();
    if head.starts_with(b"PAR1") || lower.ends_with(".parquet") || lower.ends_with(".geoparquet") {
        return Ok("Parquet");
    }
    Err(JsError::new(&format!("'{name}': not a format read remotely (COG/GeoTIFF, PMTiles, COPC, FlatGeobuf, GeoParquet)")))
}

/// Opens a remote file for reading lazily with HTTP range requests (`fetch`): a COG or other
/// GeoTIFF, a PMTiles archive, a COPC point cloud, a FlatGeobuf file or a GeoParquet file. Only
/// its header is read now, in a request or two; reads fetch only the bytes they need.
///
/// Options: `headers` (sent with every request, for authorization), `fetch` (the function to
/// call instead of `globalThis.fetch`), `blockSize` (64 KiB: reads fetch whole blocks),
/// `cacheSize` (256 MB kept between reads), `format` (when the URL does not tell it).
///
/// Servers must answer range requests; in browsers, cross-origin servers must allow the `Range`
/// header and expose `Content-Range` (CORS).
#[wasm_bindgen(js_name = openUrl)]
pub async fn open_url(url: String, options: JsValue) -> Result<RemoteDataset, JsError> {
    let (rest, fetch, headers) = {
        let (fetch, headers) =
            if options.is_object() { (get(&options, "fetch"), get(&options, "headers")) } else { (JsValue::UNDEFINED, JsValue::UNDEFINED) };
        let rest = if options.is_object() {
            let copy = js_sys::Object::assign(&js_sys::Object::new(), &js_sys::Object::from(options.clone()));
            let _ = js_sys::Reflect::delete_property(&copy, &"fetch".into());
            let _ = js_sys::Reflect::delete_property(&copy, &"headers".into());
            copy.into()
        } else {
            options.clone()
        };
        (rest, fetch, headers)
    };
    let o: OpenUrlOptions = parse_options(&rest)?;
    let fetch: js_sys::Function = if fetch.is_function() {
        fetch.unchecked_into()
    } else {
        get(&js_sys::global(), "fetch").dyn_into().map_err(|_| JsError::new("no fetch function: pass one as options.fetch"))?
    };
    let fetcher = Fetcher { url: url.clone(), fetch, headers, requests: Cell::new(0), bytes: Cell::new(0) };
    let block = o.block_size.max(512);
    // The first block: the file's length, and its first bytes.
    let first = fetcher.response(fetcher.request(Some((0, block - 1)), false)?).await?;
    // A server that ignores `Range` sends the whole file: every block is then held, and nothing
    // more is requested.
    let len = match (first.status, first.total) {
        (206, Some(total)) => total,
        (206, None) => match fetcher.head_len().await? {
            Some(len) => len,
            None => {
                return Err(JsError::new(&format!(
                    "{url}: the server's response has no Content-Range (cross-origin servers must expose it) and no Content-Length"
                )));
            }
        },
        _ => first.data.len() as u64,
    };
    let file = SparseFile::with_block_size(len, block);
    file.insert(0, &first.data);
    let path = url.split(['?', '#']).next().unwrap_or(&url);
    let name = path.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("remote").to_string();
    let format = detect(&name, &first.data, o.format.as_deref())?;
    let state = Rc::new(State {
        file,
        fetcher,
        name,
        format,
        cache_size: o.cache_size,
        reader: RefCell::new(None),
        queue: RefCell::new(js_sys::Promise::resolve(&JsValue::UNDEFINED)),
    });
    let (file, name) = (state.file.clone(), state.name.clone());
    state
        .run(|r| {
            *r = Some(match format {
                "GTiff" => Reader::Raster(
                    LazyRaster::from_sparse(file.clone())?.ok_or_else(|| terrio::Error::InvalidData(format!("'{name}' is not a TIFF")))?,
                ),
                "PMTiles" => {
                    let archive = TileArchive::from_sparse(file.clone())?;
                    let raster = if archive.is_vector() {
                        None
                    } else {
                        Some(LazyRaster::from_tile_archive(TileArchive::from_sparse(file.clone())?)?)
                    };
                    Reader::Tiles(archive, raster)
                }
                "COPC" => Reader::Points(Box::new(terrio::pointcloud::open_points_sparse(file.clone(), &name)?)),
                "FlatGeobuf" => {
                    let (info, _) = terrio::vector::read_flatgeobuf_header_sparse(&file, &name)?;
                    Reader::FlatGeobuf(info)
                }
                _ => Reader::Parquet(terrio::vector::geoparquet_row_groups_sparse(&file)?),
            });
            Ok(())
        })
        .await?;
    Ok(RemoteDataset { state })
}

/// Options of [`RemoteDataset::read`].
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct ReadArgs {
    /// Rasters: a window `[colOff, rowOff, width, height]`.
    window: Option<[usize; 4]>,
    /// Rasters: an overview, from 0.
    overview: Option<usize>,
    /// Rasters: the bands, from 1.
    bands: Option<Vec<usize>>,
    /// Vector data: a rectangle `[minX, minY, maxX, maxY]` in the layer's CRS (EPSG:3857 for
    /// vector tiles).
    bbox: Option<[f64; 4]>,
    /// GeoParquet: the row groups, from 0.
    row_groups: Option<Vec<usize>>,
    /// Vector tiles: the zoom level (the highest by default).
    zoom: Option<u8>,
    /// Vector tiles: the layers.
    layers: Option<Vec<String>>,
}

/// Keeps the features that intersect a rectangle, as `ogr2ogr -spat` does.
fn spatial_filter(layers: &[terrio::Layer], bbox: Option<[f64; 4]>) -> terrio::Result<Vec<terrio::Layer>> {
    match bbox {
        None => Ok(layers.to_vec()),
        Some([a, b, c, d]) => {
            let o = terrio::ops::vector_filter::VectorTranslateOptions { spatial_filter: Some((a, b, c, d)), ..Default::default() };
            terrio::ops::vector_filter::translate(layers, &o)
        }
    }
}

impl RemoteDataset {
    fn with_raster<T>(&self, f: impl FnOnce(&mut LazyRaster) -> T) -> Result<T, JsError> {
        match self.state.reader.borrow_mut().as_mut() {
            Some(Reader::Raster(r)) | Some(Reader::Tiles(_, Some(r))) => Ok(f(r)),
            _ => Err(JsError::new(&format!("'{}' is not a raster", self.state.name))),
        }
    }

    fn window(r: &LazyRaster, window: Option<[usize; 4]>, overview: Option<usize>) -> terrio::Result<Window> {
        Ok(match window {
            Some([x, y, w, h]) => Window::new(x, y, w, h),
            None => {
                let (w, h) = match overview {
                    None => (r.width(), r.height()),
                    Some(i) => r.overviews().get(i).copied().ok_or_else(|| terrio::Error::InvalidArgument(format!("no overview {i}")))?,
                };
                Window::full(w, h)
            }
        })
    }
}

#[wasm_bindgen]
impl RemoteDataset {
    /// `"raster"` (a GeoTIFF, or PMTiles of images), `"tiles"` (PMTiles of vector tiles),
    /// `"pointcloud"` (COPC) or `"vector"` (FlatGeobuf, GeoParquet).
    #[wasm_bindgen(getter)]
    pub fn kind(&self) -> String {
        match self.state.reader.borrow().as_ref() {
            Some(Reader::Raster(_)) | Some(Reader::Tiles(_, Some(_))) => "raster",
            Some(Reader::Tiles(_, None)) => "tiles",
            Some(Reader::Points(_)) => "pointcloud",
            _ => "vector",
        }
        .into()
    }

    /// The format: `GTiff`, `PMTiles`, `COPC`, `FlatGeobuf` or `Parquet`.
    #[wasm_bindgen(getter)]
    pub fn format(&self) -> String {
        self.state.format.into()
    }

    /// The URL.
    #[wasm_bindgen(getter)]
    pub fn url(&self) -> String {
        self.state.fetcher.url.clone()
    }

    /// The file's size in bytes.
    #[wasm_bindgen(getter)]
    pub fn size(&self) -> f64 {
        self.state.file.len() as f64
    }

    /// The requests sent and the bytes received so far: `{ requests, bytes }`.
    pub fn stats(&self) -> Result<JsValue, JsError> {
        to_js(&serde_json::json!({ "requests": self.state.fetcher.requests.get(), "bytes": self.state.fetcher.bytes.get() }))
    }

    /// What the header says: for rasters, the summary `terrio info --json` prints; for tiles, the
    /// zoom levels, bounds, tile format and metadata; for point clouds, the LAS header; for
    /// FlatGeobuf, the feature count, extent and index; for GeoParquet, the row groups.
    pub fn info(&self) -> Result<JsValue, JsError> {
        let mut reader = self.state.reader.borrow_mut();
        match reader.as_mut() {
            Some(Reader::Raster(r)) | Some(Reader::Tiles(_, Some(r))) => {
                let info = terrio::info::lazy_raster_info(r, &Default::default()).map_err(js_err)?;
                to_js(&terrio::info::Info::Raster(Box::new(info)))
            }
            Some(Reader::Tiles(a, None)) => to_js(&serde_json::json!({
                "format": a.format().name(),
                "zoomRange": a.zoom_range(),
                "bounds": a.bounds(),
                "metadata": a.metadata().into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect::<serde_json::Map<_, _>>(),
            })),
            Some(Reader::Points(p)) => to_js(p.header()),
            Some(Reader::FlatGeobuf(i)) => to_js(i),
            Some(Reader::Parquet(g)) => to_js(&serde_json::json!({ "rowGroups": g })),
            None => Err(JsError::new("not open")),
        }
    }

    /// Rasters: the width in pixels.
    #[wasm_bindgen(getter)]
    pub fn width(&self) -> Result<usize, JsError> {
        self.with_raster(|r| r.width())
    }

    /// Rasters: the height in pixels.
    #[wasm_bindgen(getter)]
    pub fn height(&self) -> Result<usize, JsError> {
        self.with_raster(|r| r.height())
    }

    /// Rasters: the number of bands.
    #[wasm_bindgen(getter)]
    pub fn count(&self) -> Result<usize, JsError> {
        self.with_raster(|r| r.band_count())
    }

    /// Rasters: the pixel type.
    #[wasm_bindgen(getter)]
    pub fn dtype(&self) -> Result<String, JsError> {
        self.with_raster(|r| dtype_name(r.data_type()).to_string())
    }

    /// Rasters: the nodata value.
    #[wasm_bindgen(getter)]
    pub fn nodata(&self) -> Result<Option<f64>, JsError> {
        self.with_raster(|r| r.nodata())
    }

    /// Rasters: the GDAL-style geotransform.
    #[wasm_bindgen(getter)]
    pub fn transform(&self) -> Result<Vec<f64>, JsError> {
        self.with_raster(|r| r.geo_transform().to_vec())
    }

    /// Rasters: the overview sizes `[[width, height], ...]`, largest first.
    #[wasm_bindgen(getter)]
    pub fn overviews(&self) -> Result<JsValue, JsError> {
        to_js(&self.with_raster(|r| r.overviews())?)
    }

    /// The CRS: `EPSG:xxxx` when its code is known, otherwise its WKT.
    #[wasm_bindgen(getter)]
    pub fn crs(&self) -> Option<String> {
        match self.state.reader.borrow().as_ref()? {
            Reader::Raster(r) | Reader::Tiles(_, Some(r)) => crs_string(&r.crs()),
            Reader::Tiles(_, None) => Some("EPSG:3857".into()),
            Reader::Points(p) => crs_string(p.crs()),
            Reader::FlatGeobuf(_) | Reader::Parquet(_) => None,
        }
    }

    /// Rasters: reads a band (from 1) as a typed array, fetching only the tiles of the window
    /// (`[colOff, rowOff, width, height]`, in the overview's pixels with `overview`).
    #[wasm_bindgen(js_name = readBand)]
    pub fn read_band(&self, band: usize, window: Option<Vec<usize>>, overview: Option<usize>) -> js_sys::Promise {
        let me = RemoteDataset { state: self.state.clone() };
        wasm_bindgen_futures::future_to_promise(async move {
            let window = match window.as_deref() {
                None => None,
                Some(&[x, y, w, h]) => Some([x, y, w, h]),
                Some(_) => return Err(JsError::new("window is [col_off, row_off, width, height]").into()),
            };
            let opts = WindowOptions { bands: vec![band], overview };
            let ds = me
                .state
                .run(|r| match r.as_mut() {
                    Some(Reader::Raster(r)) | Some(Reader::Tiles(_, Some(r))) => {
                        let w = Self::window(r, window, overview)?;
                        r.read_window(w, &opts)
                    }
                    _ => Err(terrio::Error::InvalidArgument("not a raster".into())),
                })
                .await?;
            Ok(typed_array(ds.raster.data))
        })
    }

    /// Reads into memory, as a `Dataset` (rasters, vector data) or a `PointCloud`:
    ///
    /// - rasters: `window`, `overview`, `bands`;
    /// - vector tiles: `zoom` (the highest by default), `layers`, `bbox` (EPSG:3857);
    /// - COPC: the options of `readPoints` (`bounds`, `zRange`, `classes`, `returns`,
    ///   `maxLevel`, `resolution`, `maxPoints`), fetching only the octree nodes needed;
    /// - FlatGeobuf: `bbox`, finding the features with the spatial index;
    /// - GeoParquet: `rowGroups`, `bbox` (skipping row groups by their statistics).
    ///
    /// With `bbox`, the features are those that intersect it, as `ogr2ogr -spat` keeps them.
    pub fn read(&self, options: JsValue) -> js_sys::Promise {
        let me = RemoteDataset { state: self.state.clone() };
        wasm_bindgen_futures::future_to_promise(async move {
            let points = matches!(me.state.reader.borrow().as_ref(), Some(Reader::Points(_)));
            if points {
                let o = crate::pointcloud::ReadOptions::parse(options)?;
                let pc = me
                    .state
                    .run(|r| match r.as_mut() {
                        Some(Reader::Points(p)) => p.read(&o),
                        _ => unreachable!("checked"),
                    })
                    .await?;
                return Ok(crate::pointcloud::JsPointCloud::from_points(pc).into());
            }
            let a: ReadArgs = parse_options(&options)?;
            let (file, name) = (me.state.file.clone(), me.state.name.clone());
            let ds = me
                .state
                .run(|r| -> terrio::Result<Dataset> {
                    match r.as_mut() {
                        Some(Reader::Raster(r)) | Some(Reader::Tiles(_, Some(r))) => {
                            let w = Self::window(r, a.window, a.overview)?;
                            let o = WindowOptions { bands: a.bands.clone().unwrap_or_default(), overview: a.overview };
                            Ok(Dataset::from_raster(r.read_window(w, &o)?))
                        }
                        Some(Reader::Tiles(_, None)) => {
                            let mut o = terrio::VectorOpenOptions::default();
                            o.tiles.zoom_level = a.zoom;
                            o.layers = a.layers.clone().unwrap_or_default();
                            o.spatial_filter = a.bbox.map(|[a, b, c, d]| (a, b, c, d));
                            match TileArchive::from_sparse(file.clone())?.into_dataset(&name, &o)? {
                                terrio::Dataset::Vector(v) => Ok(Dataset::from_layers(spatial_filter(&v.layers, a.bbox)?, v.driver)),
                                terrio::Dataset::Raster(r) => Ok(Dataset::from_raster(r)),
                            }
                        }
                        Some(Reader::FlatGeobuf(_)) => {
                            let bbox = a.bbox.map(|[a, b, c, d]| (a, b, c, d));
                            let (_, layer) = terrio::vector::read_flatgeobuf_sparse(&file, &name, bbox)?;
                            Ok(Dataset::from_layers(spatial_filter(&[layer], a.bbox)?, VectorDriver::FlatGeobuf))
                        }
                        Some(Reader::Parquet(_)) => {
                            let bbox = a.bbox.map(|[a, b, c, d]| (a, b, c, d));
                            let layer = terrio::vector::read_geoparquet_sparse(&file, &name, a.row_groups.as_deref(), bbox)?;
                            Ok(Dataset::from_layers(spatial_filter(&[layer], a.bbox)?, VectorDriver::Parquet))
                        }
                        _ => Err(terrio::Error::InvalidArgument("not open".into())),
                    }
                })
                .await?;
            Ok(ds.into())
        })
    }

    /// PMTiles: a tile's bytes (decompressed), or `undefined` when the archive does not have it.
    /// `y` counts rows from the top, as in XYZ tile URLs.
    #[wasm_bindgen(js_name = getTile)]
    pub fn get_tile(&self, z: u8, x: u32, y: u32) -> js_sys::Promise {
        let me = RemoteDataset { state: self.state.clone() };
        wasm_bindgen_futures::future_to_promise(async move {
            let tile = me
                .state
                .run(|r| match r.as_mut() {
                    Some(Reader::Tiles(a, _)) => a.tile(z, x, y),
                    _ => Err(terrio::Error::InvalidArgument("not a tile archive".into())),
                })
                .await?;
            Ok(tile.map(|t| js_sys::Uint8Array::from(&t[..]).into()).unwrap_or(JsValue::UNDEFINED))
        })
    }
}
