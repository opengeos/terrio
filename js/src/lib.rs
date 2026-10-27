//! WebAssembly bindings for terrio, for browsers and Node.js.
//!
//! Datasets are opened from bytes, with a file name telling the format, and written back to bytes:
//!
//! ```js
//! import { open, transformPoints } from "terrio-js";
//!
//! const ds = open(new Uint8Array(await (await fetch("dem.tif")).arrayBuffer()), "dem.tif");
//! ds.info();                 // the same summary as `terrio info --json`
//! const band = ds.readBand(1); // a Float32Array, Uint8Array... of the band's own type
//! const fgb = open(geojsonBytes, "roads.geojson").toBytes("roads.fgb");
//! const points = readPoints(lazBytes, "lidar.laz", { classes: [2] }); // LAS, LAZ, COPC
//! ```

mod multidim;
mod ops;
mod pixel;
#[cfg(feature = "pointcloud")]
mod pointcloud;
mod query;
mod remote;
#[cfg(feature = "threads")]
mod threads;
mod vector_dataset;

use serde::Deserialize;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use terrio::crs::{SpatialRef, TransformOptions, Transformer};
use terrio::formats::{RasterDriver, VectorDriver};
use terrio::ops::vector_filter::VectorTranslateOptions;
use terrio::{DataType, Layer, LazyRaster, RasterDataset, RasterWriteOptions, VectorWriteOptions, Window, WindowOptions};
use wasm_bindgen::prelude::*;
use wbraster::raster::RasterData;

/// The allocator JPEG 2000 needs in WebAssembly: see [`terrio::NullSafeAllocator`].
#[global_allocator]
static ALLOCATOR: terrio::NullSafeAllocator = terrio::NullSafeAllocator;

/// Reports Rust panics on the JavaScript console, with their message.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// The name of this build of the module: `terrio-js`, every format, `terrio-js/threads`, every
/// format with a thread pool, `terrio-js/memory64`, every format with 64-bit memory, or
/// `terrio-js/core`.
const BUILD: &str = if cfg!(feature = "threads") {
    "terrio-js/threads"
} else if cfg!(target_arch = "wasm64") {
    "terrio-js/memory64"
} else if cfg!(feature = "full") {
    "terrio-js"
} else {
    "terrio-js/core"
};

/// The cargo features of this build, the formats it reads and writes besides the ones always built.
const FEATURES: &[(&str, bool)] = &[
    ("avif", cfg!(feature = "avif")),
    ("cad", cfg!(feature = "cad")),
    ("flatgeobuf", cfg!(feature = "flatgeobuf")),
    ("grib", cfg!(feature = "grib")),
    ("jp2", cfg!(feature = "jp2")),
    ("lerc", cfg!(feature = "lerc")),
    ("lzma", cfg!(feature = "lzma")),
    ("netcdf-hdf5", cfg!(feature = "netcdf-hdf5")),
    ("osm", cfg!(feature = "osm")),
    ("parquet", cfg!(feature = "parquet")),
    ("pointcloud", cfg!(feature = "pointcloud")),
    ("sqlite", cfg!(feature = "sqlite")),
    ("tiles", cfg!(feature = "tiles")),
    ("zarr", cfg!(feature = "zarr")),
];

/// Converts a terrio error into a JavaScript `Error`. Formats this build leaves out say which
/// module has them.
fn js_err(e: terrio::Error) -> JsError {
    match e {
        // terrio's (and `not_built`'s) error for a driver the build leaves out.
        terrio::Error::Unsupported(msg) if !cfg!(feature = "full") && msg.starts_with(NOT_BUILT) => {
            JsError::new(&format!("{msg}: {BUILD} leaves it out, the full module (terrio-js) has it"))
        }
        e => JsError::new(&e.to_string()),
    }
}

/// How terrio's errors for a driver the build leaves out begin.
const NOT_BUILT: &str = "built without the `";

/// The error of a format this build leaves out, `feature` being the cargo feature that has it.
#[allow(dead_code)]
pub(crate) fn not_built(feature: &str) -> terrio::Error {
    terrio::Error::Unsupported(format!("{NOT_BUILT}{feature}` feature"))
}

/// A JSON value as a JavaScript value.
fn to_js<T: serde::Serialize>(value: &T) -> Result<JsValue, JsError> {
    let text = serde_json::to_string(value).map_err(|e| JsError::new(&e.to_string()))?;
    js_sys::JSON::parse(&text).map_err(|_| JsError::new("invalid JSON"))
}

/// A CRS as `EPSG:xxxx` (or `ESRI:xxxx`) when its code is known, otherwise its WKT.
fn crs_string(crs: &SpatialRef) -> Option<String> {
    crs.authority_code().or_else(|| crs.to_wkt())
}

/// Parses an options object (`undefined` or `null` for the defaults).
pub(crate) fn parse_options<T: serde::de::DeserializeOwned + Default>(options: &JsValue) -> Result<T, JsError> {
    if options.is_undefined() || options.is_null() {
        return Ok(T::default());
    }
    let text: String = js_sys::JSON::stringify(options).map_err(|_| JsError::new("invalid options"))?.into();
    serde_json::from_str(&text).map_err(|e| JsError::new(&format!("options: {e}")))
}

