//! Remote files read lazily with HTTP range requests through `fetch`: COGs by window and
//! overview, PMTiles tiles, COPC point clouds by bounds and level, FlatGeobuf by bounding box (with
//! its spatial index) and GeoParquet by row group; Zarr stores by window, their metadata documents,
//! shard indexes and chunks fetched as objects ([`terrio::sparse::SparseObjects`]).
//!
//! A read runs terrio's synchronous reader over the blocks fetched so far
//! ([`terrio::sparse::SparseFile`]); when it needs blocks not fetched yet, they are fetched (in
//! parallel requests, nearby ranges merged) and the read runs again, until it has all it needs.

use crate::{Dataset, ResampledArgs, crs_string, data_and_masks, js_err, js_error, parse_options, to_js, typed_array};
use serde::Deserialize;
use std::cell::RefCell;
use std::rc::Rc;
#[cfg(any(feature = "flatgeobuf", feature = "parquet"))]
use terrio::formats::VectorDriver;
#[cfg(feature = "pointcloud")]
use terrio::pointcloud::PointCloudReader;
use terrio::sparse::{SparseFile, SparseObjects};
#[cfg(feature = "tiles")]
use terrio::tiles::TileArchive;
#[cfg(feature = "flatgeobuf")]
use terrio::vector::FlatGeobufInfo;
#[cfg(feature = "parquet")]
use terrio::vector::RowGroupInfo;
use terrio::{LazyRaster, Window, WindowOptions};
use terrio_js_fetch::{Fetcher, MAX_PARALLEL, Remote, call, get, server_runtime};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// Options of [`open_url`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct OpenUrlOptions {
    /// The block size in bytes: reads fetch whole blocks.
    block_size: u64,
    /// The bytes kept in the block cache between reads.
    cache_size: u64,
    /// The format, when the URL's extension does not tell it: `GTiff`, `PMTiles`, `COPC`,
    /// `FlatGeobuf`, `Parquet` or `Zarr`.
    format: Option<String>,
    /// Zarr: the array to read, its path within the store (or group) at the URL.
    array: Option<String>,
    /// Opt in to 403 for anonymous version/existence probes; v2 .zattrs stays strict.
    allow_missing_metadata403: bool,
}

impl Default for OpenUrlOptions {
    fn default() -> Self {
        Self {
            block_size: terrio::sparse::DEFAULT_BLOCK_SIZE,
            cache_size: 256 << 20,
            format: None,
            array: None,
            allow_missing_metadata403: false,
        }
    }
}

/// What a remote file is read as.
enum Reader {
    Raster(LazyRaster),
    #[cfg(feature = "tiles")]
    Tiles(TileArchive, Option<LazyRaster>),
    #[cfg(feature = "pointcloud")]
    Points(Box<PointCloudReader>),
    #[cfg(feature = "flatgeobuf")]
    FlatGeobuf(FlatGeobufInfo),
    #[cfg(feature = "parquet")]
    Parquet(Vec<RowGroupInfo>),
}

