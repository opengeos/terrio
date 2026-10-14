//! WebAssembly bindings for terrio, for browsers and Node.js.
//!
//! Datasets are opened from bytes, with a file name telling the format, and written back to bytes:
//!
//! ```js
//! import { open, transformPoints } from "terrio";
//!
//! const ds = open(new Uint8Array(await (await fetch("dem.tif")).arrayBuffer()), "dem.tif");
//! ds.info();                 // the same summary as `terrio info --json`
//! const band = ds.readBand(1); // a Float32Array, Uint8Array... of the band's own type
//! const fgb = open(geojsonBytes, "roads.geojson").toBytes("roads.fgb");
//! ```

use serde::Deserialize;
use terrio::crs::{SpatialRef, Transformer};
use terrio::formats::{RasterDriver, VectorDriver};
use terrio::ops::vector_filter::VectorTranslateOptions;
use terrio::{DataType, Layer, LazyRaster, RasterDataset, RasterWriteOptions, VectorWriteOptions, Window, WindowOptions};
use wasm_bindgen::prelude::*;
use wbraster::raster::RasterData;

/// Reports Rust panics on the JavaScript console, with their message.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// Converts a terrio error into a JavaScript `Error`.
fn js_err(e: terrio::Error) -> JsError {
    JsError::new(&e.to_string())
}

/// A JSON value as a JavaScript value.
fn to_js<T: serde::Serialize>(value: &T) -> Result<JsValue, JsError> {
    let text = serde_json::to_string(value).map_err(|e| JsError::new(&e.to_string()))?;
    js_sys::JSON::parse(&text).map_err(|_| JsError::new("invalid JSON"))
}

/// A CRS as `EPSG:xxxx` when its code is known, otherwise its WKT.
fn crs_string(crs: &SpatialRef) -> Option<String> {
    match crs.epsg {
        Some(code) => Some(format!("EPSG:{code}")),
        None => crs.to_wkt(),
    }
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
}

enum Inner {
    Raster(LazyRaster),
    Vector(Vec<Layer>, VectorDriver),
}

/// A raster or vector dataset opened from bytes.
#[wasm_bindgen]
pub struct Dataset {
    inner: Inner,
}