/// Pixels as a typed array of their own type.
fn typed_array(data: RasterData) -> JsValue {
    match data {
        RasterData::U8(v) => js_sys::Uint8Array::from(&v[..]).into(),
        RasterData::I8(v) => js_sys::Int8Array::from(&v[..]).into(),
        RasterData::U16(v) => js_sys::Uint16Array::from(&v[..]).into(),
        RasterData::I16(v) => js_sys::Int16Array::from(&v[..]).into(),
        RasterData::U32(v) => js_sys::Uint32Array::from(&v[..]).into(),
        RasterData::I32(v) => js_sys::Int32Array::from(&v[..]).into(),
        RasterData::U64(v) => js_sys::BigUint64Array::from(&v[..]).into(),
        RasterData::I64(v) => js_sys::BigInt64Array::from(&v[..]).into(),
        RasterData::F32(v) => js_sys::Float32Array::from(&v[..]).into(),
        RasterData::F64(v) => js_sys::Float64Array::from(&v[..]).into(),
    }
}

/// Pixels from a typed array.
fn raster_data(array: &JsValue) -> Result<RasterData, JsError> {
    macro_rules! try_types {
        ($($t:ident => $v:ident),*) => {
            $(if let Some(a) = array.dyn_ref::<js_sys::$t>() {
                return Ok(RasterData::$v(a.to_vec()));
            })*
        };
    }
    try_types!(
        Uint8Array => U8, Uint8ClampedArray => U8, Int8Array => I8, Uint16Array => U16, Int16Array => I16,
        Uint32Array => U32, Int32Array => I32, BigUint64Array => U64, BigInt64Array => I64,
        Float32Array => F32, Float64Array => F64
    );
    Err(JsError::new("expected a typed array (Uint8Array, Int16Array, Float32Array...)"))
}

/// The JavaScript name of a pixel type's typed array element, as in `Float32Array`.
fn dtype_name(t: DataType) -> &'static str {
    match t {
        DataType::U8 => "uint8",
        DataType::I8 => "int8",
        DataType::U16 => "uint16",
        DataType::I16 => "int16",
        DataType::U32 => "uint32",
        DataType::I32 => "int32",
        DataType::U64 => "uint64",
        DataType::I64 => "int64",
        DataType::F32 => "float32",
        DataType::F64 => "float64",
    }
}

/// Options for [`Dataset::to_bytes`], as a JavaScript object.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct WriteOptions {
    allow_ballpark: Option<bool>,
    only_best: bool,
    /// Take the heights of 2D CRSs as ellipsoidal heights, as GDAL does after `PromoteTo3D`.
    promote_to_3d: bool,
    /// The output format's short name, overriding the file name's extension.
    format: Option<String>,
    /// Reproject into this CRS.
    t_srs: Option<String>,
    /// Vector layers to write, by name.
    layers: Option<Vec<String>>,
    /// Keep the features matching this OGR SQL expression.
    r#where: Option<String>,
    /// Keep only these fields.
    select: Option<Vec<String>>,
    /// Keep at most this many features.
    limit: Option<usize>,
    /// A raster window `[col_off, row_off, width, height]`.
    srcwin: Option<[usize; 4]>,
    /// A raster overview (0 is the largest).
    overview: Option<usize>,
    /// The bands to write, numbered from 1.
    bands: Option<Vec<usize>>,
    /// GeoTIFF/COG compression: NONE, LZW, DEFLATE, PACKBITS or ZSTD.
    compress: Option<String>,
    /// GeoTIFF/COG tile size.
    tile_size: Option<u32>,
    /// Overview decimation factors to build when writing GeoTIFFs.
    overviews: Vec<usize>,
    /// Overview resampling: nearest, average, min, max or mode.
    overview_resampling: Option<String>,
    /// PMTiles, MBTiles: the lowest zoom level written.
    minzoom: Option<u8>,
    /// PMTiles, MBTiles: the highest zoom level written.
    maxzoom: Option<u8>,
    /// Raster creation options, as GDAL's (`{ FORMAT: "NC4", MAX_Z_ERROR: "0.01" }`).
    creation_options: Option<std::collections::BTreeMap<String, serde_json::Value>>,
}

#[allow(clippy::large_enum_variant)]
enum Inner {
    Raster(LazyRaster),
    Vector(Vec<Layer>, VectorDriver),
}

thread_local! {
    /// The datasets alive, by handle: operations given datasets as JavaScript values find them
    /// here, without taking them from JavaScript.
    static DATASETS: RefCell<HashMap<u32, Weak<RefCell<Inner>>>> = RefCell::new(HashMap::new());
    static NEXT_HANDLE: Cell<u32> = const { Cell::new(1) };
}

/// A raster or vector dataset opened from bytes.
#[wasm_bindgen]
pub struct Dataset {
    handle: u32,
    inner: Rc<RefCell<Inner>>,
}

impl Drop for Dataset {
    fn drop(&mut self) {
        DATASETS.with(|d| d.borrow_mut().remove(&self.handle));
    }
}

impl Dataset {
    fn new(inner: Inner) -> Self {
        let inner = Rc::new(RefCell::new(inner));
        let handle = NEXT_HANDLE.with(|n| {
            let h = n.get();
            n.set(h.wrapping_add(1).max(1));
            h
        });
        DATASETS.with(|d| d.borrow_mut().insert(handle, Rc::downgrade(&inner)));
        Self { handle, inner }
    }

    /// A raster dataset in memory.
    pub(crate) fn from_raster(ds: RasterDataset) -> Self {
        Self::new(Inner::Raster(LazyRaster::from(ds)))
    }

    /// A raster read by window.
    pub(crate) fn from_lazy(r: LazyRaster) -> Self {
        Self::new(Inner::Raster(r))
    }

    /// A vector dataset.
    pub(crate) fn from_layers(layers: Vec<Layer>, driver: VectorDriver) -> Self {
        Self::new(Inner::Vector(layers, driver))
    }