impl Reader {
    /// The raster read, for rasters and PMTiles of images.
    fn raster(&mut self) -> Option<&mut LazyRaster> {
        match self {
            Reader::Raster(r) => Some(r),
            #[cfg(feature = "tiles")]
            Reader::Tiles(_, r) => r.as_mut(),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

struct State {
    remote: Remote,
    name: String,
    format: &'static str,
    reader: RefCell<Option<Reader>>,
}

impl State {
    /// Runs a read until it no longer misses blocks, fetching them in between ([`Remote::run`]);
    /// opening while there is no reader yet.
    async fn run<T>(&self, mut read: impl FnMut(&mut Option<Reader>) -> terrio::Result<T>) -> Result<T, JsError> {
        self.remote.run(|| self.reader.borrow().is_none(), || read(&mut self.reader.borrow_mut())).await?.map_err(js_err)
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
            "zarr" => Ok("Zarr"),
            _ => Err(JsError::new(&format!("format '{f}' is not read remotely (GTiff, PMTiles, COPC, FlatGeobuf, Parquet, Zarr)"))),
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

/// The cargo feature that reads a remote format, for those not always built.
fn feature_of(format: &str) -> Option<&'static str> {
    match format {
        "PMTiles" => Some("tiles"),
        "COPC" => Some("pointcloud"),
        "FlatGeobuf" => Some("flatgeobuf"),
        "Parquet" => Some("parquet"),
        "Zarr" => Some("zarr"),
        _ => None,
    }
}

/// Fails when this build leaves out the format.
fn check_built(format: &str) -> Result<(), JsError> {
    match feature_of(format) {
        Some(f) if !crate::FEATURES.iter().any(|&(name, on)| name == f && on) => Err(js_err(crate::not_built(f))),
        _ => Ok(()),
    }
}

/// Opens a remote file for reading lazily with HTTP range requests (`fetch`): a COG or other
/// GeoTIFF, a PMTiles archive, a COPC point cloud, a FlatGeobuf file or a GeoParquet file. Only
/// its header is read now, in a request or two; reads fetch only the bytes they need.
///
/// A Zarr store (v2 or v3, sharded or not: a URL ending in `.zarr`, or `format: "Zarr"`) is read
/// as a raster as well, the array `array` of the store or group at the URL (its path within it),
/// by window: opening fetches its metadata documents, and a window the shard indexes and chunks
/// it needs, nearby inner chunks of a shard in one request.
///
/// Options: `headers` (sent with every request, for authorization), `fetch` (the function to
/// call instead of `globalThis.fetch`), `blockSize` (64 KiB: reads fetch whole blocks),
/// `cacheSize` (256 MB kept between reads, not during one: a read holds all the bytes it needs),
/// `format` (when the URL does not tell it), `array` (Zarr), `allowMissingMetadata403`
/// (false: opt in only for anonymous stores that report absent metadata as forbidden;
/// v2 .zattrs documents, chunks and shards always fail; zarr.json probes remain ambiguous).
///
/// Servers must answer range requests; in browsers, cross-origin servers must allow the `Range`
/// header and expose `Content-Range` (CORS).
#[wasm_bindgen(js_name = openUrl)]
pub async fn open_url(url: String, options: JsValue) -> Result<RemoteDataset, JsError> {
    let (mut fetcher, rest) = Fetcher::from_options(&url, &options)?;
    let o: OpenUrlOptions = parse_options(&rest)?;
    fetcher.allow_missing_metadata403 = o.allow_missing_metadata403;
    // A format this build leaves out fails before any request, when the URL or the options tell it.
    let path = url.split(['?', '#']).next().unwrap_or(&url);
    if let Ok(format) = detect(path, &[], o.format.as_deref()) {
        check_built(format)?;
    }
    if o.format.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("zarr"))
        || (o.format.is_none() && path.trim_end_matches('/').to_ascii_lowercase().ends_with(".zarr"))
    {
        check_built("Zarr")?;
        #[cfg(feature = "zarr")]
        return open_zarr(path.to_string(), fetcher, o).await;
    }
    // The first block: the file's length, and its first bytes.
    let (file, first) = fetcher.open_file(o.block_size).await?;
    let path = url.split(['?', '#']).next().unwrap_or(&url);
    let name = path.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("remote").to_string();
    let format = detect(&name, &first, o.format.as_deref())?;
    check_built(format)?;
    let state = Rc::new(State { remote: Remote::new(file, None, fetcher, o.cache_size), name, format, reader: RefCell::new(None) });
    let (file, name) = (state.remote.file.clone(), state.name.clone());
    state
        .run(|r| {
            *r = Some(match format {
                "GTiff" => Reader::Raster(
                    LazyRaster::from_sparse(file.clone())?.ok_or_else(|| terrio::Error::InvalidData(format!("'{name}' is not a TIFF")))?,
                ),
                #[cfg(feature = "tiles")]
                "PMTiles" => {
                    let archive = TileArchive::from_sparse(file.clone())?;
                    let raster = if archive.is_vector() {
                        None
                    } else {
                        Some(LazyRaster::from_tile_archive(TileArchive::from_sparse(file.clone())?)?)
                    };
                    Reader::Tiles(archive, raster)
                }
                #[cfg(feature = "pointcloud")]
                "COPC" => Reader::Points(Box::new(terrio::pointcloud::open_points_sparse(file.clone(), &name)?)),
                #[cfg(feature = "flatgeobuf")]
                "FlatGeobuf" => {
                    let (info, _) = terrio::vector::read_flatgeobuf_header_sparse(&file, &name)?;
                    Reader::FlatGeobuf(info)
                }
                #[cfg(feature = "parquet")]
                "Parquet" => Reader::Parquet(terrio::vector::geoparquet_row_groups_sparse(&file)?),
                // Checked before: see `check_built`.
                other => return Err(crate::not_built(feature_of(other).unwrap_or(other))),
            });
            Ok(())
        })
        .await?;
    Ok(RemoteDataset { state })
}

/// Opens the array `o.array` of the Zarr store (or group) at `url`, its objects fetched as reads
/// need them: the metadata now, in a few rounds of requests (the root's documents, the array's,
/// the coordinates of its band dimensions).
#[cfg(feature = "zarr")]
async fn open_zarr(url: String, fetcher: Fetcher, o: OpenUrlOptions) -> Result<RemoteDataset, JsError> {
    let objects = std::sync::Arc::new(SparseObjects::new());
    // Shard reads split into requests that fill those in flight: worth it where they are not
    // queued (Node.js, Deno, Bun), not in browsers (see `SparseObjects::set_parallel_requests`).
    if server_runtime() {
        objects.set_parallel_requests(MAX_PARALLEL);
    }
    let state = Rc::new(State {
        remote: Remote::new(SparseFile::new(0), Some(objects.clone()), fetcher, o.cache_size),
        name: url.clone(),
        format: "Zarr",
        reader: RefCell::new(None),
    });
    let key = o.array.unwrap_or_default();
    state
        .run(|r| {
            *r = Some(Reader::Raster(LazyRaster::from_zarr_objects(objects.clone(), &url, &key)?));
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
    /// GeoParquet: the fields to read (all by default), matched without case; a struct column's
    /// name keeps its `name.member` fields. Only their column chunks and the geometry's are
    /// fetched.
    columns: Option<Vec<String>>,
    /// Vector tiles: the zoom level (the highest by default).
    zoom: Option<u8>,
    /// Vector tiles: the layers.
    layers: Option<Vec<String>>,
}

/// Keeps the features that intersect a rectangle, as `ogr2ogr -spat` does; without one, the
/// layers as they are. The features are moved, not copied.
#[cfg_attr(not(any(feature = "tiles", feature = "flatgeobuf", feature = "parquet")), allow(dead_code))]
fn spatial_filter(layers: Vec<terrio::Layer>, bbox: Option<[f64; 4]>) -> terrio::Result<Vec<terrio::Layer>> {
    match bbox {
        None => Ok(layers),
        Some([a, b, c, d]) => {
            let o = terrio::ops::vector_filter::VectorTranslateOptions { spatial_filter: Some((a, b, c, d)), ..Default::default() };
            terrio::ops::vector_filter::translate_owned(layers, &o)
        }
    }
}

impl RemoteDataset {
    fn with_raster<T>(&self, f: impl FnOnce(&mut LazyRaster) -> T) -> Result<T, JsError> {
        match self.state.reader.borrow_mut().as_mut().and_then(Reader::raster) {
            Some(r) => Ok(f(r)),
            None => Err(JsError::new(&format!("'{}' is not a raster", self.state.name))),
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
            Some(Reader::Raster(_)) => "raster",
            #[cfg(feature = "tiles")]
            Some(Reader::Tiles(_, Some(_))) => "raster",
            #[cfg(feature = "tiles")]
            Some(Reader::Tiles(_, None)) => "tiles",
            #[cfg(feature = "pointcloud")]
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
        self.state.remote.fetcher.url.clone()
    }

    /// The file's size in bytes.
    #[wasm_bindgen(getter)]
    pub fn size(&self) -> f64 {
        self.state.remote.file.len() as f64
    }

    /// The requests sent and the bytes received so far: `{ requests, bytes }`.
    pub fn stats(&self) -> Result<JsValue, JsError> {
        to_js(&serde_json::json!({ "requests": self.state.remote.fetcher.requests.get(), "bytes": self.state.remote.fetcher.bytes.get() }))
    }

    /// What the header says: for rasters, the summary `terrio info --json` prints; for tiles, the
    /// zoom levels, bounds, tile format and metadata; for point clouds, the LAS header; for
    /// FlatGeobuf, the feature count, extent and index; for GeoParquet, the row groups.
    pub fn info(&self) -> Result<JsValue, JsError> {
        let mut reader = self.state.reader.borrow_mut();
        if let Some(r) = reader.as_mut().and_then(Reader::raster) {
            let info = terrio::info::lazy_raster_info(r, &Default::default()).map_err(js_err)?;
            return to_js(&terrio::info::Info::Raster(Box::new(info)));
        }
        match reader.as_mut() {
            #[cfg(feature = "tiles")]
            Some(Reader::Tiles(a, _)) => to_js(&serde_json::json!({
                "format": a.format().name(),
                "zoomRange": a.zoom_range(),
                "bounds": a.bounds(),
                "metadata": a.metadata().into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect::<serde_json::Map<_, _>>(),
            })),
            #[cfg(feature = "pointcloud")]
            Some(Reader::Points(p)) => to_js(p.header()),
            #[cfg(feature = "flatgeobuf")]
            Some(Reader::FlatGeobuf(i)) => to_js(i),
            #[cfg(feature = "parquet")]
            Some(Reader::Parquet(g)) => to_js(&serde_json::json!({ "rowGroups": g })),
            _ => Err(JsError::new("not open")),
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
        self.with_raster(|r| r.data_type().as_str().to_string())
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
            Reader::Raster(r) => crs_string(&r.crs()),
            #[cfg(feature = "tiles")]
            Reader::Tiles(_, Some(r)) => crs_string(&r.crs()),
            #[cfg(feature = "tiles")]
            Reader::Tiles(_, None) => Some("EPSG:3857".into()),
            #[cfg(feature = "pointcloud")]
            Reader::Points(p) => crs_string(p.crs()),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// Rasters: reads a band (from 1) as a typed array, fetching only the tiles of the window
    /// (`[colOff, rowOff, width, height]`, in the overview's pixels with `overview`).
    #[wasm_bindgen(js_name = readBand)]
    pub fn read_band(&self, band: usize, window: Option<Vec<u32>>, overview: Option<usize>) -> js_sys::Promise {
        let me = RemoteDataset { state: self.state.clone() };
        wasm_bindgen_futures::future_to_promise(async move {
            let window = match window.as_deref() {
                None => None,
                Some(&[x, y, w, h]) => Some([x as usize, y as usize, w as usize, h as usize]),
                Some(_) => return Err(JsError::new("window is [col_off, row_off, width, height]").into()),
            };
            let opts = WindowOptions { bands: vec![band], overview };
            let ds = me
                .state
                .run(|r| match r.as_mut().and_then(Reader::raster) {
                    Some(r) => {
                        let w = Self::window(r, window, overview)?;
                        r.read_window(w, &opts)
                    }
                    None => Err(terrio::Error::InvalidArgument("not a raster".into())),
                })
                .await?;
            Ok(typed_array(ds.raster.data))
        })
    }

    /// Rasters: reads a window into an output of another size, as `Dataset.readResampled` does
    /// (same options), fetching only the tiles of the overview `RasterIO` would choose.
    #[wasm_bindgen(js_name = readResampled)]
    pub fn read_resampled(&self, options: JsValue) -> js_sys::Promise {
        let me = RemoteDataset { state: self.state.clone() };
        wasm_bindgen_futures::future_to_promise(async move {
            let o = parse_options::<ResampledArgs>(&options)?.options().map_err(js_err)?;
            let ds = me
                .state
                .run(|r| match r.as_mut().and_then(Reader::raster) {
                    Some(r) => r.read_resampled(&o),
                    None => Err(terrio::Error::InvalidArgument("not a raster".into())),
                })
                .await?;
            Ok(typed_array(ds.raster.data))
        })
    }

    /// Rasters: reads the mask bands of the bands `readResampled` reads, as
    /// `Dataset.readResampledMasks` does (same options): a `Uint8Array`.
    #[wasm_bindgen(js_name = readResampledMasks)]
    pub fn read_resampled_masks(&self, options: JsValue) -> js_sys::Promise {
        let me = RemoteDataset { state: self.state.clone() };
        wasm_bindgen_futures::future_to_promise(async move {
            let o = parse_options::<ResampledArgs>(&options)?.options().map_err(js_err)?;
            let masks = me
                .state
                .run(|r| match r.as_mut().and_then(Reader::raster) {
                    Some(r) => r.read_resampled_masks(&o),
                    None => Err(terrio::Error::InvalidArgument("not a raster".into())),
                })
                .await?;
            Ok(js_sys::Uint8Array::from(&masks[..]).into())
        })
    }

    /// Rasters: reads pixels and masks at once, as `Dataset.readResampledWithMasks` does (same
    /// options): `{ data, masks }`.
    #[wasm_bindgen(js_name = readResampledWithMasks)]
    pub fn read_resampled_with_masks(&self, options: JsValue) -> js_sys::Promise {
        let me = RemoteDataset { state: self.state.clone() };
        wasm_bindgen_futures::future_to_promise(async move {
            let o = parse_options::<ResampledArgs>(&options)?.options().map_err(js_err)?;
            let (ds, masks) = me
                .state
                .run(|r| match r.as_mut().and_then(Reader::raster) {
                    Some(r) => r.read_resampled_with_masks(&o),
                    None => Err(terrio::Error::InvalidArgument("not a raster".into())),
                })
                .await?;
            Ok(data_and_masks(ds.raster.data, &masks))
        })
    }

    /// Reads into memory, as a `Dataset` (rasters, vector data) or a `PointCloud`:
    ///
    /// - rasters: `window`, `overview`, `bands`;
    /// - vector tiles: `zoom` (the highest by default), `layers`, `bbox` (EPSG:3857);
    /// - COPC: the options of `readPoints` (`bounds`, `zRange`, `classes`, `returns`,
    ///   `maxLevel`, `resolution`, `maxPoints`), fetching only the octree nodes needed;
    /// - FlatGeobuf: `bbox`, finding the features with the spatial index;
    /// - GeoParquet: `rowGroups`, `bbox` (skipping row groups by their statistics), `columns`
    ///   (fetching only the column chunks of those fields and the geometry).
    ///
    /// With `bbox`, the features are those that intersect it, as `ogr2ogr -spat` keeps them.
    pub fn read(&self, options: JsValue) -> js_sys::Promise {
        let me = RemoteDataset { state: self.state.clone() };
        wasm_bindgen_futures::future_to_promise(async move {
            #[cfg(feature = "pointcloud")]
            if matches!(me.state.reader.borrow().as_ref(), Some(Reader::Points(_))) {
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
            #[allow(unused_variables)]
            let (file, name) = (me.state.remote.file.clone(), me.state.name.clone());
            let ds = me
                .state
                .run(|r| -> terrio::Result<Dataset> {
                    if let Some(r) = r.as_mut().and_then(Reader::raster) {
                        let w = Self::window(r, a.window, a.overview)?;
                        let o = WindowOptions { bands: a.bands.clone().unwrap_or_default(), overview: a.overview };
                        return Ok(Dataset::from_raster(r.read_window(w, &o)?));
                    }
                    match r.as_mut() {
                        #[cfg(feature = "tiles")]
                        Some(Reader::Tiles(_, None)) => {
                            let mut o = terrio::VectorOpenOptions::default();
                            o.tiles.zoom_level = a.zoom;
                            o.layers = a.layers.clone().unwrap_or_default();
                            o.spatial_filter = a.bbox.map(|[a, b, c, d]| (a, b, c, d));
                            match TileArchive::from_sparse(file.clone())?.into_dataset(&name, &o)? {
                                terrio::Dataset::Vector(v) => Ok(Dataset::from_layers(spatial_filter(v.layers, a.bbox)?, v.driver)),
                                terrio::Dataset::Raster(r) => Ok(Dataset::from_raster(r)),
                            }
                        }
                        #[cfg(feature = "flatgeobuf")]
                        Some(Reader::FlatGeobuf(_)) => {
                            let bbox = a.bbox.map(|[a, b, c, d]| (a, b, c, d));
                            let (_, layer) = terrio::vector::read_flatgeobuf_sparse(&file, &name, bbox)?;
                            Ok(Dataset::from_layers(spatial_filter(vec![layer], a.bbox)?, VectorDriver::FlatGeobuf))
                        }
                        #[cfg(feature = "parquet")]
                        Some(Reader::Parquet(_)) => {
                            let bbox = a.bbox.map(|[a, b, c, d]| (a, b, c, d));
                            let layer =
                                terrio::vector::read_geoparquet_sparse(&file, &name, a.row_groups.as_deref(), bbox, a.columns.as_deref())?;
                            Ok(Dataset::from_layers(spatial_filter(vec![layer], a.bbox)?, VectorDriver::Parquet))
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
    #[cfg_attr(not(feature = "tiles"), allow(unused_variables))]
    pub fn get_tile(&self, z: u8, x: u32, y: u32) -> js_sys::Promise {
        let me = RemoteDataset { state: self.state.clone() };
        wasm_bindgen_futures::future_to_promise(async move {
            let tile = me
                .state
                .run(|r| -> terrio::Result<Option<Vec<u8>>> {
                    match r.as_mut() {
                        #[cfg(feature = "tiles")]
                        Some(Reader::Tiles(a, _)) => a.tile(z, x, y),
                        _ => Err(terrio::Error::InvalidArgument("not a tile archive".into())),
                    }
                })
                .await?;
            Ok(tile.map(|t| js_sys::Uint8Array::from(&t[..]).into()).unwrap_or(JsValue::UNDEFINED))
        })
    }
}

/// OGC API Features request options (the shared Rust reader performs discovery and paging).
#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct OapifOptions {
    layers: Vec<String>,
    bbox: Option<[f64; 4]>,
    page_size: usize,
    max_features: Option<usize>,
    skip_features: usize,
    datetime: Option<String>,
    crs: Option<String>,
    preferred_crs: Option<String>,
    server_feature_axis_order: Option<String>,
    ignore_schema: bool,
    max_vector_bytes: usize,
    max_pages: usize,
}
impl Default for OapifOptions {
    fn default() -> Self {
        let o = terrio::OapifOpenOptions::default();
        Self {
            layers: Vec::new(),
            bbox: None,
            page_size: o.page_size,
            max_features: None,
            skip_features: 0,
            datetime: None,
            crs: None,
            preferred_crs: None,
            server_feature_axis_order: None,
            ignore_schema: false,
            max_vector_bytes: o.max_vector_bytes,
            max_pages: o.max_pages,
        }
    }
}

/// Reads OGC API Features over fetch, following the shared Rust reader's bounded requests.
/// Options: layers, bbox, pageSize, maxFeatures, skipFeatures, datetime, crs,
/// preferredCrs, serverFeatureAxisOrder, ignoreSchema, maxVectorBytes, maxPages,
/// and optional fetch/headers. Requires CORS in browsers; fetch redirects are refused.
#[wasm_bindgen(js_name = openOapif)]
pub async fn open_oapif(url: &str, options: JsValue) -> Result<Dataset, JsError> {
    let (rest, custom) = crate::ops::split_options(&options, &["fetch", "headers"])?;
    let a: OapifOptions = parse_options(&rest)?;
    let mut o = terrio::VectorOpenOptions {
        layers: a.layers,
        spatial_filter: a.bbox.map(|b| (b[0], b[1], b[2], b[3])),
        skip_features: a.skip_features,
        max_features: a.max_features,
        ..Default::default()
    };
    o.oapif.page_size = a.page_size;
    o.oapif.datetime = a.datetime;
    o.oapif.crs = a.crs;
    o.oapif.preferred_crs = a.preferred_crs;
    o.oapif.ignore_schema = a.ignore_schema;
    o.oapif.max_vector_bytes = a.max_vector_bytes;
    o.oapif.max_pages = a.max_pages;
    if let Some(order) = a.server_feature_axis_order {
        o.oapif.set("SERVER_FEATURE_AXIS_ORDER", &order).map_err(js_err)?;
    }
    let mut reader = terrio::oapif::Reader::new(url, &o).map_err(js_err)?;
    let fetch: js_sys::Function = custom[0]
        .clone()
        .unwrap_or_else(|| get(&js_sys::global(), "fetch"))
        .dyn_into()
        .map_err(|_| JsError::new("OAPIF requires fetch"))?;
    let supplied = custom[1].clone().unwrap_or(JsValue::UNDEFINED);
    while let Some(request) = reader.next_request().cloned() {
        let headers = js_sys::Object::new();
        if supplied.is_object() {
            let iterable = js_sys::Reflect::has(&supplied, &js_sys::Symbol::iterator()).unwrap_or(false);
            let given =
                if iterable { js_sys::Object::from_entries(&supplied).map_err(js_error)? } else { js_sys::Object::from(supplied.clone()) };
            js_sys::Object::assign(&headers, &given);
        }
        js_sys::Reflect::set(&headers, &"Accept".into(), &request.accept.into()).map_err(js_error)?;
        let init = js_sys::Object::new();
        js_sys::Reflect::set(&init, &"headers".into(), &headers).map_err(js_error)?;
        // Browser fetch can conceal redirect destinations. Do not forward caller headers
        // through redirects; callers can open the canonical service URL explicitly.
        js_sys::Reflect::set(&init, &"redirect".into(), &"error".into()).map_err(js_error)?;
        let promise = fetch
            .call2(&JsValue::UNDEFINED, &request.url.into(), &init)
            .map_err(js_error)?
            .dyn_into::<js_sys::Promise>()
            .map_err(|_| JsError::new("fetch did not return a promise"))?;
        let response = JsFuture::from(promise).await.map_err(js_error)?;
        let status = get(&response, "status").as_f64().unwrap_or(0.) as u16;
        if !(200..300).contains(&status) {
            oapif_cancel(&get(&response, "body"));
            return Err(JsError::new(&format!("OAPIF HTTP status {status}")));
        }
        let body = oapif_body(&response, request.byte_limit).await?;
        reader.accept_response(&body).map_err(js_err)?;
    }
    let result = reader.finish().map_err(js_err)?;
    Ok(Dataset::from_layers(result.layers, terrio::formats::VectorDriver::OAPIF))
}

async fn oapif_body(response: &JsValue, limit: usize) -> Result<Vec<u8>, JsError> {
    let stream = get(response, "body");
    if stream.is_null() || stream.is_undefined() {
        return Ok(Vec::new());
    }
    let reader = call(&stream, "getReader", &[])?;
    let mut data = Vec::new();
    loop {
        let promise = call(&reader, "read", &[])?.dyn_into::<js_sys::Promise>().map_err(|_| JsError::new("invalid fetch body reader"))?;
        let chunk = JsFuture::from(promise).await.map_err(js_error)?;
        if get(&chunk, "done").as_bool().unwrap_or(false) {
            break;
        }
        let bytes = js_sys::Uint8Array::new(&get(&chunk, "value"));
        let size = bytes.length() as usize;
        if size > limit.saturating_sub(data.len()) {
            oapif_cancel(&reader);
            return Err(JsError::new("OAPIF response exceeds its byte budget"));
        }
        data.try_reserve(size).map_err(|_| JsError::new("cannot allocate OAPIF response"))?;
        let start = data.len();
        data.resize(start + size, 0);
        bytes.copy_to(&mut data[start..]);
    }
    Ok(data)
}

// Cancellation of a tee branch may wait for an unread sibling. Do not delay errors.
fn oapif_cancel(body: &JsValue) {
    if let Ok(promise) = call(body, "cancel", &[]).and_then(|p| p.dyn_into::<js_sys::Promise>().map_err(js_error)) {
        wasm_bindgen_futures::spawn_local(async move {
            let _ = JsFuture::from(promise).await;
        });
    }
}