/// Opens a dataset from its bytes. `filename`'s extension tells the format (`dem.tif`,
/// `roads.fgb`, `parcels.zip`); its stem names vector layers.
///
/// GeoTIFFs are decoded one window at a time, as they are read.
#[wasm_bindgen]
pub fn open(bytes: Vec<u8>, filename: &str) -> Result<Dataset, JsError> {
    let is_tiff = matches!(bytes.get(..4), Some(b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+"));
    if is_tiff {
        return Ok(Dataset { inner: Inner::Raster(terrio::memory::open_raster_lazy_bytes(bytes, filename).map_err(js_err)?) });
    }
    Ok(match terrio::memory::open_bytes(bytes, filename).map_err(js_err)? {
        terrio::Dataset::Raster(r) => Dataset { inner: Inner::Raster(LazyRaster::from(r)) },
        terrio::Dataset::Vector(v) => Dataset { inner: Inner::Vector(v.layers, v.driver) },
    })
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
    Ok(Dataset { inner: Inner::Raster(LazyRaster::from(RasterDataset::new(raster, nodata.is_some()))) })
}

/// Creates a vector dataset from GeoJSON: an object or a JSON string. `crs` is the coordinates'
/// CRS, EPSG:4326 by default.
#[wasm_bindgen(js_name = fromGeoJSON)]
pub fn from_geojson(geojson: JsValue, name: Option<String>, crs: Option<String>) -> Result<Dataset, JsError> {
    let text = match geojson.as_string() {
        Some(s) => s,
        None => js_sys::JSON::stringify(&geojson).map_err(|_| JsError::new("cannot serialize the GeoJSON"))?.into(),
    };
    let mut layer = wbvector::geojson::parse_str(&text).map_err(|e| js_err(e.into()))?;
    layer.name = name.unwrap_or_else(|| "layer".into());
    let crs = SpatialRef::parse(crs.as_deref().unwrap_or("EPSG:4326")).map_err(js_err)?;
    crs.apply_to_layer(&mut layer);
    Ok(Dataset { inner: Inner::Vector(vec![layer], VectorDriver::GeoJSON) })
}

#[wasm_bindgen]
impl Dataset {
    /// `"raster"` or `"vector"`.
    #[wasm_bindgen(getter)]
    pub fn kind(&self) -> String {
        match self.inner {
            Inner::Raster(_) => "raster".into(),
            Inner::Vector(..) => "vector".into(),
        }
    }

    fn raster(&self) -> Result<&LazyRaster, JsError> {
        match &self.inner {
            Inner::Raster(r) => Ok(r),
            Inner::Vector(..) => Err(JsError::new("the dataset is a vector dataset, not a raster")),
        }
    }

    fn raster_mut(&mut self) -> Result<&mut LazyRaster, JsError> {
        match &mut self.inner {
            Inner::Raster(r) => Ok(r),
            Inner::Vector(..) => Err(JsError::new("the dataset is a vector dataset, not a raster")),
        }
    }

    fn layers(&self) -> Result<&[Layer], JsError> {
        match &self.inner {
            Inner::Vector(l, _) => Ok(l),
            Inner::Raster(_) => Err(JsError::new("the dataset is a raster, not a vector dataset")),
        }
    }

    /// The summary `terrio info --json` prints, as an object.
    pub fn info(&mut self, stats: Option<bool>) -> Result<JsValue, JsError> {
        let options = terrio::info::InfoOptions { statistics: stats.unwrap_or(false) };
        match &mut self.inner {
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
        match &self.inner {
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
    pub fn read_band(&mut self, band: usize, window: Option<Vec<usize>>, overview: Option<usize>) -> Result<JsValue, JsError> {
        let r = self.raster_mut()?;
        let window = match window.as_deref() {
            Some(&[x, y, w, h]) => Window::new(x, y, w, h),
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

    /// A vector layer (by name or position, the first by default) as a GeoJSON FeatureCollection
    /// object, in the layer's CRS.
    #[wasm_bindgen(js_name = toGeoJSON)]
    pub fn to_geojson(&self, layer: JsValue) -> Result<JsValue, JsError> {
        let layers = self.layers()?;
        let l = if let Some(name) = layer.as_string() {
            layers.iter().find(|l| l.name == name).ok_or_else(|| JsError::new(&format!("no layer named {name}")))?
        } else if let Some(i) = layer.as_f64() {
            layers.get(i as usize).ok_or_else(|| JsError::new(&format!("no layer {i}")))?
        } else {
            layers.first().ok_or_else(|| JsError::new("no layers"))?
        };
        js_sys::JSON::parse(&wbvector::geojson::to_string(l)).map_err(|_| JsError::new("invalid GeoJSON"))
    }

    /// Writes the dataset in a format, to bytes: the format of `filename`'s extension, or
    /// `options.format`. Options: `format`, `tSrs` (reproject), and for vector datasets `layers`,
    /// `where`, `select` and `limit`, for rasters `srcwin`, `overview`, `bands`, `compress` and
    /// `tileSize`.
    #[wasm_bindgen(js_name = toBytes)]
    pub fn to_bytes(&mut self, filename: &str, options: JsValue) -> Result<Vec<u8>, JsError> {
        let o: WriteOptions = if options.is_undefined() || options.is_null() {
            WriteOptions::default()
        } else {
            let text: String = js_sys::JSON::stringify(&options).map_err(|_| JsError::new("invalid options"))?.into();
            serde_json::from_str(&text).map_err(|e| JsError::new(&format!("options: {e}")))?
        };
        let t_srs = o.t_srs.as_deref().map(SpatialRef::parse).transpose().map_err(js_err)?;
        match &mut self.inner {
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
                    let o = terrio::ops::warp::WarpOptions { dst_srs: Some(dst), ..Default::default() };
                    ds = terrio::ops::warp::warp(&[&ds], &o).map_err(js_err)?;
                }
                let options = RasterWriteOptions { driver, compression: o.compress, tile_size: o.tile_size, ..Default::default() };
                terrio::memory::write_raster_bytes(&ds, filename, &options).map_err(js_err)
            }
            Inner::Vector(layers, _) => {
                let driver = o
                    .format
                    .as_deref()
                    .map(|f| VectorDriver::from_name(f).ok_or_else(|| JsError::new(&format!("unknown vector format '{f}'"))))
                    .transpose()?;
                if o.srcwin.is_some() || o.overview.is_some() || o.bands.is_some() || o.compress.is_some() || o.tile_size.is_some() {
                    return Err(JsError::new("srcwin, overview, bands, compress and tileSize apply to rasters"));
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
                    ..Default::default()
                };
                let out = terrio::ops::vector_filter::translate(&chosen, &translate).map_err(js_err)?;
                terrio::memory::write_vector_bytes(&out, filename, &VectorWriteOptions { driver }).map_err(js_err)
            }
        }
    }
}

/// Transforms coordinates between two CRSs (`EPSG:4326`, WKT or PROJ strings), x (longitude)
/// first. Returns `[xs, ys]` as two `Float64Array`s.
#[wasm_bindgen(js_name = transformPoints)]
pub fn transform_points(xs: Vec<f64>, ys: Vec<f64>, src_crs: &str, dst_crs: &str) -> Result<js_sys::Array, JsError> {
    if xs.len() != ys.len() {
        return Err(JsError::new("xs and ys differ in length"));
    }
    let src = SpatialRef::parse(src_crs).map_err(js_err)?;
    let dst = SpatialRef::parse(dst_crs).map_err(js_err)?;
    let t = Transformer::new(&src, &dst).map_err(js_err)?;
    let (mut ox, mut oy) = (Vec::with_capacity(xs.len()), Vec::with_capacity(ys.len()));
    for (x, y) in xs.iter().zip(&ys) {
        let (a, b) = t.transform(*x, *y).map_err(js_err)?;
        ox.push(a);
        oy.push(b);
    }
    let out = js_sys::Array::new();
    out.push(&js_sys::Float64Array::from(&ox[..]));
    out.push(&js_sys::Float64Array::from(&oy[..]));
    Ok(out)
}

/// Describes a CRS: `{ epsg, name, geographic, wkt }`.
#[wasm_bindgen(js_name = describeCrs)]
pub fn describe_crs(crs: &str) -> Result<JsValue, JsError> {
    let s = SpatialRef::parse(crs).map_err(js_err)?;
    to_js(&serde_json::json!({
        "epsg": s.epsg,
        "name": s.name(),
        "geographic": s.is_geographic(),
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