    /// The dataset a JavaScript value is, when it is one.
    pub(crate) fn lookup(value: &JsValue) -> Option<Rc<RefCell<Inner>>> {
        if !value.is_object() {
            return None;
        }
        let handle = js_sys::Reflect::get(value, &JsValue::from_str("__handle")).ok()?.as_f64()? as u32;
        DATASETS.with(|d| d.borrow().get(&handle).and_then(Weak::upgrade))
    }
}

/// Opens serialized VRT XML with a map of source file names to Uint8Array bytes.
/// Source GeoTIFFs and VRT pixel windows are read lazily, including in WebAssembly.
#[wasm_bindgen(js_name = openVrt)]
pub fn open_vrt(xml: &str, sources: JsValue) -> Result<Dataset, JsError> {
    let description = terrio::ops::vrt::VirtualRaster::from_xml(xml).map_err(js_err)?;
    if !sources.is_object() || sources.is_null() {
        return Err(JsError::new("sources must map filenames to Uint8Array bytes"));
    }
    let mut resolved = std::collections::HashMap::new();
    let object = js_sys::Object::from(sources);
    for name in js_sys::Object::keys(&object).iter() {
        let key = name.as_string().ok_or_else(|| JsError::new("invalid source name"))?;
        let bytes = js_sys::Reflect::get(&object, &name)
            .map_err(|_| JsError::new("cannot read VRT source map"))?
            .dyn_into::<js_sys::Uint8Array>()
            .map_err(|_| JsError::new("source values must be Uint8Array bytes"))?;
        let reader = terrio::memory::open_raster_lazy_bytes(bytes.to_vec(), &key).map_err(js_err)?;
        resolved.insert(key, reader);
    }
    Ok(Dataset::from_lazy(description.into_lazy(std::path::Path::new(""), resolved).map_err(js_err)?))
}

/// Opens a dataset from its bytes. `filename`'s extension tells the format (`dem.tif`,
/// `roads.fgb`, `parcels.zip`); its stem names vector layers.
///
/// GeoTIFFs and the image tiles of PMTiles and MBTiles archives are decoded one window at a
/// time, as they are read, and GRIB files one band (message) at a time; vector tiles are read as
/// layers.
#[wasm_bindgen]
pub fn open(bytes: Vec<u8>, filename: &str) -> Result<Dataset, JsError> {
    let is_tiff = matches!(bytes.get(..4), Some(b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+"));
    // GRIB: "GRIB" and an edition of 1 or 2 within the first bytes.
    let head = &bytes[..bytes.len().min(316)];
    let is_grib =
        cfg!(feature = "grib") && (0..head.len().saturating_sub(7)).any(|i| &head[i..i + 4] == b"GRIB" && matches!(head[i + 7], 1 | 2));
    if is_tiff || is_grib {
        return Ok(Dataset::from_lazy(terrio::memory::open_raster_lazy_bytes(bytes, filename).map_err(js_err)?));
    }
    #[cfg(feature = "tiles")]
    if terrio::tiles::is_archive(filename) && !terrio::tiles::TileArchive::from_bytes(bytes.clone()).map_err(js_err)?.is_vector() {
        return Ok(Dataset::from_lazy(terrio::memory::open_raster_lazy_bytes(bytes, filename).map_err(js_err)?));
    }
    Ok(match terrio::memory::open_bytes(bytes, filename).map_err(js_err)? {
        terrio::Dataset::Raster(r) => Dataset::from_raster(r),
        terrio::Dataset::Vector(v) => Dataset::from_layers(v.layers, v.driver),
    })
}

/// Opens a Zarr store (v2 or v3, GeoZarr conventions included) from its files: a `Map` or an
/// object of paths to `Uint8Array`s (`.zgroup`, `dem/.zarray`, `dem/0.0`...), as an in-memory
/// key-value store holds them. A zip archive of a store opens with `open`.
#[wasm_bindgen(js_name = openZarr)]
pub fn open_zarr(store: JsValue) -> Result<Dataset, JsError> {
    let entries = if store.is_instance_of::<js_sys::Map>() {
        js_sys::Array::from(&js_sys::Map::from(store).entries().into())
    } else if store.is_object() {
        js_sys::Object::entries(&js_sys::Object::from(store))
    } else {
        return Err(JsError::new("expected a Map or an object of paths to Uint8Arrays"));
    };
    let mut files = Vec::with_capacity(entries.length() as usize);
    for e in entries.iter() {
        let pair = js_sys::Array::from(&e);
        let key = pair.get(0).as_string().ok_or_else(|| JsError::new("store keys are strings"))?;
        let value = pair.get(1);
        let bytes = match value.dyn_ref::<js_sys::Uint8Array>() {
            Some(a) => a.to_vec(),
            None => match value.as_string() {
                Some(text) => text.into_bytes(),
                None => return Err(JsError::new(&format!("'{key}': expected a Uint8Array or a string"))),
            },
        };
        files.push((key, bytes));
    }
    Ok(Dataset::from_raster(terrio::memory::open_zarr_store(files).map_err(js_err)?))
}

/// Creates a raster from a typed array of pixels, band after band, row after row.
///
/// `transform` is the GDAL-style geotransform `[x_origin, pixel_width, 0, y_origin, 0,
/// -pixel_height]`; `crs` is `EPSG:4326`, WKT or a PROJ string.
#[wasm_bindgen(js_name = rasterFromArray)]
pub fn raster_from_array(
    data: JsValue,
    width: usize,
    height: usize,
    transform: Vec<f64>,
    crs: Option<String>,
    nodata: Option<f64>,
) -> Result<Dataset, JsError> {
    let data = raster_data(&data)?;
    let per_band = width * height;
    if per_band == 0 || data.len() % per_band != 0 {
        return Err(JsError::new("the array length is not a multiple of width * height"));
    }
    let g = transform;
    if g.len() != 6 || g[2] != 0.0 || g[4] != 0.0 || g[1] <= 0.0 || g[5] >= 0.0 {
        return Err(JsError::new("transform must be a north-up GDAL geotransform of 6 numbers"));
    }
    let crs = crs.as_deref().map(SpatialRef::parse).transpose().map_err(js_err)?.unwrap_or_default();
    let cfg = terrio::RasterConfig {
        cols: width,
        rows: height,
        bands: data.len() / per_band,
        nodata: nodata.unwrap_or(-9999.0),
        data_type: data.data_type(),
        crs: crs.to_crs_info(),
        ..Default::default()
    };
    let mut raster = terrio::Raster::from_data_native(cfg, data).map_err(|e| js_err(e.into()))?;
    raster.x_min = g[0];
    raster.cell_size_x = g[1];
    raster.cell_size_y = -g[5];
    raster.y_min = g[3] + height as f64 * g[5];
    Ok(Dataset::from_raster(RasterDataset::new(raster, nodata.is_some())))
}

/// Creates a vector dataset from GeoJSON: an object or a JSON string. `crs` is the coordinates'
/// CRS, EPSG:4326 by default.
#[wasm_bindgen(js_name = fromGeoJSON)]
pub fn from_geojson(geojson: JsValue, name: Option<String>, crs: Option<String>) -> Result<Dataset, JsError> {
    let text = match geojson.as_string() {
        Some(s) => s,
        None => js_sys::JSON::stringify(&geojson).map_err(|_| JsError::new("cannot serialize the GeoJSON"))?.into(),
    };
    let mut layer = terrio::memory::parse_geojson_layer(text.as_bytes(), "layer").map_err(js_err)?;
    layer.name = name.unwrap_or_else(|| "layer".into());
    let crs = SpatialRef::parse(crs.as_deref().unwrap_or("EPSG:4326")).map_err(js_err)?;
    crs.apply_to_layer(&mut layer);
    Ok(Dataset::from_layers(vec![layer], VectorDriver::GeoJSON))
}

/// Creates a vector dataset from an Arrow IPC stream or file (as Apache Arrow JS's
/// `tableToIPC` writes it): the geometry column is found by its GeoArrow metadata (WKB, WKT or
/// native encodings), GeoParquet metadata, or a binary column named `geometry`.
#[wasm_bindgen(js_name = fromArrowIPC)]
pub fn from_arrow_ipc(bytes: Vec<u8>, name: Option<String>) -> Result<Dataset, JsError> {
    #[cfg(not(feature = "parquet"))]
    {
        let _ = (bytes, name);
        Err(js_err(not_built("parquet")))
    }
    #[cfg(feature = "parquet")]
    let layer = terrio::arrow::from_ipc(&bytes, name.as_deref().unwrap_or("layer")).map_err(js_err)?;
    #[cfg(feature = "parquet")]
    Ok(Dataset::from_layers(vec![layer], VectorDriver::GeoJSON))
}

#[wasm_bindgen]
impl Dataset {
    /// `"raster"` or `"vector"`.
    #[wasm_bindgen(getter)]
    pub fn kind(&self) -> String {
        match &*self.inner.borrow() {
            Inner::Raster(_) => "raster".into(),
            Inner::Vector(..) => "vector".into(),
        }
    }

    /// The handle operations find the dataset by.
    #[wasm_bindgen(getter, js_name = __handle, skip_typescript)]
    pub fn handle(&self) -> u32 {
        self.handle
    }

    fn raster(&self) -> Result<std::cell::RefMut<'_, LazyRaster>, JsError> {
        std::cell::RefMut::filter_map(self.inner.borrow_mut(), |i| match i {
            Inner::Raster(r) => Some(r),
            Inner::Vector(..) => None,
        })
        .map_err(|_| JsError::new("the dataset is a vector dataset, not a raster"))
    }

    fn layers(&self) -> Result<std::cell::Ref<'_, [Layer]>, JsError> {
        std::cell::Ref::filter_map(self.inner.borrow(), |i| match i {
            Inner::Vector(l, _) => Some(l.as_slice()),
            Inner::Raster(_) => None,
        })
        .map_err(|_| JsError::new("the dataset is a raster, not a vector dataset"))
    }

    /// The summary `terrio info --json` prints, as an object.
    pub fn info(&self, stats: Option<bool>) -> Result<JsValue, JsError> {
        let options = terrio::info::InfoOptions { statistics: stats.unwrap_or(false), histogram: false };
        match &mut *self.inner.borrow_mut() {
            Inner::Raster(r) => {
                let info = terrio::info::lazy_raster_info(r, &options).map_err(js_err)?;
                to_js(&terrio::info::Info::Raster(Box::new(info)))
            }
            Inner::Vector(layers, driver) => {
                let ds = terrio::Dataset::Vector(terrio::VectorDataset { layers: layers.clone(), driver: *driver });
                to_js(&terrio::info::info(&ds, &options))
            }
        }
    }

    /// The raster width in pixels.
    #[wasm_bindgen(getter)]
    pub fn width(&self) -> Result<usize, JsError> {
        Ok(self.raster()?.width())
    }

    /// The raster height in pixels.
    #[wasm_bindgen(getter)]
    pub fn height(&self) -> Result<usize, JsError> {
        Ok(self.raster()?.height())
    }

    /// The number of raster bands.
    #[wasm_bindgen(getter)]
    pub fn count(&self) -> Result<usize, JsError> {
        Ok(self.raster()?.band_count())
    }

    /// The pixel type: `uint8`, `int16`, `float32`...
    #[wasm_bindgen(getter)]
    pub fn dtype(&self) -> Result<String, JsError> {
        Ok(dtype_name(self.raster()?.data_type()).into())
    }

    /// The raster's nodata value, or `undefined`.
    #[wasm_bindgen(getter)]
    pub fn nodata(&self) -> Result<Option<f64>, JsError> {
        Ok(self.raster()?.nodata())
    }

    /// The CRS: `EPSG:xxxx` when its code is known, otherwise its WKT, or `undefined`. For vector
    /// datasets, the first layer's.
    #[wasm_bindgen(getter)]
    pub fn crs(&self) -> Option<String> {
        match &*self.inner.borrow() {
            Inner::Raster(r) => crs_string(&r.crs()),
            Inner::Vector(l, _) => l.first().and_then(|l| crs_string(&SpatialRef::from_layer(l))),
        }
    }

    /// The GDAL-style geotransform `[x_origin, pixel_width, 0, y_origin, 0, -pixel_height]`.
    #[wasm_bindgen(getter)]
    pub fn transform(&self) -> Result<Vec<f64>, JsError> {
        Ok(self.raster()?.geo_transform().to_vec())
    }

    /// The overview sizes `[[width, height], ...]`, largest first.
    #[wasm_bindgen(getter)]
    pub fn overviews(&self) -> Result<JsValue, JsError> {
        to_js(&self.raster()?.overviews())
    }

    /// Reads a band (numbered from 1) as a typed array of its own type (`Float32Array`,
    /// `Uint8Array`...), row after row. `window` is `[col_off, row_off, width, height]` (in the
    /// overview's pixels with `overview`, numbered from 0); by default, the whole image.
    #[wasm_bindgen(js_name = readBand)]
    pub fn read_band(&self, band: usize, window: Option<Vec<u32>>, overview: Option<usize>) -> Result<JsValue, JsError> {
        let mut r = self.raster()?;
        let window = match window.as_deref() {
            Some(&[x, y, w, h]) => Window::new(x as usize, y as usize, w as usize, h as usize),
            Some(_) => return Err(JsError::new("window is [col_off, row_off, width, height]")),
            None => {
                let (w, h) = match overview {
                    None => (r.width(), r.height()),
                    Some(i) => r.overviews().get(i).copied().ok_or_else(|| JsError::new(&format!("no overview {i}")))?,
                };
                Window::full(w, h)
            }
        };
        let ds = r.read_window(window, &WindowOptions { bands: vec![band], overview }).map_err(js_err)?;
        Ok(typed_array(ds.raster.data))
    }

    /// The vector layer names.
    #[wasm_bindgen(js_name = layerNames)]
    pub fn layer_names(&self) -> Result<Vec<String>, JsError> {
        Ok(self.layers()?.iter().map(|l| l.name.clone()).collect())
    }

    /// A vector layer: by name or position, the first by default.
    fn layer(&self, layer: &JsValue) -> Result<std::cell::Ref<'_, terrio::Layer>, JsError> {
        let layers = self.layers()?;
        let index = if let Some(name) = layer.as_string() {
            layers.iter().position(|l| l.name == name).ok_or_else(|| JsError::new(&format!("no layer named {name}")))?
        } else if let Some(i) = layer.as_f64() {
            Some(i as usize).filter(|&i| i < layers.len()).ok_or_else(|| JsError::new(&format!("no layer {i}")))?
        } else if layers.is_empty() {
            return Err(JsError::new("no layers"));
        } else {
            0
        };
        Ok(std::cell::Ref::map(layers, |l| &l[index]))
    }

    /// A vector layer (by name or position, the first by default) as a GeoJSON FeatureCollection
    /// object, in the layer's CRS.
    #[wasm_bindgen(js_name = toGeoJSON)]
    pub fn to_geojson(&self, layer: JsValue) -> Result<JsValue, JsError> {
        let l = self.layer(&layer)?;
        js_sys::JSON::parse(&wbvector::geojson::to_string(&l)).map_err(|_| JsError::new("invalid GeoJSON"))
    }

    /// A vector layer (by name or position, the first by default) as an Arrow IPC stream, with
    /// its geometries in a GeoArrow column (and the CRS as PROJJSON in its metadata): read it
    /// with Apache Arrow JS (`tableFromIPC`), DuckDB-Wasm or GeoArrow-aware libraries.
    ///
    /// Options: `geometryEncoding` (`"wkb"`, the default, or `"geoarrow"` for the native
    /// encodings) and `coordinates` (`"separated"` or `"interleaved"`).
    #[wasm_bindgen(js_name = toArrowIPC)]
    pub fn to_arrow_ipc(&self, layer: JsValue, options: JsValue) -> Result<Vec<u8>, JsError> {
        let l = self.layer(&layer)?;
        #[cfg(not(feature = "parquet"))]
        {
            let _ = (l, options);
            Err(js_err(not_built("parquet")))
        }
        #[cfg(feature = "parquet")]
        {
            let get = |key: &str| -> Option<String> {
                if options.is_undefined() || options.is_null() {
                    return None;
                }
                js_sys::Reflect::get(&options, &JsValue::from_str(key)).ok()?.as_string()
            };
            let mut o = terrio::arrow::ArrowWriteOptions::default();
            if let Some(e) = get("geometryEncoding") {
                o.geometry_encoding = terrio::arrow::GeometryEncoding::parse(&e).map_err(js_err)?;
            }
            if let Some(c) = get("coordinates") {
                o.coordinates = terrio::arrow::CoordinateLayout::parse(&c).map_err(js_err)?;
            }
            terrio::arrow::to_ipc_stream(&l, &o).map_err(js_err)
        }
    }

    /// Writes the dataset in a format, to bytes: the format of `filename`'s extension, or
    /// `options.format`. Options: `format`, `tSrs` (reproject), and for vector datasets `layers`,
    /// `where`, `select` and `limit`, for rasters `srcwin`, `overview`, `bands`, `compress`
    /// (GeoTIFF/COG: `NONE`, `LZW`, `DEFLATE`, `PACKBITS`, `ZSTD`, `LERC`, `LERC_DEFLATE`,
    /// `LERC_ZSTD`, `JXL`, `JPEG`), `tileSize` and `creationOptions` (GDAL's: `MAX_Z_ERROR`,
    /// `JPEG_QUALITY`, `PREDICTOR`, `FORMAT=NC4` for netCDF, `FORMAT=ZARR_V3` for Zarr...),
    /// `overviews` (GeoTIFF factors such as `[2,4,8]`) and `overviewResampling`. Zarr
    /// stores come back as a zip archive of their files. Shapefile output requires
    /// `roads.shp.zip` (MIME `application/zip`); `.shp` alone is refused. The ZIP holds
    /// `.shp`, `.shx`, `.dbf`, `.cpg` and, when a CRS is present, `.prj`.
    /// Text over 254 UTF-8 bytes and unsupported DBF values are explicit errors.
    #[wasm_bindgen(js_name = toBytes)]
    pub fn to_bytes(&self, filename: &str, options: JsValue) -> Result<Vec<u8>, JsError> {
        let o: WriteOptions = parse_options(&options)?;
        let transform_options = TransformOptions {
            allow_ballpark: o.allow_ballpark,
            only_best: o.only_best,
            promote_to_3d: o.promote_to_3d,
            ..Default::default()
        };
        let t_srs = o.t_srs.as_deref().map(SpatialRef::parse).transpose().map_err(js_err)?;
        match &mut *self.inner.borrow_mut() {
            Inner::Raster(r) => {
                if o.layers.is_some() || o.r#where.is_some() || o.select.is_some() || o.limit.is_some() {
                    return Err(JsError::new("layers, where, select and limit apply to vector datasets"));
                }
                let driver = o
                    .format
                    .as_deref()
                    .map(|f| RasterDriver::from_name(f).ok_or_else(|| JsError::new(&format!("unknown raster format '{f}'"))))
                    .transpose()?;
                let window = match (o.srcwin, o.overview) {
                    (Some([x, y, w, h]), _) => Window::new(x, y, w, h),
                    (None, Some(i)) => {
                        let (w, h) = r.overviews().get(i).copied().ok_or_else(|| JsError::new(&format!("no overview {i}")))?;
                        Window::full(w, h)
                    }
                    (None, None) => Window::full(r.width(), r.height()),
                };
                let opts = WindowOptions { bands: o.bands.unwrap_or_default(), overview: o.overview };
                let mut ds = r.read_window(window, &opts).map_err(js_err)?;
                if let Some(dst) = t_srs {
                    let o = terrio::ops::warp::WarpOptions {
                        dst_srs: Some(dst),
                        transform_options: transform_options.clone(),
                        ..Default::default()
                    };
                    ds = terrio::ops::warp::warp(&[&ds], &o).map_err(js_err)?;
                }
                let mut creation_options: Vec<(String, String)> = o
                    .creation_options
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(k, v)| (k, v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
                    .collect();
                creation_options.extend(o.minzoom.map(|z| ("MINZOOM".to_string(), z.to_string())));
                creation_options.extend(o.maxzoom.map(|z| ("MAXZOOM".to_string(), z.to_string())));
                let options = RasterWriteOptions {
                    driver,
                    compression: o.compress,
                    tile_size: o.tile_size,
                    creation_options,
                    overviews: o.overviews,
                    overview_resampling: o.overview_resampling,
                    ..Default::default()
                };
                terrio::memory::write_raster_bytes(&ds, filename, &options).map_err(js_err)
            }
            Inner::Vector(layers, _) => {
                let driver = o
                    .format
                    .as_deref()
                    .map(|f| VectorDriver::from_name(f).ok_or_else(|| JsError::new(&format!("unknown vector format '{f}'"))))
                    .transpose()?;
                if o.srcwin.is_some()
                    || o.overview.is_some()
                    || o.bands.is_some()
                    || o.compress.is_some()
                    || o.tile_size.is_some()
                    || o.creation_options.is_some()
                {
                    return Err(JsError::new("srcwin, overview, bands, compress, tileSize and creationOptions apply to rasters"));
                }
                let chosen: Vec<Layer> = match &o.layers {
                    Some(names) => layers.iter().filter(|l| names.contains(&l.name)).cloned().collect(),
                    None => layers.clone(),
                };
                let translate = VectorTranslateOptions {
                    where_clause: o.r#where,
                    select: o.select,
                    limit: o.limit,
                    dst_srs: t_srs,
                    transform_options,
                    ..Default::default()
                };
                let out = terrio::ops::vector_filter::translate(&chosen, &translate).map_err(js_err)?;
                let mut options = VectorWriteOptions { driver, ..Default::default() };
                options.tiles.min_zoom = o.minzoom;
                options.tiles.max_zoom = o.maxzoom;
                terrio::memory::write_vector_bytes(&out, filename, &options).map_err(js_err)
            }
        }
    }
}

/// Options for [`transform_points`], as a JavaScript object.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct TransformPointsOptions {
    allow_ballpark: Option<bool>,
    only_best: bool,
    /// Take the heights of 2D CRSs as ellipsoidal heights, as GDAL does after `PromoteTo3D`.
    promote_to_3d: bool,
    /// The coordinate epoch of the source coordinates (decimal year), for a dynamic source CRS.
    s_coord_epoch: Option<f64>,
    /// The coordinate epoch of the target coordinates, for a dynamic target CRS.
    t_coord_epoch: Option<f64>,
}

/// Rasters gathered into one netCDF-4 file, each in its group: `add` them, then `toBytes`.
///
/// ```js
/// const nc = new NetcdfGroups();
/// nc.add("/terrain/dem", dem);
/// nc.add("/terrain/slope", slope);
/// const bytes = nc.toBytes({ COMPRESS: "DEFLATE" }); // read back as NETCDF:"x.nc":/terrain/slope/Band1
/// ```
#[wasm_bindgen]
#[derive(Default)]
pub struct NetcdfGroups {
    rasters: Vec<(String, RasterDataset)>,
}

#[wasm_bindgen]
impl NetcdfGroups {
    /// An empty set of groups.
    #[wasm_bindgen(constructor)]
    pub fn new() -> NetcdfGroups {
        NetcdfGroups::default()
    }

    /// Adds a raster in a group: a path such as `/terrain/dem` (groups are created as needed), or
    /// `/` for the root group. The raster is read whole.
    pub fn add(&mut self, group: &str, dataset: &Dataset) -> Result<(), JsError> {
        let ds = dataset.raster()?.read().map_err(js_err)?;
        self.rasters.push((group.to_string(), ds));
        Ok(())
    }

    /// The number of rasters added.
    #[wasm_bindgen(getter)]
    pub fn length(&self) -> usize {
        self.rasters.len()
    }

    /// Encodes the rasters as one netCDF-4 file, with netCDF creation options as GDAL names them
    /// (`FORMAT`, `COMPRESS`, `ZLEVEL`, `CHUNKING`).
    #[wasm_bindgen(js_name = toBytes)]
    pub fn to_bytes(&self, creation_options: JsValue) -> Result<Vec<u8>, JsError> {
        let co: HashMap<String, serde_json::Value> = parse_options(&creation_options)?;
        let options = RasterWriteOptions {
            creation_options: co.into_iter().map(|(k, v)| (k, v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect(),
            ..Default::default()
        };
        let refs: Vec<(&str, &RasterDataset)> = self.rasters.iter().map(|(g, ds)| (g.as_str(), ds)).collect();
        terrio::memory::write_netcdf_groups_bytes(&refs, &options).map_err(js_err)
    }
}

/// Transforms coordinates between two CRSs (`EPSG:4326`, `EPSG:4326+5773`, WKT or PROJ
/// strings), x (longitude) first. Returns `[xs, ys]` as two `Float64Array`s, or `[xs, ys, zs]`
/// when `zs` are given. For geocentric CRSs these are Cartesian Z ordinates; use
/// all three arrays to retain XYZ (the 2D call uses input Z=0 and drops output Z).
/// Options: `sCoordEpoch` and `tCoordEpoch`, the coordinate epochs of time-dependent
/// transformations. Datum shift grids are used once registered with `registerGrid`.
#[wasm_bindgen(js_name = transformPoints)]
pub fn transform_points(
    xs: Vec<f64>,
    ys: Vec<f64>,
    src_crs: &str,
    dst_crs: &str,
    zs: Option<Vec<f64>>,
    options: JsValue,
) -> Result<js_sys::Array, JsError> {
    if xs.len() != ys.len() || zs.as_ref().is_some_and(|z| z.len() != xs.len()) {
        return Err(JsError::new("xs, ys and zs differ in length"));
    }
    let o: TransformPointsOptions = parse_options(&options)?;
    let src = SpatialRef::parse(src_crs).map_err(js_err)?;
    let dst = SpatialRef::parse(dst_crs).map_err(js_err)?;
    let opts = TransformOptions {
        source_coordinate_epoch: o.s_coord_epoch,
        target_coordinate_epoch: o.t_coord_epoch,
        allow_ballpark: o.allow_ballpark,
        only_best: o.only_best,
        promote_to_3d: o.promote_to_3d,
    };
    let t = Transformer::with_options(&src, &dst, &opts).map_err(js_err)?;
    let (mut ox, mut oy, mut oz) = (Vec::with_capacity(xs.len()), Vec::with_capacity(ys.len()), Vec::new());
    for (i, (x, y)) in xs.iter().zip(&ys).enumerate() {
        match &zs {
            Some(z) => {
                let (a, b, c) = t.transform_3d(*x, *y, z[i]).map_err(js_err)?;
                ox.push(a);
                oy.push(b);
                oz.push(c);
            }
            None => {
                let (a, b) = t.transform(*x, *y).map_err(js_err)?;
                ox.push(a);
                oy.push(b);
            }
        }
    }
    let out = js_sys::Array::new();
    out.push(&js_sys::Float64Array::from(&ox[..]));
    out.push(&js_sys::Float64Array::from(&oy[..]));
    if zs.is_some() {
        out.push(&js_sys::Float64Array::from(&oz[..]));
    }
    Ok(out)
}

/// Makes a datum shift or geoid grid (NTv2 `.gsb`, GTX, or a PROJ GeoTIFF grid from
/// https://cdn.proj.org such as `us_noaa_conus.tif`) available under its name, from its bytes,
/// for the transformations that use it.
#[wasm_bindgen(js_name = registerGrid)]
pub fn register_grid(name: &str, bytes: Vec<u8>) {
    terrio::crs::register_grid(name, bytes);
}

/// Describes a CRS: `{ epsg, esri, name, geographic, wkt }` (`esri`: the ESRI code of a CRS
/// without EPSG code).
#[wasm_bindgen(js_name = describeCrs)]
pub fn describe_crs(crs: &str) -> Result<JsValue, JsError> {
    let s = SpatialRef::parse(crs).map_err(js_err)?;
    to_js(&serde_json::json!({
        "epsg": s.epsg,
        "esri": s.esri,
        "authority": s.authority(),
        "name": s.name(),
        "geographic": s.is_geographic(),
        "geocentric": s.is_geocentric(),
        "wkt": s.to_wkt(),
    }))
}

/// The formats terrio reads and writes: `[{ name, long_name, kind, extensions, read, write }]`.
/// In WebAssembly, only the formats with in-memory codecs can be used (see the README).
#[wasm_bindgen]
pub fn drivers() -> Result<JsValue, JsError> {
    to_js(&terrio::formats::all_drivers())
}

/// The version of terrio.
#[wasm_bindgen]
pub fn version() -> String {
    terrio::VERSION.into()
}

/// What this module has: `{ build, features, threads }`, `build` being `"terrio-js"` (every
/// format), `"terrio-js/threads"` (every format, with `initThreadPool`), `"terrio-js/memory64"`
/// (every format, with 64-bit memory, up to 16 GB) or `"terrio-js/core"`,
/// `features` the optional formats built in (`jp2`, `netcdf-hdf5`, `pointcloud`, `parquet`...),
/// `threads` the threads work runs on (1 until `initThreadPool` has started more), and `memory`
/// the bytes of the module's memory now.
#[wasm_bindgen]
pub fn features() -> Result<JsValue, JsError> {
    let built: Vec<&str> = FEATURES.iter().filter(|(_, on)| *on).map(|(f, _)| *f).collect();
    #[cfg(feature = "threads")]
    let threads = threads::pool_size().max(1);
    #[cfg(not(feature = "threads"))]
    let threads = 1;
    let memory = js_sys::Reflect::get(&wasm_bindgen::memory(), &JsValue::from_str("buffer"))
        .and_then(|b| js_sys::Reflect::get(&b, &JsValue::from_str("byteLength")))
        .ok()
        .and_then(|n| n.as_f64())
        .unwrap_or(0.0);
    to_js(&serde_json::json!({ "build": BUILD, "features": built, "threads": threads, "memory": memory }))
}

/// Point clouds, which this build leaves out: the functions say so.
#[cfg(not(feature = "pointcloud"))]
mod no_pointcloud {
    use super::*;

    /// Point clouds are in the full module, `terrio-js`.
    #[wasm_bindgen(js_name = openPoints)]
    pub fn open_points(_bytes: Vec<u8>, _filename: &str) -> Result<JsValue, JsError> {
        Err(js_err(not_built("pointcloud")))
    }

    /// Point clouds are in the full module, `terrio-js`.
    #[wasm_bindgen(js_name = readPoints)]
    pub fn read_points(_bytes: Vec<u8>, _filename: &str, _options: JsValue) -> Result<JsValue, JsError> {
        Err(js_err(not_built("pointcloud")))
    }

    /// Point clouds are in the full module, `terrio-js`.
    #[wasm_bindgen(js_name = pointsFromArrays)]
    pub fn points_from_arrays(_arrays: &js_sys::Object, _crs: Option<String>) -> Result<JsValue, JsError> {
        Err(js_err(not_built("pointcloud")))
    }
}

/// The random bytes `getrandom` is asked for in the memory64 module, where it has no backend of
/// its own (it seeds `ahash`'s hash maps): `crypto.getRandomValues`, 64 KiB at a time.
#[cfg(target_arch = "wasm64")]
mod getrandom_custom {
    use wasm_bindgen::{JsCast, JsValue};

    /// # Safety
    /// `dest` is valid for writes of `len` bytes, as getrandom guarantees.
    #[unsafe(no_mangle)]
    unsafe extern "Rust" fn __getrandom_v03_custom(dest: *mut u8, len: usize) -> Result<(), getrandom::Error> {
        let crypto = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("crypto")).map_err(|_| getrandom::Error::UNSUPPORTED)?;
        let fill: js_sys::Function = js_sys::Reflect::get(&crypto, &JsValue::from_str("getRandomValues"))
            .ok()
            .and_then(|f| f.dyn_into().ok())
            .ok_or(getrandom::Error::UNSUPPORTED)?;
        let mut done = 0;
        while done < len {
            let n = (len - done).min(65536);
            let buf = js_sys::Uint8Array::new_with_length(n as u32);
            fill.call1(&crypto, &buf).map_err(|_| getrandom::Error::UNSUPPORTED)?;
            // SAFETY: `done + n <= len` bytes of `dest` are writable.
            buf.copy_to(unsafe { std::slice::from_raw_parts_mut(dest.add(done), n) });
            done += n;
        }
        Ok(())
    }
}

/// Take coordinate-operation and driver warnings recorded since the previous call.
#[wasm_bindgen(js_name = takeWarnings)]
pub fn take_warnings() -> js_sys::Array {
    terrio::warnings::take().into_iter().map(JsValue::from).collect()
}
